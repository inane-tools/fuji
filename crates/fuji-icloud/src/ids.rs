//! Apple's Identity Service (IDS), the way icloud.com uses it for Skybridge:
//! fuji registers as a "browser" device of the account, so it can find the
//! user's devices and exchange end-to-end encrypted messages with them.
//! Transcribed from icloud.com's system bundle.
//!
//! Requests are binary property lists sent to `gatewayws.icloud.com` with the
//! iCloud session cookies. Blocking: use from a background thread.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use plist::{Dictionary, Value};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::traits::PublicKeyParts;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthClient, AuthError};
use crate::push::{NonceKind, PushConnection};

/// The service icloud.com registers under, and Skybridge's topic within it.
pub const MULTIPLEX_SERVICE: &str = "com.apple.private.alloy.multiplex1";
pub const ICLOUD_WEB_TOPIC: &str = "com.apple.private.alloy.icloudweb";

const GET_HANDLES_URL: &str =
    "https://gatewayws.icloud.com/ids-profile/WebObjects/VCProfileService.woa/wa/icloudGetHandles";
const REGISTER_URL: &str =
    "https://gatewayws.icloud.com/ids-identity/WebObjects/TDIdentityService.woa/wa/icloudRegister";
pub(crate) const QUERY_URL: &str =
    "https://gatewayws.icloud.com/ids-query/WebObjects/QueryService.woa/wa/icloudQuery";

/// `(p - 1) / 2` for P-256: keys whose y is at most this have the "compact"
/// form icloud.com wants (x alone determines them).
const HALF_P: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

fn ids_error(message: impl Into<String>) -> AuthError {
    AuthError::Bridge(message.into())
}

/// A P-256 key whose public point has the smaller y, so its x alone (the
/// "compact" form) identifies it.
fn compact_key() -> SigningKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let Ok(key) = SigningKey::from_slice(&bytes) else {
            continue;
        };
        let point = key.verifying_key().as_affine().to_sec1_point(false);
        if point.as_bytes()[33..65] <= HALF_P[..] {
            return key;
        }
    }
}

/// The compact form of a key: the x coordinate.
fn compact(key: &SigningKey) -> Vec<u8> {
    key.verifying_key()
        .as_affine()
        .to_sec1_point(false)
        .as_bytes()[1..33]
        .to_vec()
}

fn uncompressed(key: &SigningKey) -> Vec<u8> {
    key.verifying_key()
        .as_affine()
        .to_sec1_point(false)
        .as_bytes()
        .to_vec()
}

// Protobuf, as in `push.rs` but with the double this needs.
fn pb_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    pb_varint(out, u64::from(field << 3 | 2));
    pb_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn pb_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn pb_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    pb_varint(out, u64::from(field << 3));
    pb_varint(out, v);
}

fn pb_double(out: &mut Vec<u8>, field: u32, v: f64) {
    pb_varint(out, u64::from(field << 3 | 1));
    out.extend_from_slice(&v.to_le_bytes());
}

/// DER length.
fn der_len(out: &mut Vec<u8>, len: usize) {
    match len {
        0..=127 => out.push(len as u8),
        128..=255 => out.extend_from_slice(&[0x81, len as u8]),
        _ => out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]),
    }
}

fn der(out: &mut Vec<u8>, tag: u8, data: &[u8]) {
    out.push(tag);
    der_len(out, data.len());
    out.extend_from_slice(data);
}

/// A tagged value whose content starts with its own 2-byte length.
fn length_validated(out: &mut Vec<u8>, tag: u8, data: &[u8]) {
    out.push(tag);
    der_len(out, data.len() + 2);
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// A DER integer: big-endian, with a leading zero if the top bit is set.
fn der_integer(out: &mut Vec<u8>, bytes: &[u8]) {
    let start = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len().saturating_sub(1));
    let bytes = &bytes[start..];
    let mut v = Vec::with_capacity(bytes.len() + 1);
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        v.push(0);
    }
    v.extend_from_slice(bytes);
    der(out, 2, &v);
}

