//! Skybridge: requests to the user's device (an iPhone) over IDS, end-to-end
//! encrypted, the way icloud.com asks the iPhone to search Photos.
//!
//! A request is JSON `{ msgID, body, token? }`, encrypted per device (NGM),
//! wrapped in IDS's "nice" envelope (a binary plist, `E: "pair-ec"`) and
//! pushed on the topic `com.apple.private.alloy.icloudweb`. Answers come back
//! the same way as `{ msgID, body, i?, t? }` (`i`/`t` number the parts of a
//! streamed answer), sometimes gzipped.
//!
//! Blocking: use from a background thread.

use std::io::Read as _;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use plist::{Dictionary, Value as Plist};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::auth::{AuthClient, AuthError};
use crate::ids::{ICLOUD_WEB_TOPIC, IdsEndpoint, IdsIdentity, IdsRegistration};
use crate::ngm::{self, OwnKeys, PeerKeys};
use crate::push::{self, PROD_HOST, PushConnection};

/// IDS's command number for "nice" data messages.
const NICE_DATA: i64 = 242;

fn sky_error(message: impl Into<String>) -> AuthError {
    AuthError::Bridge(message.into())
}

/// A device that may answer Skybridge requests.
struct Device {
    endpoint: IdsEndpoint,
    keys: PeerKeys,
}

/// An answer from a device.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Which device (index into the devices found).
    pub device: usize,
    pub msg_id: String,
    pub body: Value,
    /// Part number and count of a streamed answer, if streamed.
    pub part: Option<(u64, u64)>,
}

pub struct Skybridge {
    push: PushConnection,
    identity: IdsIdentity,
    registration: IdsRegistration,
    registered_at: u64,
    devices: Vec<Device>,
    counter: u32,
    dsid: String,
    reused: bool,
    /// The handle the devices were looked up under: each message names it
    /// as the recipient (`tP`), matching the session tokens the lookup gave.
    handle: String,
}

/// A connection kept between runs (see [`Skybridge::saved`]): the same push
/// token, IDS identity and registration, so a new run doesn't register a
/// new endpoint with Apple each time. Holds private keys: store it safely.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedBridge {
    dsid: String,
    push_key: String,
    push_token: String,
    identity: String,
    registration: IdsRegistration,
    /// Seconds since the Unix epoch.
    registered_at: u64,
    /// The last message counter used, so the device sees them go on rising.
    counter: u32,
}

/// How long a registration is used again before registering anew (the
/// device lookup still checks it every run).
const REGISTRATION_KEPT: u64 = 7 * 24 * 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn uuid_bytes(id: &str) -> Vec<u8> {
    uuid::Uuid::parse_str(id)
        .map(|u| u.as_bytes().to_vec())
        .unwrap_or_default()
}

/// What both sides MAC in key verification.
fn pairing_transcript(
    challenge: &[u8],
    ours: &[u8],
    exchange_id: &str,
    session_token: &str,
    first_kt: &[u8],
    second_kt: &[u8],
) -> Vec<u8> {
    let mut out = vec![1u8];
    let field = |out: &mut Vec<u8>, data: &[u8]| {
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(data);
    };
    field(&mut out, challenge);
    field(&mut out, ours);
    out.extend_from_slice(&uuid_bytes(exchange_id));
    field(&mut out, session_token.as_bytes());
    field(&mut out, first_kt);
    field(&mut out, second_kt);
    out
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn bytes_of(v: &Plist) -> Option<Vec<u8>> {
    match v {
        Plist::Data(d) => Some(d.clone()),
        Plist::String(s) => BASE64.decode(s.trim()).ok(),
        _ => None,
    }
}

fn maybe_gunzip(data: Vec<u8>) -> Vec<u8> {
    if data.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(&data[..])
            .read_to_end(&mut out)
            .is_ok()
        {
            return out;
        }
    }
    data
}

impl AuthClient {
    /// A short-lived token proving the web session to the device.
    pub fn session_id_token(&self) -> Result<String, AuthError> {
        let mut resp = self
            .agent()
            .get("https://setup.icloud.com/setup/ws/1/generateSessionIdToken")
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .call()?;
        self.cookie_log().record(&resp);
        let status = resp.status().as_u16();
        if status == 401 || status == 421 {
            return Err(AuthError::SessionExpired);
        }
        let data: Value = resp.body_mut().read_json()?;
        data["sessionIdToken"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| sky_error("no session token from iCloud"))
    }
}

