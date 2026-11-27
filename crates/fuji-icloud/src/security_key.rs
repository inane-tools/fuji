//! Two-factor with a security key (FIDO), for accounts that have them.
//! Apple then sends no codes, to devices or by text: the two-factor page
//! asks for the key instead (`twoSV.fsaVerification`, first factor
//! `fsa2_hsa2`).
//!
//! As icloud.com does it: a WebAuthn get for the page's challenge, its
//! relying party and the account's key handles, then the answer posted to
//! `/appleauth/auth/verify/security/key`. Talking to the key is the
//! platform's job; this module only talks to Apple.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::{Account, AuthClient, AuthError};
use crate::passkey::PasskeyAssertion;

/// What the security key is asked to sign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityKeyChallenge {
    pub challenge: Vec<u8>,
    /// The relying party the keys were registered for.
    pub rp_id: String,
    /// Credential IDs of the account's keys.
    pub key_handles: Vec<Vec<u8>>,
}

impl SecurityKeyChallenge {
    /// The client data JSON, as a browser on the sign-in page writes it.
    /// Its SHA-256 is what the key signs; Apple gets the JSON itself.
    pub fn client_data_json(&self) -> Vec<u8> {
        crate::passkey::client_data_json(&self.challenge)
    }

    /// SHA-256 of [`Self::client_data_json`].
    pub fn client_data_hash(&self) -> [u8; 32] {
        Sha256::digest(self.client_data_json()).into()
    }
}

/// Base64 in either alphabet, padded or not.
fn decode_any(text: &str) -> Option<Vec<u8>> {
    let normal = text
        .trim()
        .trim_end_matches('=')
        .replace('-', "+")
        .replace('_', "/");
    BASE64_NO_PAD.decode(normal).ok()
}

/// The security key challenge on the two-factor page's boot data, if this
/// sign-in wants one.
pub(crate) fn parse_security_key(boot: &Value) -> Option<SecurityKeyChallenge> {
    let two_sv = &boot["twoSV"];
    let wants_key = two_sv["authFactors"]
        .as_array()
        .is_some_and(|f| f.iter().any(|f| f.as_str() == Some("fsa2_hsa2")));
    let fsa = &two_sv["fsaVerification"]["fsaChallenge"];
    let challenge = fsa["challenge"].as_str().and_then(decode_any)?;
    if !wants_key {
        return None;
    }
    let rp_id = fsa["rpId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("apple.com")
        .to_string();
    let key_handles = fsa["keyHandles"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|h| h.as_str().and_then(decode_any))
                .collect()
        })
        .unwrap_or_default();
    Some(SecurityKeyChallenge {
        challenge,
        rp_id,
        key_handles,
    })
}

/// The body icloud.com posts: standard base64, the challenge and IDs
/// without padding.
fn verify_body(challenge: &SecurityKeyChallenge, assertion: &PasskeyAssertion) -> Value {
    json!({
        "challenge": BASE64_NO_PAD.encode(&challenge.challenge),
        "clientData": BASE64.encode(&assertion.client_data_json),
        "signatureData": BASE64.encode(&assertion.signature),
        "authenticatorData": BASE64.encode(&assertion.authenticator_data),
        "userHandle": BASE64_NO_PAD.encode(&assertion.user_handle),
        "credentialID": BASE64_NO_PAD.encode(&assertion.credential_id),
        "rpId": challenge.rp_id,
    })
}

impl AuthClient {
    /// After the password, when two-factor is needed: the security key
    /// challenge, if this account signs in with a key instead of codes.
    pub fn security_key_challenge(&self) -> Result<Option<SecurityKeyChallenge>, AuthError> {
        let boot = self.two_factor_boot()?;
        Ok(boot.as_ref().and_then(parse_security_key))
    }

    /// Finish signing in with the key's answer to `challenge`.
    pub fn submit_security_key(
        &self,
        challenge: &SecurityKeyChallenge,
        assertion: &PasskeyAssertion,
    ) -> Result<Account, AuthError> {
        let (status, body) =
            self.auth_post_raw("verify/security/key", &verify_body(challenge, assertion))?;
        match status {
            200..=299 | 409 | 412 => self.after_second_factor(),
            401 => Err(AuthError::SessionExpired),
            403 => Err(AuthError::Locked),
            status => Err(AuthError::Unexpected {
                status,
                message: crate::auth::error_message(&body)
                    .unwrap_or_else(|| "the security key wasn't accepted".into()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(factors: Value) -> Value {
        json!({ "twoSV": {
            "authFactors": factors,
            "fsaVerification": { "fsaChallenge": {
                "challenge": "AAEC/w==",
                "rpId": "apple.com",
                "keyHandles": ["q83v", "-_8"],
            }},
        }})
    }

    #[test]
    fn reads_the_challenge() {
        let c = parse_security_key(&boot(json!(["fsa2_hsa2"]))).unwrap();
        assert_eq!(c.challenge, vec![0, 1, 2, 0xff]);
        assert_eq!(c.rp_id, "apple.com");
        assert_eq!(
            c.key_handles,
            vec![vec![0xab, 0xcd, 0xef], vec![0xfb, 0xff]]
        );
    }

    #[test]
    fn ignores_pages_that_dont_want_a_key() {
        assert!(parse_security_key(&boot(json!(["sms"]))).is_none());
        assert!(parse_security_key(&json!({ "twoSV": {} })).is_none());
    }

    #[test]
    fn client_data_names_the_sign_in_page() {
        let c = parse_security_key(&boot(json!(["fsa2_hsa2"]))).unwrap();
        let text = String::from_utf8(c.client_data_json()).unwrap();
        assert_eq!(
            text,
            r#"{"type":"webauthn.get","challenge":"AAEC_w","origin":"https://idmsa.apple.com","crossOrigin":false}"#
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "webauthn.get");
    }

    #[test]
    fn body_uses_standard_base64() {
        let c = parse_security_key(&boot(json!(["fsa2_hsa2"]))).unwrap();
        let a = PasskeyAssertion {
            client_data_json: b"{}".to_vec(),
            authenticator_data: vec![1, 2, 3],
            signature: vec![0xfb, 0xff],
            user_handle: Vec::new(),
            credential_id: vec![0xfb, 0xff],
            large_blob: None,
        };
        let body = verify_body(&c, &a);
        assert_eq!(body["challenge"], "AAEC/w");
        assert_eq!(body["clientData"], "e30=");
        assert_eq!(body["signatureData"], "+/8=");
        assert_eq!(body["credentialID"], "+/8");
        assert_eq!(body["userHandle"], "");
        assert_eq!(body["rpId"], "apple.com");
    }
}