/// fuji's identity as an IDS device: kept between runs (it holds secrets).
pub struct IdsIdentity {
    /// Signs for "next generation messaging" (NGM).
    signing: SigningKey,
    /// Others encrypt to this (ECDH), signed by `signing`.
    prekey: SigningKey,
    prekey_timestamp: f64,
    /// The older identity: an ECDSA key and an RSA-OAEP key.
    legacy_signing: SigningKey,
    legacy_rsa: rsa::PrivateKey,
}

#[derive(Serialize, Deserialize)]
struct SavedIdentity {
    signing: String,
    prekey: String,
    prekey_timestamp: f64,
    legacy_signing: String,
    legacy_rsa: String,
}

impl IdsIdentity {
    /// New keys. The RSA key takes a moment.
    pub fn generate() -> Result<Self, AuthError> {
        let legacy_rsa = rsa::PrivateKey::new(&mut rand::rng(), 2048)
            .map_err(|e| ids_error(format!("couldn't make a key: {e}")))?;
        Ok(Self {
            signing: compact_key(),
            prekey: compact_key(),
            prekey_timestamp: now_secs_f64(),
            legacy_signing: compact_key(),
            legacy_rsa,
        })
    }

    /// For saving (JSON; contains private keys).
    pub fn to_json(&self) -> String {
        let saved = SavedIdentity {
            signing: BASE64.encode(self.signing.to_bytes()),
            prekey: BASE64.encode(self.prekey.to_bytes()),
            prekey_timestamp: self.prekey_timestamp,
            legacy_signing: BASE64.encode(self.legacy_signing.to_bytes()),
            legacy_rsa: BASE64.encode(
                self.legacy_rsa
                    .to_pkcs8_der()
                    .map(|d| d.as_bytes().to_vec())
                    .unwrap_or_default(),
            ),
        };
        serde_json::to_string(&saved).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Option<Self> {
        let saved: SavedIdentity = serde_json::from_str(json).ok()?;
        let key = |b64: &str| SigningKey::from_slice(&BASE64.decode(b64).ok()?).ok();
        Some(Self {
            signing: key(&saved.signing)?,
            prekey: key(&saved.prekey)?,
            prekey_timestamp: saved.prekey_timestamp,
            legacy_signing: key(&saved.legacy_signing)?,
            legacy_rsa: rsa::PrivateKey::from_pkcs8_der(&BASE64.decode(saved.legacy_rsa).ok()?)
                .ok()?,
        })
    }

    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing
    }

    pub(crate) fn prekey(&self) -> &SigningKey {
        &self.prekey
    }

    /// `ngm-public-identity` (also the key transparency "loggable data"):
    /// `{ 1: PublicDeviceIdentity { 1: compact signing key }, 2: 12, 3: 0 }`.
    pub fn kt_loggable_data(&self) -> Vec<u8> {
        let mut identity = Vec::new();
        pb_bytes(&mut identity, 1, &compact(&self.signing));
        let mut out = Vec::new();
        pb_bytes(&mut out, 1, &identity);
        pb_uint(&mut out, 2, 12);
        pb_uint(&mut out, 3, 0);
        out
    }

    /// `public-message-identity-key`: the legacy identity, DER-like.
    fn identity_key(&self) -> Vec<u8> {
        let public = self.legacy_rsa.to_public_key();
        let mut rsa = Vec::new();
        der_integer(&mut rsa, &public.n().to_be_bytes());
        der_integer(&mut rsa, &public.e().to_be_bytes());
        let mut rsa_seq = Vec::new();
        der(&mut rsa_seq, 0x30, &rsa);
        let mut inner = Vec::new();
        length_validated(&mut inner, 0x81, &uncompressed(&self.legacy_signing));
        length_validated(&mut inner, 0x82, &rsa_seq);
        let mut out = Vec::new();
        der(&mut out, 0x30, &inner);
        out
    }

