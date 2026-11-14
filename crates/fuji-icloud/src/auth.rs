//! Apple ID sign-in for iCloud on the web.
//!
//! Flow, as icloud.com does it:
//! 1. `idmsa.apple.com/appleauth/auth/signin/init` and `/signin/complete` run SRP.
//! 2. A 409 from `signin/complete` means a two-factor code is needed. The code
//!    comes from a trusted Apple device, or by SMS to a trusted phone number.
//! 3. After the code is accepted, `/2sv/trust` returns a trust token so later
//!    sign-ins on this machine skip the code.
//! 4. `setup.icloud.com/setup/ws/1/accountLogin` exchanges the session token
//!    for iCloud web cookies and the list of service URLs.
//!
//! Blocking I/O: call these from a background thread.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ureq::Agent;
use ureq::http::Response;

use crate::srp::{self, Protocol, SrpClient};

const AUTH_ROOT: &str = "https://idmsa.apple.com";
const AUTH_ENDPOINT: &str = "https://idmsa.apple.com/appleauth/auth";
const HOME_ENDPOINT: &str = "https://www.icloud.com";
const SETUP_ENDPOINT: &str = "https://setup.icloud.com/setup/ws/1";
/// The OAuth client id icloud.com identifies itself with.
const WIDGET_KEY: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
/// Apple's error code for a wrong verification code.
const WRONG_CODE: i64 = -21669;
const JSON_ACCEPT: &str = "application/json, text/javascript";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Incorrect Apple ID or password.")]
    InvalidCredentials,
    #[error("That code isn't right. Check it and try again.")]
    InvalidCode,
    #[error("This Apple ID is locked. Unlock it at iforgot.apple.com, then try again.")]
    Locked,
    #[error("Apple requires this account to use {0}, which fuji doesn't support yet.")]
    WrongDomain(String),
    #[error("Your saved session has expired. Sign in again.")]
    SessionExpired,
    #[error("{0}")]
    Srp(#[from] srp::SrpError),
    #[error("Couldn't reach Apple: {0}")]
    Network(#[from] ureq::Error),
    #[error("Apple returned an error ({status}): {message}")]
    Unexpected { status: u16, message: String },
}

/// The tokens worth keeping between runs. Store them in the OS keychain.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedSession {
    pub apple_id: String,
    pub client_id: String,
    pub account_country: Option<String>,
    pub session_token: Option<String>,
    pub trust_token: Option<String>,
}

/// A signed-in iCloud account.
#[derive(Clone, Debug)]
pub struct Account {
    pub apple_id: String,
    pub dsid: String,
    pub full_name: String,
    /// Service name (for example `ckdatabasews`) to base URL.
    pub webservices: HashMap<String, String>,
}

/// What happened after a sign-in step.
#[derive(Clone, Debug)]
pub enum SignInStep {
    Done(Account),
    /// A verification code was sent to the user's trusted devices.
    NeedsCode,
}

/// A phone number that can receive codes by text message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedPhone {
    pub id: u64,
    /// For example `(•••) •••-••12`.
    pub number: String,
}

#[derive(Default)]
struct State {
    apple_id: String,
    client_id: String,
    scnt: Option<String>,
    session_id: Option<String>,
    session_token: Option<String>,
    trust_token: Option<String>,
    account_country: Option<String>,
}

pub struct AuthClient {
    agent: Agent,
    state: Mutex<State>,
}

#[derive(Deserialize)]
struct InitResponse {
    iteration: u32,
    salt: String,
    protocol: String,
    b: String,
    c: Value,
}

