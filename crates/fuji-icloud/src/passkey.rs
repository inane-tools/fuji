//! Signing in with a passkey (usually the iPhone's, over QR code and
//! Bluetooth), the way the Apple ID sign-in widget does it.
//!
//! Besides signing in, an Apple passkey can hand over a *pairing key* in its
//! WebAuthn large blob: a secret the web client shares with that device, used
//! later to send it Photos searches ("Skybridge").
//!
//! The WebAuthn call itself belongs to the platform (on Windows,
//! `webauthn.dll`), so this module only talks to Apple: it gets a challenge,
//! and turns the platform's answer into a signed-in session.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64URL};
use serde_json::{Map, Value, json};

use crate::auth::{AuthClient, AuthError, SignInStep};

/// Where the sign-in widget runs; the client data names it as the origin.
pub const ORIGIN: &str = "https://idmsa.apple.com";

/// Keys of the large blob that stay on this side.
const BLOB_PAIRING_KEY: &str = "com.apple.gs.passkey.pairingkey";
const BLOB_MID: &str = "com.apple.gs.passkey.mid";
const BLOB_SHARING_KEY: &str = "com.apple.gs.passkey.sharingkey";

/// What to ask the passkey to sign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasskeyChallenge {
    /// The bytes to sign.
    pub challenge: Vec<u8>,
    /// As Apple sent it (standard base64), echoed back on submit.
    challenge_text: String,
    /// The relying party the passkey belongs to.
    pub rp_id: String,
    request_id: String,
    /// Credential IDs Apple will accept; empty means any of the account's.
    pub allow_credentials: Vec<Vec<u8>>,
}

/// What the platform's WebAuthn call returned.
#[derive(Clone, Debug, Default)]
pub struct PasskeyAssertion {
    /// The exact client data JSON that was hashed and signed.
    pub client_data_json: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub signature: Vec<u8>,
    pub user_handle: Vec<u8>,
    pub credential_id: Vec<u8>,
    /// The passkey's large blob, if it has one and the platform read it.
    pub large_blob: Option<Vec<u8>>,
}

/// A secret shared with the device whose passkey signed in.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingKey {
    /// Base64, as Apple's blob has it (32 bytes decoded).
    pub key: String,
    /// Apple's "mid" for the pairing, if given.
    pub mid: Option<String>,
}

impl std::fmt::Debug for PairingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingKey(..)")
    }
}

/// The client data JSON for `challenge`, the way a browser writes it.
pub fn client_data_json(challenge: &[u8]) -> Vec<u8> {
    format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}","crossOrigin":false}}"#,
        BASE64URL.encode(challenge)
    )
    .into_bytes()
}

impl AuthClient {
    /// Ask Apple for a passkey challenge. `apple_id` narrows it to that
    /// account; without it any of the user's passkeys can answer.
    pub fn passkey_challenge(&self, apple_id: Option<&str>) -> Result<PasskeyChallenge, AuthError> {
        let mut body = json!({ "passkeyAutofill": false });
        if let Some(apple_id) = apple_id.map(str::trim).filter(|a| !a.is_empty()) {
            body["accountName"] = json!(apple_id);
            self.set_apple_id(apple_id);
        }
        let data = self.auth_post("verify/device/key/challenge", &[], &body)?;
        parse_challenge(&data)
    }

    /// Finish signing in with the platform's answer to `challenge`. Returns
    /// the sign-in result and the pairing key, if the passkey gave one.
    pub fn passkey_sign_in(
        &self,
        challenge: &PasskeyChallenge,
        assertion: &PasskeyAssertion,
    ) -> Result<(SignInStep, Option<PairingKey>), AuthError> {
        let (body, pairing) = assertion_body(challenge, assertion)?;
        self.auth_post(
            "verify/device/key",
            &[("isRememberMeEnabled", "true")],
            &body,
        )?;
        let step = self.after_passkey()?;
        Ok((step, pairing))
    }
}

/// Base64 as Apple writes it: standard or URL alphabet, padded or not.
fn decode_any(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let normal: String = text.replace('-', "+").replace('_', "/");
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normal)
        .ok()
}

fn bad(message: impl Into<String>) -> AuthError {
    AuthError::Unexpected {
        status: 200,
        message: message.into(),
    }
}

fn parse_challenge(data: &Value) -> Result<PasskeyChallenge, AuthError> {
    let fsa = data
        .get("fsaChallenge")
        .ok_or_else(|| bad("passkey challenge missing"))?;
    let text = |name: &str| {
        fsa.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| bad(format!("passkey challenge has no {name}")))
    };
    let challenge_text = text("challenge")?;
    let challenge =
        decode_any(&challenge_text).ok_or_else(|| bad("passkey challenge isn't base64"))?;
    let allow_credentials = fsa
        .get("keyHandles")
        .and_then(Value::as_array)
        .map(|handles| {
            handles
                .iter()
                .filter_map(Value::as_str)
                .filter_map(decode_any)
                .collect()
        })
        .unwrap_or_default();
    Ok(PasskeyChallenge {
        challenge,
        challenge_text,
        rp_id: text("rpId")?,
        request_id: text("requestId")?,
        allow_credentials,
    })
}