    /// `public-message-ngm-device-prekey-data-key`: the pre-key, signed.
    fn prekey_data(&self) -> Vec<u8> {
        let prekey = compact(&self.prekey);
        let mut signed = b"NGMPrekeySignature".to_vec();
        signed.extend_from_slice(&prekey);
        signed.extend_from_slice(&self.prekey_timestamp.to_le_bytes());
        let signature: Signature = self.signing.sign(&signed);
        let mut out = Vec::new();
        pb_bytes(&mut out, 1, &prekey);
        pb_bytes(&mut out, 2, &signature.to_bytes());
        pb_double(&mut out, 3, self.prekey_timestamp);
        out
    }
}

fn now_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// What registering gave: fuji's address and certificate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdsRegistration {
    /// The handle registered first (e.g. "mailto:...").
    pub uri: String,
    pub handles: Vec<String>,
    /// Apple's certificate for this registration (base64).
    pub cert: String,
}

fn string(v: &str) -> Value {
    Value::String(v.to_string())
}

fn b64(v: &[u8]) -> Value {
    Value::String(BASE64.encode(v))
}

fn status(dict: &Dictionary) -> Option<i64> {
    match dict.get("status")? {
        Value::Integer(i) => i.as_signed(),
        _ => None,
    }
}

impl AuthClient {
    /// POST a binary plist to IDS with the iCloud cookies.
    pub(crate) fn ids_post(&self, url: &str, body: Dictionary) -> Result<Dictionary, AuthError> {
        let mut bytes = Vec::new();
        Value::Dictionary(body)
            .to_writer_binary(&mut bytes)
            .map_err(|e| ids_error(e.to_string()))?;
        let mut resp = self
            .agent()
            .post(url)
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .header("Accept", "application/x-bplist")
            .content_type("application/x-bplist")
            .send(&bytes[..])?;
        self.cookie_log().record(&resp);
        let code = resp.status().as_u16();
        let data = resp.body_mut().read_to_vec()?;
        if std::env::var_os("FUJI_DEBUG").is_some() {
            eprintln!(
                "ids: {} -> {code}, {} bytes",
                url.rsplit('/').next().unwrap_or(url),
                data.len()
            );
        }
        if code == 401 || code == 421 {
            return Err(AuthError::SessionExpired);
        }
        if !(200..300).contains(&code) {
            return Err(AuthError::Unexpected {
                status: code,
                message: String::from_utf8_lossy(&data).chars().take(200).collect(),
            });
        }
        let value: Value = bplist::parse(&data)
            .ok_or_else(|| "not a readable binary plist".to_string())
            .or_else(|plist_err| {
                serde_json::from_slice::<serde_json::Value>(&data)
                    .map(json_to_plist)
                    .map_err(|_| plist_err)
            })
            .map_err(|e| ids_error(format!("IDS sent an unreadable answer ({e})")))?;
        value
            .into_dictionary()
            .ok_or_else(|| ids_error("IDS sent an unexpected answer"))
    }