impl Default for AuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthClient {
    pub fn new() -> Self {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: Agent::new_with_config(config),
            state: Mutex::new(State {
                client_id: format!("auth-{}", uuid::Uuid::new_v4()),
                ..Default::default()
            }),
        }
    }

    /// Continue from a saved session (keeps the trust token, so no new code is needed).
    pub fn from_saved(saved: &SavedSession) -> Self {
        let client = Self::new();
        {
            let mut st = client.state.lock().unwrap();
            st.apple_id = saved.apple_id.clone();
            if !saved.client_id.is_empty() {
                st.client_id = saved.client_id.clone();
            }
            st.account_country = saved.account_country.clone();
            st.session_token = saved.session_token.clone();
            st.trust_token = saved.trust_token.clone();
        }
        client
    }

    /// The HTTP agent, which holds the iCloud cookies after sign-in.
    /// Other iCloud services must use it (clones share the cookie jar).
    pub fn agent(&self) -> Agent {
        self.agent.clone()
    }

    pub fn client_id(&self) -> String {
        self.state.lock().unwrap().client_id.clone()
    }

    /// The tokens to persist after a successful sign-in.
    pub fn saved_session(&self) -> SavedSession {
        let st = self.state.lock().unwrap();
        SavedSession {
            apple_id: st.apple_id.clone(),
            client_id: st.client_id.clone(),
            account_country: st.account_country.clone(),
            session_token: st.session_token.clone(),
            trust_token: st.trust_token.clone(),
        }
    }

    /// Sign in again with the saved session token, without a password.
    pub fn resume(&self) -> Result<Account, AuthError> {
        if self.state.lock().unwrap().session_token.is_none() {
            return Err(AuthError::SessionExpired);
        }
        match self.account_login() {
            Ok(SignInStep::Done(account)) => Ok(account),
            Ok(SignInStep::NeedsCode) => Err(AuthError::SessionExpired),
            Err(AuthError::Unexpected { status, .. }) if status == 401 || status == 421 => {
                Err(AuthError::SessionExpired)
            }
            Err(err) => Err(err),
        }
    }

    /// Sign in with an Apple ID and password.
    pub fn sign_in(&self, apple_id: &str, password: &str) -> Result<SignInStep, AuthError> {
        let apple_id = apple_id.trim().to_string();
        self.state.lock().unwrap().apple_id = apple_id.clone();

        // SRP step 1: send our public value, get the salt and server value.
        let srp = SrpClient::new();
        let init_body = json!({
            "a": BASE64.encode(srp.public_key()),
            "accountName": apple_id,
            "protocols": ["s2k", "s2k_fo"],
        });
        let mut resp = self
            .auth_headers(
                self.agent.post(format!("{AUTH_ENDPOINT}/signin/init")),
                true,
                JSON_ACCEPT,
            )
            .send_json(&init_body)?;
        self.capture(&resp);
        match resp.status().as_u16() {
            200 => {}
            400 | 401 | 403 => return Err(AuthError::InvalidCredentials),
            status => return Err(unexpected(status, &mut resp)),
        }
        let init: InitResponse = resp.body_mut().read_json()?;
        let protocol = Protocol::parse(&init.protocol).ok_or_else(|| AuthError::Unexpected {
            status: 200,
            message: format!("unknown password protocol {:?}", init.protocol),
        })?;
        let salt = decode(&init.salt)?;
        let server_public = decode(&init.b)?;

        // SRP step 2: prove we know the password.
        let key = srp::derive_password_key(password, &salt, init.iteration, protocol);
        let proof = srp.process_challenge(&apple_id, &key, &salt, &server_public)?;
        let trust_tokens: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .trust_token
            .iter()
            .cloned()
            .collect();
        let complete_body = json!({
            "accountName": apple_id,
            "c": init.c,
            "m1": BASE64.encode(&proof.m1),
            "m2": BASE64.encode(&proof.m2),
            "rememberMe": true,
            "trustTokens": trust_tokens,
        });
        let mut resp = self
            .auth_headers(
                self.agent
                    .post(format!("{AUTH_ENDPOINT}/signin/complete"))
                    .query("isRememberMeEnabled", "true"),
                true,
                JSON_ACCEPT,
            )
            .send_json(&complete_body)?;
        self.capture(&resp);

        match resp.status().as_u16() {
            200 => self.account_login(),
            409 => {
                // Two-factor needed. Ask Apple to push a code to trusted devices.
                self.request_device_code()?;
                Ok(SignInStep::NeedsCode)
            }
            412 => {
                // Account needs a "repair" acknowledgement (no 2FA on it).
                let mut resp = self
                    .auth_headers(
                        self.agent.post(format!("{AUTH_ENDPOINT}/repair/complete")),
                        false,
                        JSON_ACCEPT,
                    )
                    .send_json(json!({}))?;
                self.capture(&resp);
                if !resp.status().is_success() {
                    return Err(unexpected(resp.status().as_u16(), &mut resp));
                }
                self.account_login()
            }
            401 => Err(AuthError::InvalidCredentials),
            403 => Err(AuthError::Locked),
            status => Err(unexpected(status, &mut resp)),
        }
    }

    /// Ask Apple to show a code on the user's trusted devices.
    pub fn request_device_code(&self) -> Result<(), AuthError> {
        let resp = self
            .auth_headers(
                self.agent
                    .put(format!("{AUTH_ENDPOINT}/verify/trusteddevice/securitycode")),
                false,
                "application/json",
            )
            .send_empty()?;
        self.capture(&resp);
        // Older accounts get the push automatically and may reject this; that's fine.
        Ok(())
    }

    /// Submit a code shown on a trusted device.
    pub fn submit_device_code(&self, code: &str) -> Result<Account, AuthError> {
        let mut resp = self
            .auth_headers(
                self.agent
                    .post(format!("{AUTH_ENDPOINT}/verify/trusteddevice/securitycode")),
                false,
                "application/json",
            )
            .send_json(json!({ "securityCode": { "code": code.trim() } }))?;
        self.capture(&resp);
        check_code_response(&mut resp)?;
        self.trust_and_login()
    }

    /// Phone numbers that can get a code by text message.
    pub fn trusted_phones(&self) -> Result<Vec<TrustedPhone>, AuthError> {
        let mut resp = self
            .auth_headers(self.agent.get(AUTH_ENDPOINT), false, "text/html")
            .call()?;
        self.capture(&resp);
        if !resp.status().is_success() {
            return Err(unexpected(resp.status().as_u16(), &mut resp));
        }
        let html = resp.body_mut().read_to_string()?;
        Ok(parse_trusted_phones(&html))
    }

    /// Text a code to a trusted phone number.
    pub fn request_sms_code(&self, phone_id: u64) -> Result<(), AuthError> {
        let mut resp = self
            .auth_headers(
                self.agent.put(format!("{AUTH_ENDPOINT}/verify/phone")),
                false,
                JSON_ACCEPT,
            )
            .send_json(json!({ "phoneNumber": { "id": phone_id }, "mode": "sms" }))?;
        self.capture(&resp);
        if !resp.status().is_success() {
            return Err(unexpected(resp.status().as_u16(), &mut resp));
        }
        Ok(())
    }

    /// Submit a code received by text message.
    pub fn submit_sms_code(&self, phone_id: u64, code: &str) -> Result<Account, AuthError> {
        let mut resp = self
            .auth_headers(
                self.agent
                    .post(format!("{AUTH_ENDPOINT}/verify/phone/securitycode")),
                false,
                "application/json",
            )
            .send_json(json!({
                "phoneNumber": { "id": phone_id },
                "securityCode": { "code": code.trim() },
                "mode": "sms",
            }))?;
        self.capture(&resp);
        check_code_response(&mut resp)?;
        self.trust_and_login()
    }

    /// Mark this machine as trusted, then finish signing in to iCloud.
    fn trust_and_login(&self) -> Result<Account, AuthError> {
        let resp = self
            .auth_headers(
                self.agent.get(format!("{AUTH_ENDPOINT}/2sv/trust")),
                false,
                JSON_ACCEPT,
            )
            .call()?;
        self.capture(&resp);
        match self.account_login()? {
            SignInStep::Done(account) => Ok(account),
            SignInStep::NeedsCode => Err(AuthError::Unexpected {
                status: 200,
                message: "Apple still wants a verification code after it was accepted".into(),
            }),
        }
    }

    /// Exchange the Apple ID session token for an iCloud web session.
    fn account_login(&self) -> Result<SignInStep, AuthError> {
        let (body, apple_id) = {
            let st = self.state.lock().unwrap();
            (
                json!({
                    "accountCountryCode": st.account_country,
                    "dsWebAuthToken": st.session_token,
                    "extended_login": true,
                    "trustToken": st.trust_token.clone().unwrap_or_default(),
                }),
                st.apple_id.clone(),
            )
        };
        let mut resp = self
            .agent
            .post(format!("{SETUP_ENDPOINT}/accountLogin"))
            .header("Origin", HOME_ENDPOINT)
            .header("Referer", format!("{HOME_ENDPOINT}/"))
            .send_json(&body)?;
        self.capture(&resp);
        if !resp.status().is_success() {
            return Err(unexpected(resp.status().as_u16(), &mut resp));
        }
        let data: Value = resp.body_mut().read_json()?;
        parse_account_login(&data, apple_id)
    }

    /// Headers every `idmsa.apple.com` request needs.
    fn auth_headers<B>(
        &self,
        req: ureq::RequestBuilder<B>,
        origin: bool,
        accept: &str,
    ) -> ureq::RequestBuilder<B> {
        let st = self.state.lock().unwrap();
        let mut req = req
            .header("Accept", accept)
            .header("Content-Type", "application/json")
            .header("X-Apple-OAuth-Client-Id", WIDGET_KEY)
            .header("X-Apple-OAuth-Client-Type", "firstPartyAuth")
            .header("X-Apple-OAuth-Redirect-URI", HOME_ENDPOINT)
            .header("X-Apple-OAuth-Require-Grant-Code", "true")
            .header("X-Apple-OAuth-Response-Mode", "web_message")
            .header("X-Apple-OAuth-Response-Type", "code")
            .header("X-Apple-OAuth-State", st.client_id.as_str())
            .header("X-Apple-Widget-Key", WIDGET_KEY);
        if origin {
            req = req
                .header("Origin", AUTH_ROOT)
                .header("Referer", format!("{AUTH_ROOT}/"));
        } else {
            req = req
                .header("Origin", HOME_ENDPOINT)
                .header("Referer", format!("{HOME_ENDPOINT}/"));
        }
        if let Some(scnt) = &st.scnt {
            req = req.header("scnt", scnt.as_str());
        }
        if let Some(id) = &st.session_id {
            req = req.header("X-Apple-ID-Session-Id", id.as_str());
        }
        req
    }

    /// Keep the session headers Apple sends back.
    fn capture<B>(&self, resp: &Response<B>) {
        let headers = resp.headers();
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let mut st = self.state.lock().unwrap();
        if let Some(v) = get("scnt") {
            st.scnt = Some(v);
        }
        if let Some(v) = get("X-Apple-ID-Session-Id") {
            st.session_id = Some(v);
        }
        if let Some(v) = get("X-Apple-Session-Token") {
            st.session_token = Some(v);
        }
        if let Some(v) = get("X-Apple-TwoSV-Trust-Token") {
            st.trust_token = Some(v);
        }
        if let Some(v) = get("X-Apple-ID-Account-Country") {
            st.account_country = Some(v);
        }
    }
}