impl Skybridge {
    /// Connect push, register with IDS and find the account's devices under
    /// `handle` (e.g. "mailto:<apple id>"). With `saved` (from an earlier
    /// run on this account), it goes on as that connection: same push
    /// token, identity and, while it's good, registration.
    pub fn connect(
        auth: &AuthClient,
        saved: Option<&SavedBridge>,
        dsid: &str,
        handle: &str,
    ) -> Result<Self, AuthError> {
        let timer = std::time::Instant::now();
        let debug = std::env::var_os("FUJI_DEBUG").is_some();
        let lap = |what: &str| {
            if std::env::var_os("FUJI_DEBUG").is_some() {
                eprintln!("skybridge: {what} at {:?}", timer.elapsed());
            }
        };
        let saved = saved.filter(|s| s.dsid == dsid);
        let old = saved.and_then(|s| {
            Some((
                s,
                BASE64.decode(&s.push_key).ok()?,
                BASE64.decode(&s.push_token).ok()?,
                IdsIdentity::from_json(&s.identity)?,
            ))
        });
        let (mut push, identity, kept, counter) = match old {
            Some((s, key, token, identity)) => {
                let push = PushConnection::reconnect(PROD_HOST, &[ICLOUD_WEB_TOPIC], &key, &token)?;
                let fresh = now_secs().saturating_sub(s.registered_at) < REGISTRATION_KEPT;
                let kept = (push.token == token && fresh)
                    .then(|| (s.registration.clone(), s.registered_at));
                (push, identity, kept, s.counter)
            }
            None => (
                PushConnection::connect(PROD_HOST, &[ICLOUD_WEB_TOPIC])?,
                IdsIdentity::generate()?,
                None,
                0,
            ),
        };
        lap("push connected");
        let reused = kept.is_some();
        let found = match kept {
            Some((registration, at)) => auth
                .ids_query(&push, &registration, handle, ICLOUD_WEB_TOPIC)
                .map(|endpoints| (registration, at, endpoints))
                .ok(),
            None => None,
        };
        let (registration, registered_at, endpoints) = match found {
            Some(found) => found,
            // New, or what was kept no longer works: register (again).
            None => {
                let registration = auth.ids_register(&mut push, &identity, dsid)?;
                lap("registered");
                let endpoints = auth.ids_query(&push, &registration, handle, ICLOUD_WEB_TOPIC)?;
                (registration, now_secs(), endpoints)
            }
        };
        lap("devices found");
        let devices = endpoints
            .into_iter()
            .filter(|e| e.is_device)
            .filter_map(|endpoint| {
                let keys =
                    PeerKeys::from_ids(&endpoint.kt_loggable_data, &endpoint.prekey_data).ok()?;
                Some(Device { endpoint, keys })
            })
            .collect();
        if debug {
            let scheme = |u: &str| u.split(':').next().unwrap_or("").to_string();
            eprintln!(
                "skybridge: own handle {} the looked-up handle (own {}, looked up {})",
                if registration.uri == handle {
                    "is"
                } else {
                    "differs from"
                },
                scheme(&registration.uri),
                scheme(handle),
            );
        }
        Ok(Self {
            push,
            identity,
            registration,
            registered_at,
            devices,
            counter,
            dsid: dsid.to_string(),
            reused,
            handle: handle.to_string(),
        })
    }

    /// Whether this went on with a registration kept from an earlier run.
    pub fn reused(&self) -> bool {
        self.reused
    }

    /// What to keep to connect again as this connection next run.
    pub fn saved(&self) -> SavedBridge {
        SavedBridge {
            dsid: self.dsid.clone(),
            push_key: BASE64.encode(self.push.key_bytes()),
            push_token: BASE64.encode(&self.push.token),
            identity: self.identity.to_json(),
            registration: self.registration.clone(),
            registered_at: self.registered_at,
            counter: self.counter,
        }
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    fn own(&self) -> OwnKeys<'_> {
        OwnKeys {
            signing: self.identity.signing_key(),
            prekey: self.identity.prekey(),
        }
    }