    /// The account's handles (email addresses, phone numbers) for `service`.
    pub fn ids_handles(
        &self,
        push: &PushConnection,
        service: &str,
    ) -> Result<Vec<String>, AuthError> {
        let mut body = Dictionary::new();
        body.insert("protocol-version".into(), string("1"));
        body.insert("push-token".into(), b64(&push.token));
        body.insert("service".into(), string(service));
        let reply = self.ids_post(GET_HANDLES_URL, body)?;
        if status(&reply) != Some(0) {
            return Err(ids_error(format!(
                "IDS getHandles status {:?}",
                status(&reply)
            )));
        }
        let handles = reply
            .get("handles")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|h| {
                        h.as_dictionary()?
                            .get("uri")?
                            .as_string()
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Email addresses first, as icloud.com orders them.
        let (mut emails, phones): (Vec<_>, Vec<_>) =
            handles.into_iter().partition(|h| h.starts_with("mailto:"));
        emails.extend(phones);
        Ok(emails)
    }

    /// Register fuji with IDS for Skybridge's topic, over `push`.
    pub fn ids_register(
        &self,
        push: &mut PushConnection,
        identity: &IdsIdentity,
        dsid: &str,
    ) -> Result<IdsRegistration, AuthError> {
        let handles = self.ids_handles(push, ICLOUD_WEB_TOPIC)?;
        if handles.is_empty() {
            return Err(ids_error("This account has no IDS handles."));
        }
        let nonce = push.signed_nonce(NonceKind::Ids);
        let jwt = push.jwt().map_err(|e| ids_error(e.to_string()))?;

        let mut client_data = Dictionary::new();
        client_data.insert("hardware-version".into(), string("browser"));
        client_data.insert("supports-gft-errors".into(), Value::Boolean(true));
        client_data.insert("supports-modern-gft".into(), Value::Boolean(true));
        client_data.insert("supports-skybridge".into(), Value::Boolean(true));
        client_data.insert(
            "public-message-ngm-device-prekey-data-key".into(),
            b64(&identity.prekey_data()),
        );
        client_data.insert(
            "public-message-identity-key".into(),
            b64(&identity.identity_key()),
        );
        client_data.insert(
            "public-message-identity-version".into(),
            Value::Integer(2.into()),
        );
        client_data.insert(
            "public-message-identity-ngm-version".into(),
            Value::Integer(12.into()),
        );
        client_data.insert(
            "ngm-public-identity".into(),
            b64(&identity.kt_loggable_data()),
        );

        let mut service = Dictionary::new();
        service.insert("service".into(), string(MULTIPLEX_SERVICE));
        service.insert(
            "sub-services".into(),
            Value::Array(vec![string(ICLOUD_WEB_TOPIC)]),
        );
        service.insert("client-data".into(), Value::Dictionary(client_data));
        service.insert(
            "uris".into(),
            Value::Array(handles.iter().map(|h| string(h)).collect()),
        );

        let mut web_auth = Dictionary::new();
        web_auth.insert("nonce".into(), b64(&nonce.nonce));
        web_auth.insert("public-key".into(), b64(&nonce.public_key));
        web_auth.insert("signature".into(), b64(&nonce.signature));
        web_auth.insert("jwt".into(), b64(jwt.as_bytes()));

        let mut body = Dictionary::new();
        body.insert("protocol-version".into(), string("1"));
        body.insert("push-token".into(), b64(&push.token));
        body.insert("user-id".into(), string(&format!("D:{dsid}")));
        body.insert(
            "services".into(),
            Value::Array(vec![Value::Dictionary(service)]),
        );
        body.insert("web-token-auth".into(), Value::Dictionary(web_auth));
        body.insert("hwv".into(), string("Browser"));
        body.insert("language".into(), string("en"));

        let reply = self.ids_post(REGISTER_URL, body)?;
        if status(&reply) != Some(0) {
            return Err(ids_error(format!(
                "IDS register status {:?}{}",
                status(&reply),
                reply
                    .get("message")
                    .and_then(Value::as_string)
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            )));
        }
        let cert = reply
            .get("services")
            .and_then(Value::as_array)
            .and_then(|s| s.first())
            .and_then(Value::as_dictionary)
            .and_then(|s| s.get("cert"))
            .map(|c| match c {
                Value::Data(d) => BASE64.encode(d),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ids_error("IDS registered without a certificate"))?;
        Ok(IdsRegistration {
            uri: handles[0].clone(),
            handles,
            cert,
        })
    }
}

/// One of the account's endpoints for a topic: a device or a browser.
#[derive(Clone, Debug)]
pub struct IdsEndpoint {
    pub push_token: Vec<u8>,
    pub session_token: Vec<u8>,
    /// The device's `ngm-public-identity` (key transparency data).
    pub kt_loggable_data: Vec<u8>,
    /// The device's signed pre-key (`PublicDevicePrekey`).
    pub prekey_data: Vec<u8>,
    /// The device's legacy identity (DER-like), if it has one.
    pub legacy_identity: Option<Vec<u8>>,
    /// A real device (not another browser).
    pub is_device: bool,
    /// Everything the device published, for what isn't decoded yet.
    pub client_data: Dictionary,
}

/// Bytes from a plist value: data, or base64 text.
fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Data(d) => Some(d.clone()),
        Value::String(s) => BASE64.decode(s.trim()).ok(),
        _ => None,
    }
}