fn decode(value: &str) -> Result<Vec<u8>, AuthError> {
    BASE64.decode(value).map_err(|_| AuthError::Unexpected {
        status: 200,
        message: "malformed sign-in challenge".into(),
    })
}

fn unexpected(status: u16, resp: &mut Response<ureq::Body>) -> AuthError {
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    AuthError::Unexpected {
        status,
        message: error_message(&body).unwrap_or_else(|| "no details".into()),
    }
}

/// Pull a readable message out of Apple's error JSON, if there is one.
fn error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let from_list = |key: &str| {
        v.get(key)?
            .as_array()?
            .first()?
            .get("message")?
            .as_str()
            .map(str::to_string)
    };
    from_list("serviceErrors")
        .or_else(|| from_list("service_errors"))
        .or_else(|| v.get("errorMessage")?.as_str().map(str::to_string))
        .or_else(|| v.get("error")?.as_str().map(str::to_string))
        .or_else(|| v.get("reason")?.as_str().map(str::to_string))
}

fn check_code_response(resp: &mut Response<ureq::Body>) -> Result<(), AuthError> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    if status == 400 || is_wrong_code(&body) {
        return Err(AuthError::InvalidCode);
    }
    Err(AuthError::Unexpected {
        status,
        message: error_message(&body).unwrap_or_else(|| "no details".into()),
    })
}

