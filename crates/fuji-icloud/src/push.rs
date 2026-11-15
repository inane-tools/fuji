//! Apple's web push ("webcourier"): a WebSocket to `websocket.push.apple.com`
//! over which Apple delivers messages for the topics a client asks for. The
//! sign-in widget uses it to hear from the trusted device during approval;
//! icloud.com uses it for Skybridge. Transcribed from the widget's
//! `bridge-*.js`.
//!
//! Blocking: use from a background thread.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use serde_json::Value;
use sha1::{Digest as _, Sha1};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Production push host.
pub const PROD_HOST: &str = "websocket.push.apple.com";
/// How long a fresh push token lasts (the widget's value).
const TOKEN_SECONDS: u64 = 86_400;

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("couldn't connect to Apple's push service: {0}")]
    Connect(String),
    #[error("Apple's push service refused the connection (status {0})")]
    Refused(u64),
    #[error("Apple's push service closed the connection")]
    Closed,
    #[error("bad message from Apple's push service")]
    Malformed,
}

/// A host for a push environment name ("prod", "sandbox", ...).
pub fn host_for(environment: &str) -> &'static str {
    match environment {
        "sandbox" | "qa1" => "websocket.sandbox.push.apple.com",
        "qa2" => "websocket.qa2.push.apple.com",
        "carry" => "websocket-carry.push.apple.com",
        "tc" => "websocket-tc.push.apple.com",
        _ => PROD_HOST,
    }
}

// ---- Protobuf, just enough ---------------------------------------------