    /// Send `body` to device `index`; returns the message ID.
    pub fn send(
        &mut self,
        index: usize,
        body: Value,
        token: Option<&str>,
    ) -> Result<String, AuthError> {
        let msg_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let mut request = json!({ "msgID": msg_id, "body": body });
        if let Some(token) = token {
            request["token"] = json!(token);
        }
        let plain = serde_json::to_vec(&request).expect("serializable");
        self.counter += 1;
        let device = self
            .devices
            .get(index)
            .ok_or_else(|| sky_error("no such device"))?;
        let outer = ngm::encrypt(&self.own(), &device.keys, &plain, self.counter);

        let mut entry = Dictionary::new();
        entry.insert("P".into(), Plist::Data(outer));
        entry.insert(
            "sT".into(),
            Plist::String(BASE64.encode(&device.endpoint.session_token)),
        );
        entry.insert(
            "t".into(),
            Plist::String(BASE64.encode(&device.endpoint.push_token)),
        );
        // The recipient is the handle the devices were looked up under (as
        // icloud.com does), not our own handle: the session tokens are for it.
        entry.insert("tP".into(), Plist::String(self.handle.clone()));

        // A version 4 UUID, as icloud.com sends.
        let mut uuid = [0u8; 16];
        rand::fill(&mut uuid);
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        let mut envelope = Dictionary::new();
        envelope.insert("E".into(), Plist::String("pair-ec".into()));
        envelope.insert("c".into(), Plist::Integer(NICE_DATA.into()));
        envelope.insert("v".into(), Plist::Integer(6.into()));
        envelope.insert("U".into(), Plist::Data(uuid.to_vec()));
        envelope.insert(
            "i".into(),
            Plist::Integer(u64::from(rand::random::<u32>()).into()),
        );
        envelope.insert("sP".into(), Plist::String(self.registration.uri.clone()));
        envelope.insert("ua".into(), Plist::String("Mozilla".into()));
        envelope.insert("dtl".into(), Plist::Array(vec![Plist::Dictionary(entry)]));
        let mut payload = Vec::new();
        Plist::Dictionary(envelope)
            .to_writer_binary(&mut payload)
            .map_err(|e| sky_error(e.to_string()))?;
        self.push.send_push(ICLOUD_WEB_TOPIC, &payload)?;
        Ok(msg_id)
    }