fn is_wrong_code(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let code = |e: &Value| {
        e.get("code")
            .and_then(|c| c.as_i64().or_else(|| c.as_str()?.parse().ok()))
    };
    ["serviceErrors", "service_errors"].iter().any(|key| {
        v.get(*key)
            .and_then(Value::as_array)
            .is_some_and(|errs| errs.iter().any(|e| code(e) == Some(WRONG_CODE)))
    })
}

fn parse_account_login(data: &Value, apple_id: String) -> Result<SignInStep, AuthError> {
    if let Some(domain) = data.get("domainToUse").and_then(Value::as_str) {
        return Err(AuthError::WrongDomain(domain.to_string()));
    }
    let ds = data.get("dsInfo").cloned().unwrap_or(Value::Null);
    let hsa_version = ds.get("hsaVersion").and_then(Value::as_i64).unwrap_or(0);
    let challenge = data
        .get("hsaChallengeRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let trusted = data
        .get("hsaTrustedBrowser")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if hsa_version >= 1 && (challenge || !trusted) {
        return Ok(SignInStep::NeedsCode);
    }

    let text = |key: &str| {
        ds.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let webservices = data
        .get("webservices")
        .and_then(Value::as_object)
        .map(|services| {
            services
                .iter()
                .filter_map(|(name, svc)| {
                    Some((name.clone(), svc.get("url")?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let full_name = match text("fullName") {
        name if !name.is_empty() => name,
        _ => format!("{} {}", text("firstName"), text("lastName"))
            .trim()
            .to_string(),
    };
    Ok(SignInStep::Done(Account {
        apple_id: if apple_id.is_empty() {
            text("appleId")
        } else {
            apple_id
        },
        dsid: text("dsid"),
        full_name,
        webservices,
    }))
}

/// Trusted phone numbers are in a JSON `<script class="boot_args">` in the HTML.
fn parse_trusted_phones(html: &str) -> Vec<TrustedPhone> {
    let Some(json) = boot_args(html) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let two_sv = &v["direct"]["twoSV"];
    let mut numbers = &two_sv["phoneNumberVerification"]["trustedPhoneNumbers"];
    if !numbers.is_array() {
        numbers = &two_sv["bridgeInitiateData"]["phoneNumberVerification"]["trustedPhoneNumbers"];
    }
    numbers
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    Some(TrustedPhone {
                        id: p.get("id")?.as_u64()?,
                        number: p.get("obfuscatedNumber")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn boot_args(html: &str) -> Option<&str> {
    let marker = html.find("class=\"boot_args\"")?;
    let tag_start = html[..marker].rfind("<script")?;
    let content_start = tag_start + html[tag_start..].find('>')? + 1;
    let content_end = content_start + html[content_start..].find("</script>")?;
    Some(html[content_start..content_end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to Apple. Run with `cargo test -p fuji-icloud -- --ignored`.
    /// Uses a made-up address; checks Apple accepts our headers and SRP value.
    #[test]
    #[ignore]
    fn live_signin_init_is_accepted() {
        let client = AuthClient::new();
        let srp = SrpClient::new();
        let mut resp = client
            .auth_headers(
                client.agent.post(format!("{AUTH_ENDPOINT}/signin/init")),
                true,
                JSON_ACCEPT,
            )
            .send_json(json!({
                "a": BASE64.encode(srp.public_key()),
                "accountName": "fuji-connectivity-check@example.com",
                "protocols": ["s2k", "s2k_fo"],
            }))
            .expect("request sent");
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        println!("status {status}: {body}");
        assert_eq!(status, 200, "Apple rejected signin/init: {body}");
        let init: InitResponse = serde_json::from_str(&body).expect("init response shape");
        assert!(Protocol::parse(&init.protocol).is_some());
        assert!(!decode(&init.salt).unwrap().is_empty());
        assert!(!decode(&init.b).unwrap().is_empty());
    }

    #[test]
    fn parses_trusted_phones_from_boot_args() {
        let html = r#"<html><head>
            <script type="application/json" class="boot_args">
            {"direct":{"twoSV":{"phoneNumberVerification":{"trustedPhoneNumbers":[
                {"id":1,"obfuscatedNumber":"(•••) •••-••12","pushMode":"sms"},
                {"id":2,"obfuscatedNumber":"+44 ••••• ••••34"}
            ]}}}}
            </script></head></html>"#;
        let phones = parse_trusted_phones(html);
        assert_eq!(phones.len(), 2);
        assert_eq!(phones[0].id, 1);
        assert_eq!(phones[1].number, "+44 ••••• ••••34");
    }

    #[test]
    fn missing_boot_args_gives_no_phones() {
        assert!(parse_trusted_phones("<html></html>").is_empty());
    }

    #[test]
    fn account_login_needing_code() {
        let data = json!({
            "dsInfo": { "hsaVersion": 2, "dsid": "1" },
            "hsaChallengeRequired": true,
            "hsaTrustedBrowser": false
        });
        assert!(matches!(
            parse_account_login(&data, "a@b.c".into()),
            Ok(SignInStep::NeedsCode)
        ));
    }

    #[test]
    fn account_login_done() {
        let data = json!({
            "dsInfo": { "hsaVersion": 2, "dsid": "42", "firstName": "Ada", "lastName": "L" },
            "hsaChallengeRequired": false,
            "hsaTrustedBrowser": true,
            "webservices": { "ckdatabasews": { "url": "https://p01-ckdatabasews.icloud.com:443", "status": "active" } }
        });
        let Ok(SignInStep::Done(account)) = parse_account_login(&data, "a@b.c".into()) else {
            panic!("expected a signed-in account");
        };
        assert_eq!(account.dsid, "42");
        assert_eq!(account.full_name, "Ada L");
        assert_eq!(
            account.webservices["ckdatabasews"],
            "https://p01-ckdatabasews.icloud.com:443"
        );
    }

    #[test]
    fn china_domain_is_reported() {
        let data = json!({ "domainToUse": "iCloud.com.cn" });
        assert!(matches!(
            parse_account_login(&data, String::new()),
            Err(AuthError::WrongDomain(_))
        ));
    }

    #[test]
    fn recognizes_wrong_code_error() {
        assert!(is_wrong_code(
            r#"{"service_errors":[{"code":"-21669","message":"Incorrect"}]}"#
        ));
        assert!(is_wrong_code(r#"{"serviceErrors":[{"code":-21669}]}"#));
        assert!(!is_wrong_code(r#"{"serviceErrors":[{"code":"-20101"}]}"#));
    }

    #[test]
    fn reads_error_messages() {
        assert_eq!(
            error_message(r#"{"serviceErrors":[{"message":"Your account is locked"}]}"#).as_deref(),
            Some("Your account is locked")
        );
        assert_eq!(error_message("not json"), None);
    }
}