pub(crate) mod pb {
    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub fn bytes_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
        varint(out, u64::from(field << 3 | 2));
        varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    pub fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
        varint(out, u64::from(field << 3));
        varint(out, v);
    }

    /// A field's value: a number or a byte run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Value<'a> {
        Int(u64),
        Bytes(&'a [u8]),
        Fixed64(u64),
    }

    fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *data.get(*pos)?;
            *pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }

    /// All (field number, value) pairs of a message, in order.
    pub fn fields(data: &[u8]) -> Option<Vec<(u32, Value<'_>)>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let key = read_varint(data, &mut pos)?;
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => out.push((field, Value::Int(read_varint(data, &mut pos)?))),
                2 => {
                    let len = read_varint(data, &mut pos)? as usize;
                    let end = pos.checked_add(len).filter(|&e| e <= data.len())?;
                    out.push((field, Value::Bytes(&data[pos..end])));
                    pos = end;
                }
                1 => {
                    let bytes = data.get(pos..pos + 8)?;
                    out.push((
                        field,
                        Value::Fixed64(u64::from_le_bytes(bytes.try_into().ok()?)),
                    ));
                    pos += 8;
                }
                5 => pos += 4,
                _ => return None,
            }
        }
        Some(out)
    }

    pub fn get<'a, 'b>(fields: &'b [(u32, Value<'a>)], field: u32) -> Option<&'b Value<'a>> {
        fields.iter().find(|(f, _)| *f == field).map(|(_, v)| v)
    }

    pub fn get_bytes<'a>(fields: &[(u32, Value<'a>)], field: u32) -> Option<&'a [u8]> {
        match get(fields, field)? {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn get_int(fields: &[(u32, Value<'_>)], field: u32) -> Option<u64> {
        match get(fields, field)? {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// A 64-bit fixed field read as a double.
    pub fn get_fixed64(fields: &[(u32, Value<'_>)], field: u32) -> Option<f64> {
        match get(fields, field)? {
            Value::Fixed64(v) => Some(f64::from_bits(*v)),
            _ => None,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a nonce is for: a connection, or another service (IDS) vouching
/// for this connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceKind {
    Connection,
    Ids,
}

/// A nonce signed with the connection's key, as the push service and IDS
/// expect it.
#[derive(Clone, Debug)]
pub struct SignedNonce {
    /// The connection's public key, raw (65 bytes).
    pub public_key: Vec<u8>,
    pub nonce: Vec<u8>,
    /// `01 03` + DER, r and s kept at 32 bytes.
    pub signature: Vec<u8>,
}

/// A nonce: for a connection `0x00`, the time in milliseconds (8 bytes,
/// big-endian) and 8 random bytes; for a service `0x01`, the time, the
/// service number (IDS 1), and 8 random bytes. Signed with ECDSA-SHA256.
fn sign_nonce(key: &SigningKey, timestamp_ms: u64, kind: NonceKind) -> SignedNonce {
    let public = key.verifying_key().as_affine().to_sec1_point(false);
    let mut nonce = Vec::with_capacity(18);
    match kind {
        NonceKind::Connection => {
            nonce.push(0);
            nonce.extend_from_slice(&timestamp_ms.to_be_bytes());
        }
        NonceKind::Ids => {
            nonce.push(1);
            nonce.extend_from_slice(&timestamp_ms.to_be_bytes());
            nonce.push(1);
        }
    }
    let mut random = [0u8; 8];
    rand::fill(&mut random);
    nonce.extend_from_slice(&random);

    let sig: Signature = key.sign(&nonce);
    let (r, s) = (sig.r().to_bytes(), sig.s().to_bytes());
    let part = |v: &[u8]| {
        let mut out = vec![0x02];
        let pad = v[0] > 127;
        out.push((v.len() + usize::from(pad)) as u8);
        if pad {
            out.push(0);
        }
        out.extend_from_slice(v);
        out
    };
    let (r, s) = (part(&r), part(&s));
    let mut signature = vec![1, 3, 0x30, (r.len() + s.len()) as u8];
    signature.extend_from_slice(&r);
    signature.extend_from_slice(&s);
    SignedNonce {
        public_key: public.as_bytes().to_vec(),
        nonce,
        signature,
    }
}

/// The connection's signed hello, asking for a new token.
fn connect_message(key: &SigningKey, timestamp_ms: u64) -> Vec<u8> {
    let signed = sign_nonce(key, timestamp_ms, NonceKind::Connection);
    let mut new_connection = Vec::new();
    pb::varint_field(&mut new_connection, 1, TOKEN_SECONDS);
    let mut v1 = Vec::new();
    pb::bytes_field(&mut v1, 1, &signed.public_key);
    pb::bytes_field(&mut v1, 2, &signed.nonce);
    pb::bytes_field(&mut v1, 3, &signed.signature);
    pb::bytes_field(&mut v1, 5, &new_connection);
    let mut message = Vec::new();
    pb::bytes_field(&mut message, 1, &v1);
    message
}

/// A message Apple pushed for one of our topics.
#[derive(Debug, Clone)]
pub struct Push {
    /// SHA-1 of the topic name.
    pub topic_hash: Vec<u8>,
    pub id: u64,
    /// The payload as JSON, if it is JSON.
    pub payload: Option<Value>,
    /// The payload as sent.
    pub raw: Vec<u8>,
}

pub struct PushConnection {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    /// The connection's key; it also signs nonces for IDS.
    key: SigningKey,
    /// Pushes that arrived while waiting for something else.
    pending: VecDeque<Push>,
    /// The token Apple gave this connection.
    pub token: Vec<u8>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn set_read_timeout(socket: &WebSocket<MaybeTlsStream<TcpStream>>, timeout: Option<Duration>) {
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => &s.sock,
        _ => return,
    };
    let _ = tcp.set_read_timeout(timeout);
}

impl PushConnection {
    /// Connect to `host` and subscribe to `topics`.
    pub fn connect(host: &str, topics: &[&str]) -> Result<Self, PushError> {
        let key = loop {
            let mut bytes = [0u8; 32];
            rand::fill(&mut bytes);
            if let Ok(key) = SigningKey::from_slice(&bytes) {
                break key;
            }
        };
        let mut timestamp = now_ms();
        // The widget tries three times; a wrong clock gets the server's time.
        for _ in 0..3 {
            let url = format!("wss://{host}/v2/{}", hex(&connect_message(&key, timestamp)));
            let mut request = url
                .into_client_request()
                .map_err(|e| PushError::Connect(e.to_string()))?;
            request.headers_mut().insert(
                "Origin",
                "https://idmsa.apple.com".parse().expect("valid header"),
            );
            let (mut socket, _) =
                tungstenite::connect(request).map_err(|e| PushError::Connect(e.to_string()))?;
            set_read_timeout(&socket, Some(Duration::from_secs(15)));
            let response = loop {
                match socket.read() {
                    Ok(Message::Binary(data)) => break data.to_vec(),
                    Ok(Message::Close(_)) => return Err(PushError::Closed),
                    Ok(_) => continue,
                    Err(e) => return Err(PushError::Connect(e.to_string())),
                }
            };
            let outer = pb::fields(&response).ok_or(PushError::Malformed)?;
            let reply = pb::get_bytes(&outer, 1).ok_or(PushError::Malformed)?;
            let reply = pb::fields(reply).ok_or(PushError::Malformed)?;
            match pb::get_int(&reply, 2).unwrap_or(0) {
                0 => {
                    let token_b64 = pb::get_bytes(&reply, 1).ok_or(PushError::Malformed)?;
                    let token = base64_any(token_b64).ok_or(PushError::Malformed)?;
                    let mut conn = Self {
                        socket,
                        key,
                        pending: VecDeque::new(),
                        token,
                    };
                    conn.filter(topics)?;
                    return Ok(conn);
                }
                // INVALID_NONCE: retry with the server's clock.
                2 => match pb::get_int(&reply, 3) {
                    Some(secs) => timestamp = secs * 1000,
                    None => return Err(PushError::Refused(2)),
                },
                status => return Err(PushError::Refused(status)),
            }
        }
        Err(PushError::Refused(2))
    }

    /// The token as the sign-in widget sends it (`ptkn`): hex.
    pub fn token_hex(&self) -> String {
        hex(&self.token)
    }

    fn send(&mut self, message: Vec<u8>) -> Result<(), PushError> {
        self.socket
            .send(Message::Binary(message.into()))
            .map_err(|e| PushError::Connect(e.to_string()))
    }

    fn filter(&mut self, topics: &[&str]) -> Result<(), PushError> {
        let mut filter = Vec::new();
        for topic in topics {
            pb::bytes_field(&mut filter, 1, topic.as_bytes());
        }
        let mut message = Vec::new();
        pb::bytes_field(&mut message, 3, &filter);
        self.send(message)
    }

    /// A nonce for IDS, signed with this connection's key.
    pub fn signed_nonce(&self, kind: NonceKind) -> SignedNonce {
        sign_nonce(&self.key, now_ms(), kind)
    }

    /// The next message from the server before `deadline`, if any.
    fn read_message(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, PushError> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            set_read_timeout(&self.socket, Some(left.min(Duration::from_secs(5))));
            match self.socket.read() {
                Ok(Message::Binary(data)) => return Ok(Some(data.to_vec())),
                Ok(Message::Close(_)) => return Err(PushError::Closed),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(PushError::Connect(e.to_string())),
            }
        }
    }

    /// If `data` is a push, acknowledge it and return it.
    fn take_push(&mut self, outer: &[(u32, pb::Value<'_>)]) -> Result<Option<Push>, PushError> {
        let Some(push) = pb::get_bytes(outer, 2) else {
            return Ok(None);
        };
        let push = pb::fields(push).ok_or(PushError::Malformed)?;
        let topic_hash = pb::get_bytes(&push, 1).unwrap_or_default().to_vec();
        let id = pb::get_int(&push, 2).unwrap_or(0);
        let raw = pb::get_bytes(&push, 4).unwrap_or_default().to_vec();
        let payload = serde_json::from_slice(&raw).ok();
        let mut ack = Vec::new();
        pb::bytes_field(&mut ack, 1, &topic_hash);
        pb::varint_field(&mut ack, 2, id);
        let mut message = Vec::new();
        pb::bytes_field(&mut message, 2, &ack);
        self.send(message)?;
        Ok(Some(Push {
            topic_hash,
            id,
            payload,
            raw,
        }))
    }

    /// Wait up to `timeout` for the next push, acknowledging it. `None` if
    /// none came in time.
    pub fn next_push(&mut self, timeout: Duration) -> Result<Option<Push>, PushError> {
        if let Some(push) = self.pending.pop_front() {
            return Ok(Some(push));
        }
        let deadline = Instant::now() + timeout;
        while let Some(data) = self.read_message(deadline)? {
            let outer = pb::fields(&data).ok_or(PushError::Malformed)?;
            if let Some(push) = self.take_push(&outer)? {
                return Ok(Some(push));
            }
        }
        Ok(None)
    }

    /// Push `payload` on `topic` to whoever the payload addresses (IDS
    /// routes it): a `reversePushMessage` with the topic name, a random id, a
    /// 100 s expiry and a random UUID.
    pub fn send_push(&mut self, topic: &str, payload: &[u8]) -> Result<(), PushError> {
        let mut uuid = [0u8; 16];
        rand::fill(&mut uuid);
        let expires = (now_ms() + 100_000) / 1000;
        let mut push = Vec::new();
        pb::bytes_field(&mut push, 1, topic.as_bytes());
        pb::varint_field(&mut push, 2, u64::from(rand::random::<u32>()));
        pb::varint_field(&mut push, 3, expires);
        pb::bytes_field(&mut push, 4, payload);
        pb::bytes_field(&mut push, 8, &uuid);
        let mut message = Vec::new();
        pb::bytes_field(&mut message, 6, &push);
        self.send(message)
    }

    /// A JWT from the push service vouching for this connection (IDS wants
    /// one to register). Lasts 10 minutes.
    pub fn jwt(&mut self) -> Result<String, PushError> {
        let mut request = Vec::new();
        pb::varint_field(&mut request, 1, 600);
        let mut message = Vec::new();
        pb::bytes_field(&mut message, 7, &request);
        self.send(message)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while let Some(data) = self.read_message(deadline)? {
            let outer = pb::fields(&data).ok_or(PushError::Malformed)?;
            if let Some(push) = self.take_push(&outer)? {
                self.pending.push_back(push);
                continue;
            }
            let Some(reply) = pb::get_bytes(&outer, 8) else {
                continue;
            };
            let reply = pb::fields(reply).ok_or(PushError::Malformed)?;
            let status = pb::get_int(&reply, 2).unwrap_or(0);
            return match pb::get_bytes(&reply, 1) {
                Some(token) if status == 1 => {
                    String::from_utf8(token.to_vec()).map_err(|_| PushError::Malformed)
                }
                _ => Err(PushError::Refused(status)),
            };
        }
        Err(PushError::Closed)
    }

    pub fn close(mut self) {
        let _ = self.socket.close(None);
    }
}

/// SHA-1 of a topic name, as pushes name their topic.
pub fn topic_hash(topic: &str) -> Vec<u8> {
    Sha1::digest(topic.as_bytes()).to_vec()
}

fn base64_any(text: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let text = std::str::from_utf8(text).ok()?.trim().trim_end_matches('=');
    let normal = text.replace('-', "+").replace('_', "/");
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normal)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier as _;

    #[test]
    fn protobuf_round_trip() {
        let mut m = Vec::new();
        pb::varint_field(&mut m, 2, 300);
        pb::bytes_field(&mut m, 4, b"hello");
        let f = pb::fields(&m).unwrap();
        assert_eq!(pb::get_int(&f, 2), Some(300));
        assert_eq!(pb::get_bytes(&f, 4), Some(&b"hello"[..]));
        assert_eq!(pb::fields(&[0x0a, 0x05, 1]), None);
    }

    #[test]
    fn connect_message_is_signed() {
        let key = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let msg = connect_message(&key, 0x0102_0304_0506_0708);
        let outer = pb::fields(&msg).unwrap();
        let v1 = pb::fields(pb::get_bytes(&outer, 1).unwrap()).unwrap();
        let public = pb::get_bytes(&v1, 1).unwrap();
        let nonce = pb::get_bytes(&v1, 2).unwrap();
        let signature = pb::get_bytes(&v1, 3).unwrap();
        assert_eq!(public.len(), 65);
        assert_eq!(nonce.len(), 17);
        assert_eq!(&nonce[..9], &[0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&signature[..3], &[1, 3, 0x30]);
        // The DER after the prefix verifies against the public key.
        let sig = Signature::from_der(&signature[2..]).unwrap();
        let vk = VerifyingKey::from_sec1_bytes(public).unwrap();
        assert!(vk.verify(nonce, &sig).is_ok());
        let req = pb::fields(pb::get_bytes(&v1, 5).unwrap()).unwrap();
        assert_eq!(pb::get_int(&req, 1), Some(TOKEN_SECONDS));
    }

    #[test]
    fn ids_nonce_is_typed() {
        let key = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let signed = sign_nonce(&key, 5, NonceKind::Ids);
        assert_eq!(signed.nonce.len(), 18);
        assert_eq!(signed.nonce[0], 1);
        assert_eq!(&signed.nonce[1..9], &5u64.to_be_bytes());
        assert_eq!(signed.nonce[9], 1);
        let sig = Signature::from_der(&signed.signature[2..]).unwrap();
        let vk = VerifyingKey::from_sec1_bytes(&signed.public_key).unwrap();
        assert!(vk.verify(&signed.nonce, &sig).is_ok());
    }

    #[test]
    fn topic_hash_is_sha1() {
        assert_eq!(topic_hash("abc").len(), 20);
        assert_eq!(hex(&topic_hash("abc"))[..8], *"a9993e36");
    }

    /// Connects to Apple's real push service and gets a token (nothing is
    /// sent anywhere else). Run with `cargo test -p fuji-icloud
    /// live_push_connects -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_push_connects() {
        let mut conn = PushConnection::connect(PROD_HOST, &["com.apple.idmsauthwidget"]).unwrap();
        println!("push token: {} bytes", conn.token.len());
        assert!(!conn.token.is_empty());
        let jwt = conn.jwt().unwrap();
        println!("jwt: {} chars, {} parts", jwt.len(), jwt.split('.').count());
        conn.close();
    }
}