    /// The next answer from one of the devices, within `timeout`.
    pub fn receive(&mut self, timeout: Duration) -> Result<Option<Reply>, AuthError> {
        let deadline = Instant::now() + timeout;
        let topic = push::topic_hash(ICLOUD_WEB_TOPIC);
        let debug = std::env::var_os("FUJI_DEBUG").is_some();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(p) = self.push.next_push(left)? else {
                return Ok(None);
            };
            if p.topic_hash != topic {
                if debug {
                    eprintln!("skybridge: push on another topic, skipped");
                }
                continue;
            }
            let Some(envelope) = crate::ids::parse_bplist(&p.raw).and_then(Plist::into_dictionary)
            else {
                if debug {
                    eprintln!("skybridge: push that isn't a plist, skipped");
                }
                continue;
            };
            if envelope.get("E").and_then(Plist::as_string) != Some("pair-ec") {
                if debug && envelope.get("c").and_then(Plist::as_signed_integer) == Some(255) {
                    // A delivery status for something we sent: `s` is the code.
                    let num = |k: &str| envelope.get(k).and_then(Plist::as_signed_integer);
                    let sender = envelope
                        .get("t")
                        .and_then(bytes_of)
                        .and_then(|t| self.devices.iter().position(|d| d.endpoint.push_token == t));
                    eprintln!(
                        "skybridge: delivery status s={:?} e={:?} from device {sender:?}",
                        num("s"),
                        num("e"),
                    );
                    continue;
                }
                if debug {
                    let keys: Vec<&str> = envelope.keys().map(String::as_str).collect();
                    eprintln!(
                        "skybridge: message with encryption {:?} (keys {keys:?}, command {:?}), skipped",
                        envelope.get("E").and_then(Plist::as_string),
                        envelope.get("c").and_then(Plist::as_signed_integer),
                    );
                }
                continue;
            }
            let (Some(from), Some(sealed)) = (
                envelope.get("t").and_then(bytes_of),
                envelope.get("P").and_then(bytes_of),
            ) else {
                continue;
            };
            let Some(index) = self
                .devices
                .iter()
                .position(|d| d.endpoint.push_token == from)
            else {
                if debug {
                    eprintln!("skybridge: a message from an unknown sender, skipped");
                }
                continue;
            };
            let own = self.own();
            let Ok((_counter, message)) = ngm::decrypt(&own, &self.devices[index].keys, &sealed)
            else {
                if std::env::var_os("FUJI_DEBUG").is_some() {
                    eprintln!("skybridge: a message from device {index} didn't decrypt");
                }
                continue;
            };
            let Ok(reply) = serde_json::from_slice::<Value>(&maybe_gunzip(message)) else {
                if debug {
                    eprintln!("skybridge: a message from device {index} isn't JSON, skipped");
                }
                continue;
            };
            let part = match (reply["i"].as_u64(), reply["t"].as_u64()) {
                (Some(i), Some(t)) => Some((i, t)),
                _ => None,
            };
            return Ok(Some(Reply {
                device: index,
                msg_id: reply["msgID"].as_str().unwrap_or_default().to_string(),
                body: reply["body"].clone(),
                part,
            }));
        }
    }

    /// Prove we hold the pairing key: every device is asked, the one paired
    /// with us answers. Returns (device index, device name).
    pub fn verify_key(
        &mut self,
        pairing_key: &[u8],
        session_token: &str,
    ) -> Result<(usize, String), AuthError> {
        struct Pending {
            exchange: String,
            step1: String,
            step2: Option<(String, Vec<u8>, Vec<u8>)>,
        }
        let ours = self.identity.kt_loggable_data();
        let mut pending: Vec<Option<Pending>> = Vec::new();
        for index in 0..self.devices.len() {
            let exchange = uuid::Uuid::new_v4().to_string().to_uppercase();
            let body = json!({ "kv": { "s": { "v": 1, "t": exchange } } });
            let step1 = self.send(index, body, Some(session_token))?;
            if std::env::var_os("FUJI_DEBUG").is_some() {
                eprintln!("skybridge: asked device {index} to verify the key");
            }
            pending.push(Some(Pending {
                exchange,
                step1,
                step2: None,
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let timer = Instant::now();
        while let Some(reply) = self.receive(deadline.saturating_duration_since(Instant::now()))? {
            if std::env::var_os("FUJI_DEBUG").is_some() {
                eprintln!(
                    "skybridge: reply from device {} at {:?}",
                    reply.device,
                    timer.elapsed()
                );
            }
            let Some(Some(p)) = pending.get_mut(reply.device) else {
                continue;
            };
            let theirs = self.devices[reply.device].keys.kt_loggable_data.clone();
            if reply.msg_id == p.step1 {
                // The device's challenge; answer with our MAC and challenge.
                let Some(challenge) = reply.body["kv"]["c"]["c"]
                    .as_str()
                    .and_then(|c| BASE64.decode(c).ok())
                    .filter(|c| c.len() == 32)
                else {
                    pending[reply.device] = None;
                    continue;
                };
                let mut mine = [0u8; 32];
                rand::fill(&mut mine);
                let exchange = p.exchange.clone();
                let tag = mac(
                    pairing_key,
                    &pairing_transcript(
                        &challenge,
                        &mine,
                        &exchange,
                        session_token,
                        &ours,
                        &theirs,
                    ),
                );
                let body = json!({ "kv": { "rc": {
                    "r": BASE64.encode(&tag),
                    "c": BASE64.encode(mine),
                    "t": exchange,
                } } });
                let step2 = self.send(reply.device, body, None)?;
                if let Some(Some(p)) = pending.get_mut(reply.device) {
                    p.step2 = Some((step2, challenge, mine.to_vec()));
                }
            } else if let Some((step2, challenge, mine)) = &p.step2 {
                if &reply.msg_id != step2 {
                    continue;
                }
                if reply.body.get("error").is_some() {
                    pending[reply.device] = None;
                    continue;
                }
                let r = &reply.body["kv"]["r"];
                let (answer, name) = match r {
                    Value::String(s) => (s.as_str(), ""),
                    _ => (
                        r["r"].as_str().unwrap_or_default(),
                        r["s"]["deviceDisplayName"].as_str().unwrap_or_default(),
                    ),
                };
                let expected = mac(
                    pairing_key,
                    &pairing_transcript(
                        challenge,
                        mine,
                        &p.exchange,
                        session_token,
                        &theirs,
                        &ours,
                    ),
                );
                if BASE64.decode(answer).ok().as_deref() == Some(&expected[..]) {
                    return Ok((reply.device, name.to_string()));
                }
                pending[reply.device] = None;
            }
            if pending.iter().all(Option::is_none) {
                break;
            }
        }
        Err(sky_error(
            "None of your devices accepted Fuji's pairing key.",
        ))
    }

    pub fn close(self) {
        self.push.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_layout() {
        let t = pairing_transcript(
            &[1; 32],
            &[2; 32],
            "00112233-4455-6677-8899-AABBCCDDEEFF",
            "tok",
            b"ab",
            b"c",
        );
        assert_eq!(t[0], 1);
        assert_eq!(&t[1..3], &[32, 0]);
        assert_eq!(&t[35..37], &[32, 0]);
        assert_eq!(t[69], 0x00);
        assert_eq!(t[84], 0xff);
        assert_eq!(&t[85..87], &[3, 0]);
        assert_eq!(&t[87..90], b"tok");
        assert_eq!(t.len(), 1 + 34 + 34 + 16 + 5 + 4 + 3);
    }
}