/// Split the large blob: the pairing key stays here, the rest goes to Apple.
fn split_blob(blob: &[u8]) -> (Option<String>, Option<PairingKey>) {
    let Ok(Value::Object(mut fields)) = serde_json::from_slice::<Value>(blob) else {
        // Not the JSON Apple's passkeys hold: pass it on unchanged.
        return (Some(BASE64.encode(blob)), None);
    };
    let take = |fields: &mut Map<String, Value>, name: &str| {
        fields
            .remove(name)
            .and_then(|v| v.as_str().map(str::to_string))
    };
    let key = take(&mut fields, BLOB_PAIRING_KEY);
    let mid = take(&mut fields, BLOB_MID);
    fields.remove(BLOB_SHARING_KEY);
    let rest = serde_json::to_vec(&Value::Object(fields)).unwrap_or_default();
    (
        Some(BASE64.encode(rest)),
        key.map(|key| PairingKey { key, mid }),
    )
}

fn assertion_body(
    challenge: &PasskeyChallenge,
    a: &PasskeyAssertion,
) -> Result<(Value, Option<PairingKey>), AuthError> {
    if a.signature.is_empty() || a.authenticator_data.is_empty() {
        return Err(bad("the passkey returned no signature"));
    }
    let mut body = json!({
        "challenge": challenge.challenge_text,
        "clientData": BASE64.encode(&a.client_data_json),
        "signatureData": BASE64.encode(&a.signature),
        "authenticatorData": BASE64.encode(&a.authenticator_data),
        "userHandle": BASE64.encode(&a.user_handle).trim_end_matches('=').to_string(),
        "credentialID": BASE64.encode(&a.credential_id),
        "rpId": challenge.rp_id,
        "requestId": challenge.request_id,
        "rememberMe": true,
        "passkeyAutofill": false,
    });
    let mut pairing = None;
    if let Some(blob) = a.large_blob.as_deref().filter(|b| !b.is_empty()) {
        let (extension_data, key) = split_blob(blob);
        if let Some(data) = extension_data {
            body["extensionData"] = json!(data);
        }
        pairing = key;
    }
    Ok((body, pairing))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to Apple: asks for a challenge (signs nothing in). Run with
    /// `cargo test -p fuji-icloud live_passkey_challenge -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_passkey_challenge() {
        let client = AuthClient::new();
        let c = client.passkey_challenge(None).expect("a challenge");
        println!(
            "rpId {}, {} challenge bytes, {} allowed credentials",
            c.rp_id,
            c.challenge.len(),
            c.allow_credentials.len()
        );
        assert!(!c.challenge.is_empty());
    }

    #[test]
    fn client_data_names_the_widget_origin() {
        let json: Value = serde_json::from_slice(&client_data_json(&[0xfb, 0xff, 1])).unwrap();
        assert_eq!(json["type"], "webauthn.get");
        assert_eq!(json["challenge"], "-_8B");
        assert_eq!(json["origin"], ORIGIN);
    }

    #[test]
    fn parses_a_challenge() {
        let data = json!({ "fsaChallenge": {
            "challenge": BASE64.encode([1, 2, 3]),
            "rpId": "apple.com",
            "requestId": "req-1",
            "keyHandles": [BASE64URL.encode([9, 9])],
        }, "requirePrf": false });
        let c = parse_challenge(&data).unwrap();
        assert_eq!(c.challenge, vec![1, 2, 3]);
        // Apple sends 32 bytes as 43 characters, without padding.
        assert_eq!(decode_any(&"A".repeat(43)).map(|b| b.len()), Some(32));
        assert_eq!(c.rp_id, "apple.com");
        assert_eq!(c.allow_credentials, vec![vec![9, 9]]);
        assert!(parse_challenge(&json!({})).is_err());
    }

    #[test]
    fn pairing_key_stays_out_of_what_apple_gets() {
        let blob = json!({
            BLOB_PAIRING_KEY: "a2V5",
            BLOB_MID: "m-1",
            BLOB_SHARING_KEY: "c2hhcmU=",
            "other": 1,
        });
        let (rest, key) = split_blob(&serde_json::to_vec(&blob).unwrap());
        let key = key.unwrap();
        assert_eq!(key.key, "a2V5");
        assert_eq!(key.mid.as_deref(), Some("m-1"));
        let sent: Value = serde_json::from_slice(&BASE64.decode(rest.unwrap()).unwrap()).unwrap();
        assert_eq!(sent, json!({ "other": 1 }));
        // The key never shows in debug output.
        assert_eq!(format!("{key:?}"), "PairingKey(..)");
    }

    #[test]
    fn assertion_body_shape() {
        let challenge = PasskeyChallenge {
            challenge: vec![1],
            challenge_text: "AQ==".into(),
            rp_id: "apple.com".into(),
            request_id: "r".into(),
            allow_credentials: vec![],
        };
        let assertion = PasskeyAssertion {
            client_data_json: b"{}".to_vec(),
            authenticator_data: vec![1],
            signature: vec![2],
            user_handle: vec![3],
            credential_id: vec![4],
            large_blob: None,
        };
        let (body, key) = assertion_body(&challenge, &assertion).unwrap();
        assert!(key.is_none());
        assert_eq!(body["challenge"], "AQ==");
        assert_eq!(body["userHandle"], "Aw");
        assert_eq!(body["credentialID"], "BA==");
        assert!(body.get("extensionData").is_none());
        let empty = PasskeyAssertion::default();
        assert!(assertion_body(&challenge, &empty).is_err());
    }
}