impl AuthClient {
    /// The account's endpoints for `topic` under `uri` (e.g. "mailto:..."),
    /// except this one (`push`).
    pub fn ids_query(
        &self,
        push: &PushConnection,
        registration: &IdsRegistration,
        uri: &str,
        topic: &str,
    ) -> Result<Vec<IdsEndpoint>, AuthError> {
        let mut body = Dictionary::new();
        body.insert("protocol-version".into(), string("1"));
        body.insert("push-token".into(), b64(&push.token));
        body.insert("self-uri".into(), string(&registration.uri));
        body.insert("topic".into(), string(topic));
        body.insert("uris".into(), Value::Array(vec![string(uri)]));
        body.insert("cert".into(), string(&registration.cert));
        let reply = self.ids_post(QUERY_URL, body)?;
        if status(&reply) != Some(0) {
            return Err(ids_error(format!("IDS query status {:?}", status(&reply))));
        }
        let Some(result) = reply
            .get("results")
            .and_then(Value::as_dictionary)
            .and_then(|r| r.get(uri))
            .and_then(Value::as_dictionary)
        else {
            return Ok(Vec::new());
        };
        let identities = result
            .get("identities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(identities
            .iter()
            .filter_map(|identity| {
                let identity = identity.as_dictionary()?;
                let client_data = identity
                    .get("client-data")
                    .and_then(Value::as_dictionary)
                    .cloned()
                    .unwrap_or_default();
                let push_token = bytes_of(identity.get("push-token")?)?;
                if push_token == push.token {
                    return None;
                }
                Some(IdsEndpoint {
                    session_token: identity.get("session-token").and_then(bytes_of)?,
                    kt_loggable_data: identity
                        .get("kt-loggable-data")
                        .or_else(|| client_data.get("ngm-public-identity"))
                        .and_then(bytes_of)?,
                    prekey_data: client_data
                        .get("public-message-ngm-device-prekey-data-key")
                        .and_then(bytes_of)?,
                    legacy_identity: client_data
                        .get("public-message-identity-key")
                        .and_then(bytes_of),
                    is_device: client_data
                        .get("supports-skybridge")
                        .and_then(Value::as_boolean)
                        != Some(true),
                    push_token,
                    client_data,
                })
            })
            .collect())
    }
}

/// A binary plist reader that, unlike the `plist` crate, accepts the
/// 16-byte integers Apple's writer emits (kept as reals if they don't fit).
mod bplist {
    use plist::{Dictionary, Value};

