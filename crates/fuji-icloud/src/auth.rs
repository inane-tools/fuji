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

use crate::cookies::CookieLog;
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
/// icloud.com's current build, as its setup calls name it (both numbers).
const SETUP_CLIENT_BUILD: &str = "2642Build23";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Incorrect Apple ID or password.")]
    InvalidCredentials,
    #[error("That code isn't right. Check it and try again.")]
    InvalidCode,
    #[error("This Apple ID is locked. Unlock it at iforgot.apple.com, then try again.")]
    Locked,
    #[error("Apple requires this account to use {0}, which Fuji doesn't support yet.")]
    WrongDomain(String),
    #[error("Your saved session has expired. Sign in again.")]
    SessionExpired,
    #[error("{0}")]
    Srp(#[from] srp::SrpError),
    #[error("Couldn't reach Apple: {0}")]
    Network(#[from] ureq::Error),
    #[error("Apple returned an error ({status}): {message}")]
    Unexpected { status: u16, message: String },
    /// Approval on a device didn't go through; the message says why.
    #[error("{0}")]
    Bridge(String),
    /// Access to Advanced Data Protection data wasn't given; the message says why.
    #[error("{0}")]
    DataAccess(String),
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
    /// Services whose data is end-to-end encrypted (Advanced Data
    /// Protection): the web session needs the user's approval first.
    pub pcs_required: Vec<String>,
    /// The alternate account ID (`dsInfo.aDsID`, like `000123-05-<uuid>`),
    /// which on-demand 2FA wraps its token with.
    pub adsid: Option<String>,
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
    /// While a second factor is asked for again from a signed-in session
    /// ("on-demand 2FA"): the token icloud.com gets for it, sent as
    /// `X-Apple-Identity-Token`, and the `X-Apple-2FA-App` Apple hands back.
    identity_token: Option<String>,
    two_fa_app: Option<String>,
    /// The sign-in page's `twoFactorAuthAppKey` and `twoFactorAuthEnv`, which
    /// the on-demand two-factor page is asked for with (`?tfaak=&ev=`).
    two_fa_app_key: Option<(String, String)>,
    /// From the widget page, sent back while on-demand as the widget does:
    /// `X-Apple-Auth-Attributes` (latest from Apple) and `X-Apple-Domain-Id`.
    auth_attributes: Option<String>,
    domain_id: Option<String>,
}

pub struct AuthClient {
    agent: Agent,
    state: Mutex<State>,
    cookies: CookieLog,
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
            cookies: CookieLog::default(),
        }
    }

    /// iCloud's cookies, for saving (encrypted) between runs.
    pub fn export_cookies(&self) -> String {
        self.cookies.export()
    }

    /// Restore cookies saved by [`Self::export_cookies`]. Returns how many loaded.
    pub fn import_cookies(&self, json: &str) -> usize {
        self.cookies.import(json, &self.agent)
    }

    /// Shared with other iCloud clients so their responses' cookies are kept too.
    pub(crate) fn cookie_log(&self) -> CookieLog {
        self.cookies.clone()
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
    ///
    /// Tries the saved iCloud web session first (`/validate`, using the
    /// restored cookies), which Apple doesn't treat as a new sign-in. Only if
    /// that has expired does it log in to iCloud again with the session token.
    pub fn resume(&self) -> Result<Account, AuthError> {
        if !self.cookies.is_empty() {
            match self.validate() {
                Ok(SignInStep::Done(account)) => {
                    debug("resume: continued the saved iCloud session (validate)");
                    return Ok(account);
                }
                Ok(SignInStep::NeedsCode) => debug("resume: validate wants a code"),
                Err(err) => debug(&format!("resume: validate failed: {err}")),
            }
        } else {
            debug("resume: no saved cookies");
        }
        debug("resume: logging in to iCloud again with the session token");
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
        // FUJI_NO_TRUST=1 signs in as if on a new machine, so Apple asks for
        // two-factor (for studying that page).
        let trust_tokens: Vec<String> = if std::env::var_os("FUJI_NO_TRUST").is_some() {
            debug("sign-in: FUJI_NO_TRUST set, not sending the trust token");
            Vec::new()
        } else {
            self.state
                .lock()
                .unwrap()
                .trust_token
                .iter()
                .cloned()
                .collect()
        };
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
        debug(&format!("signin/complete: {}", resp.status().as_u16()));

        match resp.status().as_u16() {
            200 => self.account_login(),
            // Two-factor needed. The caller starts approval on a device
            // (`start_bridge`), or asks for a code (`request_device_code`).
            409 => Ok(SignInStep::NeedsCode),
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
        debug(&format!(
            "verify/trusteddevice/securitycode (new code): {}",
            resp.status().as_u16()
        ));
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

    /// The two-factor page's HTML.
    fn two_factor_html(&self) -> Result<String, AuthError> {
        let mut resp = self
            .auth_headers(self.two_factor_page(), false, "text/html")
            .call()?;
        self.capture(&resp);
        let status = resp.status().as_u16();
        debug(&format!("two-factor page: {status}"));
        if !resp.status().is_success() {
            return Err(unexpected(status, &mut resp));
        }
        Ok(resp.body_mut().read_to_string()?)
    }

    /// The two-factor page's boot data (`boot_args.direct`), if it has any.
    pub(crate) fn two_factor_boot(&self) -> Result<Option<Value>, AuthError> {
        let html = self.two_factor_html()?;
        let boot = boot_args(&html)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .map(|mut v| v["direct"].take());
        if let Some(boot) = &boot {
            debug(&format!(
                "two-factor page: authFactors {}",
                boot["twoSV"]["authFactors"]
            ));
        }
        Ok(boot)
    }

    /// POST JSON to `idmsa.apple.com/appleauth/auth/<path>`; the status and
    /// the body as text, whatever the status.
    pub(crate) fn auth_post_raw(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<(u16, String), AuthError> {
        let mut resp = self
            .auth_headers(
                self.agent.post(format!("{AUTH_ENDPOINT}/{path}")),
                true,
                JSON_ACCEPT,
            )
            .send_json(body)?;
        self.capture(&resp);
        let status = resp.status().as_u16();
        debug(&format!("{path}: {status}"));
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }

    /// Start asking for a second factor again from this signed-in session,
    /// as icloud.com does to pair with the iPhone for search when sign-in
    /// didn't (a security key or a plain code). Until [`Self::end_on_demand`],
    /// the two-factor calls (`bridge_config`, `start_bridge`, ...) run in
    /// that context.
    pub fn begin_on_demand(&self, account: &Account) -> Result<(), AuthError> {
        let dsid = account.dsid.as_str();
        let adsid = account
            .adsid
            .as_deref()
            .ok_or_else(|| AuthError::Unexpected {
                status: 200,
                message: "iCloud didn't give the account's alternate ID".into(),
            })?;
        let client_id = self.client_id();
        let mut resp = self
            .agent
            .post(format!("{SETUP_ENDPOINT}/fetchOnDemandWsToken"))
            .query("clientBuildNumber", SETUP_CLIENT_BUILD)
            .query("clientMasteringNumber", SETUP_CLIENT_BUILD)
            .query("clientId", &client_id)
            .query("dsid", dsid)
            .header("Origin", HOME_ENDPOINT)
            .header("Referer", format!("{HOME_ENDPOINT}/"))
            .send_empty()?;
        self.capture(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        debug(&format!(
            "fetchOnDemandWsToken: {status} success={} error={}",
            v["success"], v["error"]
        ));
        let token = v["onDemand2FAToken"]
            .as_str()
            .filter(|t| !t.is_empty() && v["success"] == true)
            .ok_or_else(|| AuthError::Unexpected {
                status,
                message: v["error"]
                    .as_str()
                    .unwrap_or("Apple wouldn't start pairing")
                    .to_string(),
            })?;
        // icloud.com sends the token wrapped with the account's alternate
        // ID: base64("<aDsID>:<token>").
        let token = BASE64.encode(format!("{adsid}:{token}"));
        let page = self.two_fa_app_key()?;
        debug(&format!(
            "on-demand: widget page has app key {}, auth attributes {}",
            page.app_key.is_some(),
            page.auth_attributes.is_some(),
        ));
        let mut st = self.state.lock().unwrap();
        st.identity_token = Some(token);
        st.two_fa_app = None;
        st.two_fa_app_key = page.app_key;
        st.domain_id = page.domain_id;
        // The widget's replayed headers start empty: the page's
        // `authAttributes`, and no sign-in session yet (Apple's answers
        // fill those in, and later requests send them back).
        st.auth_attributes = page.auth_attributes;
        st.scnt = None;
        st.session_id = None;
        Ok(())
    }

    /// The two-factor page: in the on-demand context, with the app key the
    /// widget passes.
    fn two_factor_page(&self) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        let st = self.state.lock().unwrap();
        let req = self.agent.get(AUTH_ENDPOINT);
        match (&st.identity_token, &st.two_fa_app_key) {
            (Some(_), Some((key, env))) => req.query("tfaak", key).query("ev", env),
            _ => req,
        }
    }

    /// `twoFactorAuthAppKey` and `twoFactorAuthEnv` from the sign-in
    /// widget's page, as the widget has them when it starts (and its session
    /// headers, kept by `capture`).
    fn two_fa_app_key(&self) -> Result<WidgetPage, AuthError> {
        let client_id = self.client_id();
        // The page the widget loads for on-demand 2FA (`odc=1`): plain
        // `auth/signin`, as first-party OAuth is off then.
        let mut resp = self
            .agent
            .get(format!("{AUTH_ENDPOINT}/signin"))
            .query("widgetKey", WIDGET_KEY)
            .query("language", "en_US")
            .query("skVersion", "7")
            .query("odc", "1")
            .query("iframeId", &client_id)
            .header("Referer", format!("{HOME_ENDPOINT}/"))
            .call()?;
        debug(&format!(
            "on-demand widget page: {}",
            resp.status().as_u16()
        ));
        // Its cookies apply, but not its headers: the widget's script can't
        // see a page load's headers, so it starts from the page's own data.
        self.cookies.record(&resp);
        let html = resp.body_mut().read_to_string().unwrap_or_default();
        Ok(parse_widget_page(&html))
    }

    /// Leave the on-demand context.
    pub fn end_on_demand(&self) {
        let mut st = self.state.lock().unwrap();
        st.identity_token = None;
        st.two_fa_app = None;
        st.two_fa_app_key = None;
        st.auth_attributes = None;
        st.domain_id = None;
    }

    /// A call to `setup.icloud.com/setup/ws/1/<path>` the way icloud.com
    /// makes it: client numbers and `dsid` in the query, a JSON body sent as
    /// text. The status and the JSON answer (`null` if none).
    pub(crate) fn setup_call(
        &self,
        path: &str,
        dsid: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), AuthError> {
        let client_id = self.client_id();
        let query = [
            ("clientBuildNumber", SETUP_CLIENT_BUILD),
            ("clientMasteringNumber", SETUP_CLIENT_BUILD),
            ("clientId", client_id.as_str()),
            ("dsid", dsid),
        ];
        let url = format!("{SETUP_ENDPOINT}/{path}");
        let mut resp = match body {
            Some(body) => {
                let mut req = self.agent.post(&url);
                for (k, v) in query {
                    req = req.query(k, v);
                }
                req.header("Origin", HOME_ENDPOINT)
                    .header("Referer", format!("{HOME_ENDPOINT}/"))
                    .content_type("text/plain;charset=UTF-8")
                    .send(body.to_string())?
            }
            None => {
                let mut req = self.agent.get(&url);
                for (k, v) in query {
                    req = req.query(k, v);
                }
                req.header("Origin", HOME_ENDPOINT)
                    .header("Referer", format!("{HOME_ENDPOINT}/"))
                    .call()?
            }
        };
        self.capture(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// Whether the session holds an unexpired cookie with this name.
    pub(crate) fn has_cookie(&self, name: &str) -> bool {
        let jar = self.agent.cookie_jar_lock();
        let found = jar
            .iter()
            .any(|c| c.name() == name && !c.value().is_empty());
        jar.release();
        found
    }

    /// After a second factor was accepted (a security key): trust this
    /// machine and log in to iCloud.
    pub(crate) fn after_second_factor(&self) -> Result<Account, AuthError> {
        self.trust_and_login()
    }

    /// The two-factor page's settings for approval on a device ("bridge"),
    /// if Apple offers it for this sign-in.
    pub fn bridge_config(&self) -> Result<Option<BridgeConfig>, AuthError> {
        let html = self.two_factor_html()?;
        Ok(parse_bridge_config(&html))
    }

    /// Phone numbers that can get a code by text message.
    pub fn trusted_phones(&self) -> Result<Vec<TrustedPhone>, AuthError> {
        let html = self.two_factor_html()?;
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

    pub(crate) fn set_apple_id(&self, apple_id: &str) {
        self.state.lock().unwrap().apple_id = apple_id.to_string();
    }

    /// POST JSON to `idmsa.apple.com/appleauth/auth/<path>` and read the JSON
    /// answer (`null` if there's no body).
    pub(crate) fn auth_post(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value, AuthError> {
        let mut req = self.agent.post(format!("{AUTH_ENDPOINT}/{path}"));
        for (key, value) in query {
            req = req.query(*key, *value);
        }
        let mut resp = self.auth_headers(req, true, JSON_ACCEPT).send_json(body)?;
        self.capture(&resp);
        let status = resp.status().as_u16();
        debug(&format!("{path}: {status}"));
        match status {
            200..=299 => {
                let text = resp.body_mut().read_to_string().unwrap_or_default();
                Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
            }
            401 => Err(AuthError::InvalidCredentials),
            403 => Err(AuthError::Locked),
            status => Err(unexpected(status, &mut resp)),
        }
    }

    /// POST JSON to `idmsa.apple.com/appleauth/auth/<path>`; only the status.
    pub(crate) fn bridge_post_status(&self, path: &str, body: &Value) -> Result<u16, AuthError> {
        // The widget leaves the on-demand token off the closing "done" post.
        let done = body.get("data").and_then(Value::as_str) == Some("ZG9uZQ==");
        let mut resp = self
            .auth_headers_with(
                self.agent.post(format!("{AUTH_ENDPOINT}/{path}")),
                true,
                JSON_ACCEPT,
                !done,
            )
            .send_json(body)?;
        self.capture(&resp);
        let status = resp.status().as_u16();
        debug(&format!("{path}: {status}"));
        if status == 401 {
            return Err(AuthError::SessionExpired);
        }
        let _ = resp.body_mut().read_to_string();
        Ok(status)
    }

    /// After approval on a device: trust this machine and log in to iCloud,
    /// as after a typed code.
    pub(crate) fn after_bridge(&self) -> Result<SignInStep, AuthError> {
        self.trust_and_login().map(SignInStep::Done)
    }

    /// After a passkey sign-in: a passkey is two factors in one, so trust
    /// this machine and log in to iCloud.
    pub(crate) fn after_passkey(&self) -> Result<SignInStep, AuthError> {
        let resp = self
            .auth_headers(
                self.agent.get(format!("{AUTH_ENDPOINT}/2sv/trust")),
                false,
                JSON_ACCEPT,
            )
            .call()?;
        self.capture(&resp);
        debug(&format!("passkey: trust {}", resp.status().as_u16()));
        self.account_login()
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

    /// Check the current iCloud web session (cookies) and get the account.
    fn validate(&self) -> Result<SignInStep, AuthError> {
        let apple_id = self.state.lock().unwrap().apple_id.clone();
        let mut resp = self
            .agent
            .post(format!("{SETUP_ENDPOINT}/validate"))
            .header("Origin", HOME_ENDPOINT)
            .header("Referer", format!("{HOME_ENDPOINT}/"))
            .content_type("text/plain")
            .send("null")?;
        self.capture(&resp);
        if !resp.status().is_success() {
            return Err(unexpected(resp.status().as_u16(), &mut resp));
        }
        let data: Value = resp.body_mut().read_json()?;
        parse_account_login(&data, apple_id)
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
        self.auth_headers_with(req, origin, accept, true)
    }

    /// [`Self::auth_headers`]; `identity` false leaves out the on-demand
    /// identity token (the widget does for the closing "done" bridge posts).
    fn auth_headers_with<B>(
        &self,
        req: ureq::RequestBuilder<B>,
        origin: bool,
        accept: &str,
        identity: bool,
    ) -> ureq::RequestBuilder<B> {
        let st = self.state.lock().unwrap();
        let mut req = req
            .header("Accept", accept)
            .header("Content-Type", "application/json")
            .header("X-Apple-Widget-Key", WIDGET_KEY);
        if st.identity_token.is_none() {
            // Sign-in runs as first-party OAuth; the widget turns that off
            // for on-demand 2FA, and sends none of these then.
            req = req
                .header("X-Apple-OAuth-Client-Id", WIDGET_KEY)
                .header("X-Apple-OAuth-Client-Type", "firstPartyAuth")
                .header("X-Apple-OAuth-Redirect-URI", HOME_ENDPOINT)
                .header("X-Apple-OAuth-Require-Grant-Code", "true")
                .header("X-Apple-OAuth-Response-Mode", "web_message")
                .header("X-Apple-OAuth-Response-Type", "code")
                .header("X-Apple-OAuth-State", st.client_id.as_str());
        } else {
            // icloud.com's widget uses sign-in version 2.
            req = req
                .header("X-Apple-Privacy-Consent", "true")
                .header("X-Apple-Privacy-Consent-Accepted", "true");
        }
        if st.identity_token.is_some() {
            // On-demand 2FA: as the widget inside Apple's page sends them, the
            // page as referrer, Origin only on posts, and the session headers
            // only once Apple's answers have given them.
            let referer = format!(
                "{AUTH_ENDPOINT}/signin?widgetKey={WIDGET_KEY}&language=en_US&skVersion=7&odc=1&iframeId={}",
                st.client_id
            );
            if origin {
                req = req.header("Origin", AUTH_ROOT);
            }
            // Replayed from Apple's answers since the widget started.
            if let Some(scnt) = &st.scnt {
                req = req.header("scnt", scnt.as_str());
            }
            if let Some(id) = &st.session_id {
                req = req.header("X-Apple-ID-Session-Id", id.as_str());
            }
            req = req
                .header("Referer", referer)
                .header("X-Requested-With", "XMLHttpRequest")
                .header("Accept-Language", "en-US,en;q=0.9")
                // A request from Apple's own page, as the browser marks it.
                .header("Sec-Fetch-Site", "same-origin")
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Dest", "empty")
                .header("X-Apple-Locale", "en_US")
                .header("X-Apple-Offer-Security-Upgrade", "1")
                .header("X-Apple-I-FD-Client-Info", client_info());
        } else {
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
        }
        if st.identity_token.is_some() {
            req = req.header("X-Apple-Frame-Id", st.client_id.as_str());
            if let Some(v) = &st.auth_attributes {
                req = req.header("X-Apple-Auth-Attributes", v.as_str());
            }
            if let Some(v) = &st.domain_id {
                req = req.header("X-Apple-Domain-Id", v.as_str());
            }
        }
        if identity && let Some(token) = &st.identity_token {
            req = req.header("X-Apple-Identity-Token", token.as_str());
            if let Some(app) = &st.two_fa_app {
                req = req.header("X-Apple-2FA-App", app.as_str());
            }
        }
        req
    }

    /// Keep the session headers Apple sends back.
    fn capture(&self, resp: &Response<ureq::Body>) {
        self.cookies.record(resp);
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
        if st.identity_token.is_some()
            && let Some(v) = get("X-Apple-2FA-App")
        {
            st.two_fa_app = Some(v);
        }
        if let Some(v) = get("X-Apple-Auth-Attributes") {
            st.auth_attributes = Some(v);
        }
    }
}

/// Diagnostics on stderr when `FUJI_DEBUG` is set. Never logs secrets.
fn debug(message: &str) {
    if std::env::var_os("FUJI_DEBUG").is_some() {
        eprintln!("fuji-icloud: {message}");
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
pub(crate) fn error_message(body: &str) -> Option<String> {
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

    let pcs_required = data
        .get("webservices")
        .and_then(Value::as_object)
        .map(|services| {
            services
                .iter()
                .filter(|(_, svc)| svc.get("pcsRequired").and_then(Value::as_bool) == Some(true))
                .map(|(name, _)| name.clone())
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
        adsid: Some(text("aDsID")).filter(|a| !a.is_empty()),
        full_name,
        webservices,
        pcs_required,
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

/// What the two-factor page says about Apple's newer approval ("bridge"):
/// the scripts the widget loads for it and the push channel it listens on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeConfig {
    /// The push client script (`AppleID.service.bridge.APNS`).
    pub push_script: Option<String>,
    /// The key exchange worker script.
    pub prover_script: Option<String>,
    pub apns_environment: Option<String>,
    pub apns_topic: Option<String>,
}

/// A script URL from a value that may be a whole `<script src=...>` tag.
fn script_src(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match value.find("src=") {
        Some(i) => {
            let rest = &value[i + 4..];
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = rest[1..].find(quote)?;
                Some(rest[1..1 + end].to_string())
            } else {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(rest.len());
                Some(rest[..end].to_string())
            }
        }
        None => Some(value.to_string()),
    }
}

fn parse_bridge_config(html: &str) -> Option<BridgeConfig> {
    let v: Value = serde_json::from_str(boot_args(html)?).ok()?;
    let boot = &v["direct"];
    let initiate = &boot["twoSV"]["bridgeInitiateData"];
    let text = |v: &Value| v.as_str().map(str::to_string).filter(|s| !s.is_empty());
    let config = BridgeConfig {
        push_script: boot["bridge"]["bridgeScriptTag"]
            .as_str()
            .and_then(script_src),
        prover_script: boot["bridge"]["proverWorkerScriptTag"]
            .as_str()
            .and_then(script_src),
        apns_environment: text(&initiate["apnsEnvironment"]),
        apns_topic: text(&initiate["apnsTopic"]),
    };
    if debug_enabled() {
        let keys = |v: &Value| {
            v.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        };
        debug(&format!("bridge: boot keys [{}]", keys(boot)));
        debug(&format!("bridge: bridge keys [{}]", keys(&boot["bridge"])));
        debug(&format!("bridge: twoSV keys [{}]", keys(&boot["twoSV"])));
        debug(&format!("bridge: initiate keys [{}]", keys(initiate)));
        debug(&format!("bridge: {config:?}"));
    }
    (config != BridgeConfig::default()).then_some(config)
}

/// `twoFactorAuthAppKey` and `twoFactorAuthEnv`.
type AppKey = (String, String);

/// What the widget sends as `X-Apple-I-FD-Client-Info`: browser, language,
/// time zone, version, and an empty fingerprint.
fn client_info() -> String {
    json!({
        "U": USER_AGENT,
        "L": "en-US",
        "Z": "GMT+00:00",
        "V": "1.1",
        "F": "",
    })
    .to_string()
}

/// A string or number field (`"name":"…"` or `"name":3`) in the widget page.
fn boot_field(html: &str, name: &str) -> Option<String> {
    let marker = format!("\"{name}\":");
    let rest = html[html.find(&marker)? + marker.len()..].trim_start();
    let value: String = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?.to_string(),
        None => rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect(),
    };
    (!value.is_empty()).then_some(value)
}

/// What the on-demand widget page gives the widget.
#[derive(Debug, Default, PartialEq)]
struct WidgetPage {
    app_key: Option<AppKey>,
    domain_id: Option<String>,
    auth_attributes: Option<String>,
}

/// Every `boot_args` block in a page, parsed.
fn all_boot_args(html: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(marker) = rest.find("class=\"boot_args\"") {
        let Some(open) = rest[marker..].find('>') else {
            break;
        };
        let start = marker + open + 1;
        let Some(len) = rest[start..].find("</script>") else {
            break;
        };
        if let Ok(v) = serde_json::from_str(rest[start..start + len].trim()) {
            out.push(v);
        }
        rest = &rest[start + len..];
    }
    out
}

/// The widget page's boot data: `direct` of the block that has the app key.
fn parse_widget_page(html: &str) -> WidgetPage {
    let text = |v: &Value| match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let direct = all_boot_args(html)
        .into_iter()
        .map(|mut v| v["direct"].take())
        .find(|d| d.get("twoFactorAuthAppKey").is_some())
        .unwrap_or(Value::Null);
    WidgetPage {
        app_key: text(&direct["twoFactorAuthAppKey"]).zip(text(&direct["twoFactorAuthEnv"])),
        domain_id: text(&direct["domainId"]).or_else(|| boot_field(html, "domainId")),
        auth_attributes: text(&direct["authAttributes"]),
    }
}

fn debug_enabled() -> bool {
    std::env::var_os("FUJI_DEBUG").is_some()
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
    fn reads_bridge_config() {
        let html = r#"<script type="application/json" class="boot_args">
            {"direct":{"bridge":{"bridgeScriptTag":"<script src=\"https://cdn.example/b.js\"><\/script>",
            "proverWorkerScriptTag":"https://cdn.example/p.js"},
            "twoSV":{"bridgeInitiateData":{"apnsEnvironment":"PROD","apnsTopic":"com.example.topic"}}}}
            </script>"#;
        let c = parse_bridge_config(html).unwrap();
        assert_eq!(c.push_script.as_deref(), Some("https://cdn.example/b.js"));
        assert_eq!(c.prover_script.as_deref(), Some("https://cdn.example/p.js"));
        assert_eq!(c.apns_environment.as_deref(), Some("PROD"));
        assert_eq!(c.apns_topic.as_deref(), Some("com.example.topic"));
        assert_eq!(parse_bridge_config("<html></html>"), None);
    }

    #[test]
    fn reads_the_widget_page() {
        let html = r#"<script type="application/json" class="boot_args">{"direct":{"x":1}}</script>
            <script type="application/json" class="boot_args">{"direct":{"twoFactorAuthEnv":1,
            "twoFactorAuthAppKey":"AbC+\/=","authAttributes":"attrs"},"additional":{}}</script>
            <script>var c = {"domainId":3};</script>"#;
        let page = parse_widget_page(html);
        assert_eq!(page.app_key, Some(("AbC+/=".to_string(), "1".to_string())));
        assert_eq!(page.auth_attributes.as_deref(), Some("attrs"));
        assert_eq!(page.domain_id.as_deref(), Some("3"));
        assert_eq!(parse_widget_page("<html></html>"), WidgetPage::default());
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
