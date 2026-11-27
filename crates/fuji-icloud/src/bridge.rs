//! Two-factor approval on a trusted device, the newer way ("bridge"): the
//! device approves the sign-in, the user types the code it shows, and the
//! two sides run SPAKE2+ over Apple's push service with that code. Besides
//! signing in, this leaves a key shared with the device: icloud.com keeps it
//! as the Skybridge pairing key.
//!
//! Flow (the sign-in widget's `bridge/*` thunks):
//! 1. Connect to push (topic from the two-factor page) and post step 0 with
//!    a session ID and the push token. The device is asked to approve.
//! 2. A push with `nextStep: 2` brings the salt.
//! 3. With the code: post step 2 with our SPAKE2+ share.
//! 4. A push with `nextStep: 4` and `data` brings the device's share and
//!    confirmation; post step 4 with ours.
//! 5. A push with `encryptedCode` brings the real verification code,
//!    encrypted under the agreed key; validate it, post "done", log in.
//!
//! Blocking: use from a background thread.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};

use crate::auth::{Account, AuthClient, AuthError, BridgeConfig, SignInStep};
use crate::passkey::PairingKey;
use crate::push::{self, PushConnection, PushError};
use crate::spake::Prover;

/// The topic when the two-factor page doesn't name one.
const DEFAULT_TOPIC: &str = "com.apple.idmsauthwidget";
/// How long to wait for a device to approve. The widget puts no deadline on
/// this; the caller shows the code screen meanwhile, so be generous.
const FIRST_PUSH: Duration = Duration::from_secs(180);
/// How long to wait for the device in later steps.
const NEXT_PUSH: Duration = Duration::from_secs(60);

/// How a device approval started.
#[derive(Debug)]
pub enum BridgeStart {
    /// A device approved: finish with the code it shows ([`AuthClient::finish_bridge`]).
    Approval(Box<BridgeApproval>),
    /// A device approved, but Apple chose plain code checking: submit the
    /// code it shows with [`AuthClient::submit_device_code`]. Don't ask for
    /// a new one; that would prompt the devices again.
    PlainCode,
    /// Not offered for this sign-in: ask for a code ([`AuthClient::request_device_code`]).
    NotOffered,
}

/// An approval in progress: waiting for the user to type the device's code.
pub struct BridgeApproval {
    push: PushConnection,
    topic_hash: Vec<u8>,
    session_uuid: String,
    ptkn: String,
    salt: Vec<u8>,
    state: Map<String, Value>,
}

impl std::fmt::Debug for BridgeApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeApproval(..)")
    }
}

/// Diagnostics on stderr when `FUJI_DEBUG` is set. Never logs secrets.
fn debug(message: &str) {
    if std::env::var_os("FUJI_DEBUG").is_some() {
        eprintln!("fuji-icloud: bridge: {message}");
    }
}

/// What a push carried, for the debug log: field names and step, no values.
fn describe(payload: &Value) -> String {
    let has = |name: &str| payload.get(name).is_some_and(|v| !v.is_null());
    let present: Vec<&str> = [
        "salt",
        "data",
        "encryptedCode",
        "sessionUUID",
        "idmsdata",
        "akdata",
        "mid",
    ]
    .into_iter()
    .filter(|n| has(n))
    .collect();
    let txnid =
        text(payload, "txnid").map_or("none", |t| if t.ends_with("_W") { "_W" } else { "other" });
    format!(
        "nextStep={:?} ec={} txnid={txnid} fields={present:?}",
        next_step(payload),
        payload.get("ec").map_or("none".into(), Value::to_string),
    )
}

fn bridge_error(message: impl Into<String>) -> AuthError {
    AuthError::Bridge(message.into())
}

impl From<PushError> for AuthError {
    fn from(err: PushError) -> Self {
        bridge_error(err.to_string())
    }
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let normal = text.replace('-', "+").replace('_', "/");
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normal)
        .ok()
}

/// `nextStep` arrives as a number or a string.
fn next_step(payload: &Value) -> Option<u64> {
    match &payload["nextStep"] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn text<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload.get(name)?.as_str().filter(|s| !s.is_empty())
}