    struct Reader<'a> {
        data: &'a [u8],
        offsets: Vec<usize>,
        ref_size: usize,
    }

    fn be(bytes: &[u8]) -> Option<u128> {
        if bytes.len() > 16 {
            return None;
        }
        Some(bytes.iter().fold(0u128, |acc, &b| acc << 8 | u128::from(b)))
    }

    impl Reader<'_> {
        fn length(&self, pos: &mut usize, low: u8) -> Option<usize> {
            if low != 0x0f {
                return Some(usize::from(low));
            }
            let marker = *self.data.get(*pos)?;
            if marker >> 4 != 1 {
                return None;
            }
            let size = 1usize << (marker & 0x0f);
            let v = be(self.data.get(*pos + 1..*pos + 1 + size)?)?;
            *pos += 1 + size;
            usize::try_from(v).ok()
        }

        fn reference(&self, pos: usize) -> Option<usize> {
            usize::try_from(be(self.data.get(pos..pos + self.ref_size)?)?).ok()
        }

        fn object(&self, index: usize, depth: usize) -> Option<Value> {
            if depth > 64 {
                return None;
            }
            let mut pos = *self.offsets.get(index)?;
            let marker = *self.data.get(pos)?;
            pos += 1;
            let (high, low) = (marker >> 4, marker & 0x0f);
            Some(match high {
                0x0 => match low {
                    0x8 => Value::Boolean(false),
                    0x9 => Value::Boolean(true),
                    _ => Value::String(String::new()),
                },
                0x1 => {
                    let size = 1usize << low;
                    let v = be(self.data.get(pos..pos + size)?)?;
                    // 8-byte ints are signed; 16-byte ones may not fit at all.
                    match size {
                        8 => Value::Integer((v as u64 as i64).into()),
                        16 => match u64::try_from(v) {
                            Ok(u) => Value::Integer(u.into()),
                            Err(_) => Value::Real(v as f64),
                        },
                        _ => Value::Integer((v as u64).into()),
                    }
                }
                0x2 => {
                    let size = 1usize << low;
                    let bytes = self.data.get(pos..pos + size)?;
                    Value::Real(match size {
                        4 => f64::from(f32::from_be_bytes(bytes.try_into().ok()?)),
                        8 => f64::from_be_bytes(bytes.try_into().ok()?),
                        _ => return None,
                    })
                }
                0x3 => Value::Real(f64::from_be_bytes(
                    self.data.get(pos..pos + 8)?.try_into().ok()?,
                )),
                0x4 => {
                    let len = self.length(&mut pos, low)?;
                    Value::Data(self.data.get(pos..pos + len)?.to_vec())
                }
                0x5 => {
                    let len = self.length(&mut pos, low)?;
                    Value::String(String::from_utf8_lossy(self.data.get(pos..pos + len)?).into())
                }
                0x6 => {
                    let len = self.length(&mut pos, low)?;
                    let units: Vec<u16> = self
                        .data
                        .get(pos..pos + 2 * len)?
                        .chunks(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    Value::String(String::from_utf16_lossy(&units))
                }
                0x8 => {
                    let size = usize::from(low) + 1;
                    Value::Integer((be(self.data.get(pos..pos + size)?)? as u64).into())
                }
                0xa | 0xc => {
                    let len = self.length(&mut pos, low)?;
                    let mut items = Vec::with_capacity(len);
                    for i in 0..len {
                        let r = self.reference(pos + i * self.ref_size)?;
                        items.push(self.object(r, depth + 1)?);
                    }
                    Value::Array(items)
                }
                0xd => {
                    let len = self.length(&mut pos, low)?;
                    let mut dict = Dictionary::new();
                    for i in 0..len {
                        let k = self.reference(pos + i * self.ref_size)?;
                        let v = self.reference(pos + (len + i) * self.ref_size)?;
                        let Value::String(key) = self.object(k, depth + 1)? else {
                            return None;
                        };
                        dict.insert(key, self.object(v, depth + 1)?);
                    }
                    Value::Dictionary(dict)
                }
                _ => return None,
            })
        }
    }

    pub fn parse(data: &[u8]) -> Option<Value> {
        if !data.starts_with(b"bplist00") || data.len() < 40 {
            return None;
        }
        let trailer = &data[data.len() - 32..];
        let offset_size = usize::from(trailer[6]);
        let ref_size = usize::from(trailer[7]);
        let count = usize::try_from(be(&trailer[8..16])?).ok()?;
        let top = usize::try_from(be(&trailer[16..24])?).ok()?;
        let table = usize::try_from(be(&trailer[24..32])?).ok()?;
        let offsets = (0..count)
            .map(|i| {
                let at = table + i * offset_size;
                usize::try_from(be(data.get(at..at + offset_size)?)?).ok()
            })
            .collect::<Option<Vec<_>>>()?;
        Reader {
            data,
            offsets,
            ref_size,
        }
        .object(top, 0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_what_plist_writes() {
            let mut d = Dictionary::new();
            d.insert("status".into(), Value::Integer(0.into()));
            d.insert("name".into(), Value::String("My iPhone".into()));
            d.insert("data".into(), Value::Data(vec![1, 2, 3]));
            d.insert(
                "list".into(),
                Value::Array(vec![Value::Boolean(true), Value::Real(1.5)]),
            );
            let mut bytes = Vec::new();
            Value::Dictionary(d.clone())
                .to_writer_binary(&mut bytes)
                .unwrap();
            assert_eq!(parse(&bytes), Some(Value::Dictionary(d)));
        }

        #[test]
        fn accepts_sixteen_byte_integers() {
            // { "n": <16-byte int 2^64 + 1> }
            let mut b = b"bplist00".to_vec();
            let dict_at = b.len();
            b.extend_from_slice(&[0xd1, 1, 2]);
            let key_at = b.len();
            b.extend_from_slice(&[0x51, 0x6e]);
            let int_at = b.len();
            b.push(0x14);
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
            let table = b.len();
            b.extend_from_slice(&[dict_at as u8, key_at as u8, int_at as u8]);
            let mut trailer = vec![0u8; 6];
            trailer.extend_from_slice(&[1, 1]);
            trailer.extend_from_slice(&3u64.to_be_bytes());
            trailer.extend_from_slice(&0u64.to_be_bytes());
            trailer.extend_from_slice(&(table as u64).to_be_bytes());
            b.extend_from_slice(&trailer);
            let v = parse(&b).unwrap();
            assert!(matches!(
                v.as_dictionary().unwrap().get("n"),
                Some(Value::Real(_))
            ));
        }
    }
}

/// Read a binary plist (see `bplist`).
pub(crate) fn parse_bplist(data: &[u8]) -> Option<Value> {
    bplist::parse(data)
}

fn json_to_plist(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::String(String::new()),
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(|i| Value::Integer(i.into()))
            .unwrap_or_else(|| Value::Real(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(a) => Value::Array(a.into_iter().map(json_to_plist).collect()),
        serde_json::Value::Object(o) => {
            Value::Dictionary(o.into_iter().map(|(k, v)| (k, json_to_plist(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier as _;

    #[test]
    fn compact_keys_have_the_smaller_y() {
        for _ in 0..8 {
            let key = compact_key();
            let point = key.verifying_key().as_affine().to_sec1_point(false);
            assert!(point.as_bytes()[33..65] <= HALF_P[..]);
            assert_eq!(compact(&key).len(), 32);
        }
    }

    #[test]
    fn identity_round_trips_and_encodes() {
        let id = IdsIdentity::generate().unwrap();
        let again = IdsIdentity::from_json(&id.to_json()).unwrap();
        assert_eq!(again.kt_loggable_data(), id.kt_loggable_data());
        assert_eq!(again.identity_key(), id.identity_key());

        // The pre-key signature verifies with the signing key.
        let data = id.prekey_data();
        assert_eq!(data[0], 0x0a);
        let prekey = &data[2..34];
        let sig = Signature::from_slice(&data[36..100]).unwrap();
        let mut signed = b"NGMPrekeySignature".to_vec();
        signed.extend_from_slice(prekey);
        signed.extend_from_slice(&id.prekey_timestamp.to_le_bytes());
        let vk = VerifyingKey::from(&id.signing);
        assert!(vk.verify(&signed, &sig).is_ok());

        // Legacy identity: SEQUENCE { 0x81 (2 + 2 + 65) ..., 0x82 ... }.
        let key = id.identity_key();
        assert_eq!(key[0], 0x30);
        let inner_start = if key[1] & 0x80 != 0 {
            2 + (key[1] & 0x7f) as usize
        } else {
            2
        };
        assert_eq!(key[inner_start], 0x81);
        assert_eq!(key[inner_start + 1], 67);
        assert_eq!(&key[inner_start + 2..inner_start + 4], &[0, 65]);
    }

    #[test]
    fn der_integers_are_minimal_and_positive() {
        let mut out = Vec::new();
        der_integer(&mut out, &[0, 0, 0x80, 1]);
        assert_eq!(out, vec![2, 3, 0, 0x80, 1]);
    }
}