impl BridgeApproval {
    /// The fields the device sent that every later step echoes.
    fn remember(&mut self, payload: &Value) {
        for name in ["idmsdata", "akdata", "mid"] {
            if let Some(v) = payload.get(name).filter(|v| !v.is_null()) {
                self.state.insert(name.into(), v.clone());
            }
        }
    }

    /// Wait for the next push for this session on our topic.
    fn next(&mut self, timeout: Duration) -> Result<Value, AuthError> {
        loop {
            let Some(push) = self.push.next_push(timeout)? else {
                return Err(bridge_error(
                    "Your device didn't answer. Check that it's on and connected, then try again.",
                ));
            };
            if push.topic_hash != self.topic_hash {
                continue;
            }
            let Some(payload) = push.payload else {
                debug("push without a payload");
                continue;
            };
            debug(&format!("push: {}", describe(&payload)));
            if let Some(ec) = payload
                .get("ec")
                .and_then(Value::as_i64)
                .filter(|&ec| ec != 0)
            {
                return Err(bridge_error(format!(
                    "Your device stopped the approval (error {ec})."
                )));
            }
            // Plain code checking ("_W") names the session by our push token
            // (base64, third part of `txnid`), not by `sessionUUID`.
            if let Some(txnid) = text(&payload, "txnid").filter(|t| t.ends_with("_W")) {
                let ours = txnid
                    .split('_')
                    .nth(2)
                    .and_then(b64_decode)
                    .is_some_and(|t| t.eq_ignore_ascii_case(self.ptkn.as_bytes()));
                if !ours {
                    debug("push: _W for another push token, skipped");
                    continue;
                }
            } else {
                let session = text(&payload, "sessionUUID");
                if session.is_some() && session != Some(self.session_uuid.as_str()) {
                    debug("push: for another session, skipped");
                    continue;
                }
            }
            self.remember(&payload);
            return Ok(payload);
        }
    }

    /// The body of a step post.
    fn step_body(&self, next_step: u64, data: Option<String>) -> Value {
        let mut body = json!({
            "sessionUUID": self.session_uuid,
            "ptkn": self.ptkn,
            "nextStep": next_step,
        });
        if let Some(v) = self.state.get("idmsdata") {
            body["idmsdata"] = v.clone();
        }
        if let Some(v) = self.state.get("akdata") {
            body["akdata"] = match v {
                Value::String(_) => v.clone(),
                other => Value::String(other.to_string()),
            };
        }
        if let Some(data) = data {
            body["data"] = json!(data);
        }
        body
    }
}

impl AuthClient {
    /// After the password, when two-factor is needed: ask the user's devices
    /// to approve, and wait (up to a few minutes) until one has. Blocks, so
    /// show the code screen meanwhile.
    pub fn start_bridge(&self) -> Result<BridgeStart, AuthError> {
        let Some(config) = self.bridge_config()? else {
            debug("not offered on the two-factor page");
            return Ok(BridgeStart::NotOffered);
        };
        self.start_bridge_with(&config)
    }

    fn start_bridge_with(&self, config: &BridgeConfig) -> Result<BridgeStart, AuthError> {
        let topic = config.apns_topic.as_deref().unwrap_or(DEFAULT_TOPIC);
        let host = push::host_for(config.apns_environment.as_deref().unwrap_or("prod"));
        let push = PushConnection::connect(host, &[topic])?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut approval = BridgeApproval {
            ptkn: push.token_hex(),
            push,
            topic_hash: push::topic_hash(topic),
            session_uuid: format!("{}-{now}", uuid::Uuid::new_v4()),
            salt: Vec::new(),
            state: Map::new(),
        };
        self.bridge_post("bridge/step/0", &approval.step_body(0, None))?;

        let payload = loop {
            let payload = approval.next(FIRST_PUSH)?;
            if next_step(&payload) == Some(2) {
                break payload;
            }
        };
        if text(&payload, "txnid").is_some_and(|t| t.ends_with("_W")) {
            // Apple chose plain code checking: no key exchange this time.
            debug("approved, plain code checking");
            approval.push.close();
            return Ok(BridgeStart::PlainCode);
        }
        approval.salt = text(&payload, "salt")
            .and_then(b64_decode)
            .ok_or_else(|| bridge_error("Your device sent no salt."))?;
        debug("approved, key exchange");
        Ok(BridgeStart::Approval(Box::new(approval)))
    }

    /// Finish with the code the device shows. Returns the account and the
    /// key now shared with that device.
    pub fn finish_bridge(
        &self,
        mut approval: BridgeApproval,
        code: &str,
    ) -> Result<(Account, PairingKey), AuthError> {
        let code = code.trim();
        let prover = Prover::new(&approval.salt, code);
        let message1 = BASE64.encode(prover.message1());
        self.bridge_post("bridge/step/2", &approval.step_body(2, Some(message1)))?;

        // The device's share and confirmation: base64 of "<share>_<confirmation>".
        let payload = loop {
            let payload = approval.next(NEXT_PUSH)?;
            if next_step(&payload) == Some(4) && text(&payload, "data").is_some() {
                break payload;
            }
        };
        let joined = text(&payload, "data")
            .and_then(b64_decode)
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| bridge_error("Your device sent an unreadable message."))?;
        let (share, confirmation) = joined
            .split_once('_')
            .and_then(|(s, c)| Some((b64_decode(s)?, b64_decode(c)?)))
            .ok_or_else(|| bridge_error("Your device sent an unreadable message."))?;
        let confirming = prover
            .finish(&share)
            .map_err(|e| bridge_error(e.to_string()))?;
        let message2 = BASE64.encode(&confirming.message2);
        self.bridge_post("bridge/step/4", &approval.step_body(4, Some(message2)))?;
        let agreed = confirming
            .verify(&confirmation)
            .map_err(|_| AuthError::InvalidCode)?;

        // The real code, encrypted under the agreed key.
        let payload = loop {
            let payload = approval.next(NEXT_PUSH)?;
            if text(&payload, "encryptedCode").is_some() {
                break payload;
            }
        };
        let final_step = if next_step(&payload) == Some(6) { 6 } else { 4 };
        let sealed = text(&payload, "encryptedCode")
            .and_then(b64_decode)
            .ok_or_else(|| bridge_error("Your device sent an unreadable code."))?;
        let real_code = agreed
            .decrypt(&sealed)
            .map_err(|e| bridge_error(e.to_string()))?;
        let status = self.bridge_post_status(
            "bridge/code/validate",
            &json!({ "sessionUUID": approval.session_uuid, "code": real_code }),
        )?;
        if !matches!(status, 200..=299 | 409 | 412) {
            return Err(AuthError::InvalidCode);
        }
        let done = BASE64.encode("done");
        // Closing the exchange; a failure here doesn't undo the sign-in.
        let _ = self.bridge_post_status(
            &format!("bridge/step/{final_step}"),
            &approval.step_body(final_step, Some(done)),
        );
        approval.push.close();

        let pairing = PairingKey {
            key: BASE64.encode(&agreed.shared_key),
            mid: approval
                .state
                .get("mid")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        match self.after_bridge()? {
            SignInStep::Done(account) => Ok((account, pairing)),
            SignInStep::NeedsCode => Err(bridge_error(
                "Apple still wants a verification code after the approval",
            )),
        }
    }

    fn bridge_post(&self, path: &str, body: &Value) -> Result<(), AuthError> {
        match self.bridge_post_status(path, body)? {
            200..=299 => Ok(()),
            status => Err(AuthError::Unexpected {
                status,
                message: format!("{path} was refused"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_next_step_either_way() {
        assert_eq!(next_step(&json!({ "nextStep": 2 })), Some(2));
        assert_eq!(next_step(&json!({ "nextStep": "4" })), Some(4));
        assert_eq!(next_step(&json!({})), None);
    }

    #[test]
    fn decodes_base64_variants() {
        assert_eq!(b64_decode("aGk="), Some(b"hi".to_vec()));
        assert_eq!(b64_decode("aGk"), Some(b"hi".to_vec()));
        assert_eq!(b64_decode("-_8"), Some(vec![0xfb, 0xff]));
    }
}
