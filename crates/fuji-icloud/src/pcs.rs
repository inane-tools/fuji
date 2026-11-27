//! Access to end-to-end encrypted data (Advanced Data Protection) from a
//! web session. With ADP on, iCloud gives a web session nothing from a
//! service marked `pcsRequired` until Apple hands it that service's "PCS"
//! cookies, which needs the user's approval on a trusted device.
//!
//! As icloud.com does it, on `setup.icloud.com/setup/ws/1`:
//! 1. `requestWebAccessState`: is web access on, is ADP effective (iCloud
//!    data recovery off), has a device already consented?
//! 2. Without consent, `enableDeviceConsentForPCS` asks the devices; poll
//!    the state until one allows it.
//! 3. `generateSessionIdToken`, then `requestPCS` for the app until Apple
//!    answers `success` and sets the cookies.
//!
//! Every call carries the client numbers in the query and a body with a
//! `workflowUuid`; without them Apple answers 422 with no body. (Shape from
//! icloudpy's ADP work, mandarons/icloudpy#214.)
//!
//! Blocking: call from a background thread.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::auth::{Account, AuthClient, AuthError};

/// How long to wait for the user to allow access on a device.
const APPROVAL_WAIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_secs(4);
/// What Apple says while the device hasn't sent the cookies yet.
const PENDING: [&str; 2] = [
    "Requested the device to upload cookies.",
    "Cookies not available yet on server.",
];

/// The Photos service as `accountLogin` names it, and its cookies.
const PHOTOS_SERVICE: &str = "ckdatabasews";
const PHOTOS_APP: &str = "photos";
const PHOTOS_COOKIE: &str = "X-APPLE-WEBAUTH-PCS-Photos";

fn debug(message: &str) {
    if std::env::var_os("FUJI_DEBUG").is_some() {
        eprintln!("fuji-icloud: pcs: {message}");
    }
}

fn denied(message: impl Into<String>) -> AuthError {
    AuthError::DataAccess(message.into())
}

/// What a step answered, for the log: status, `status` and `message`
/// fields and which flags were present. No identifiers.
fn describe(status: u16, v: &Value) -> String {
    let flag = |k: &str| v.get(k).map_or("-".to_string(), Value::to_string);
    format!(
        "{status} status={} message={} webAccess={} consented={} icdrsDisabled={} sent={}",
        flag("status"),
        flag("message"),
        flag("isWebAccessAllowed"),
        flag("isDeviceConsentedForPCS"),
        flag("isICDRSDisabled"),
        flag("isDeviceConsentNotificationSent"),
    )
}

fn workflow() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

/// What happened, for the caller to tell the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Nothing was needed (no ADP, or the cookies are already here).
    Ready,
    /// Access was given just now.
    Granted,
}

impl AuthClient {
    /// Whether opening Photos needs [`Self::photos_access`] first.
    pub fn needs_photos_access(&self, account: &Account) -> bool {
        account.pcs_required.iter().any(|s| s == PHOTOS_SERVICE) && !self.has_cookie(PHOTOS_COOKIE)
    }

    /// Get access to the Photos library of an ADP account. `on_waiting`
    /// runs once, just before waiting for the user to allow it on a device.
    pub fn photos_access(
        &self,
        account: &Account,
        mut on_waiting: impl FnMut(),
    ) -> Result<Access, AuthError> {
        if !self.needs_photos_access(account) {
            return Ok(Access::Ready);
        }
        let dsid = account.dsid.as_str();
        let flow = workflow();
        let post = |path: &str, body: Value| -> Result<(u16, Value), AuthError> {
            let mut body = body;
            body["workflowUuid"] = json!(flow);
            let (status, v) = self.setup_call(path, dsid, Some(&body))?;
            debug(&format!("{path}: {}", describe(status, &v)));
            Ok((status, v))
        };
        let state = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool);

        let (status, web) = post("requestWebAccessState", json!({}))?;
        if web.is_null() && status >= 400 {
            return Err(denied(format!(
                "Apple refused to check access to your encrypted data ({status})."
            )));
        }
        if state(&web, "isWebAccessAllowed") == Some(false) {
            return Err(denied(
                "Your account uses Advanced Data Protection. To open it in Fuji, turn on \"Access iCloud Data on the Web\" on your iPhone (Settings › your name › iCloud), then sign in again.",
            ));
        }
        if state(&web, "isICDRSDisabled") == Some(false) {
            // Apple still holds a recovery key: no approval needed.
            return Ok(Access::Ready);
        }

        let deadline = Instant::now() + APPROVAL_WAIT;
        let mut waited = false;
        let mut wait = |on_waiting: &mut dyn FnMut()| {
            if !waited {
                waited = true;
                on_waiting();
            }
        };

        if state(&web, "isDeviceConsentedForPCS") != Some(true) {
            wait(&mut on_waiting);
            let (_, sent) = post("enableDeviceConsentForPCS", json!({}))?;
            if state(&sent, "isDeviceConsentNotificationSent") == Some(false) {
                return Err(denied(
                    "Apple didn't ask your devices to allow access. Check that your iPhone is on and signed in, then try again.",
                ));
            }
            loop {
                if Instant::now() >= deadline {
                    return Err(denied(
                        "Access to your photos wasn't allowed in time. Sign in again and allow it on your iPhone.",
                    ));
                }
                std::thread::sleep(POLL);
                let (_, web) = post("requestWebAccessState", json!({}))?;
                if state(&web, "isDeviceConsentedForPCS") == Some(true) {
                    break;
                }
            }
        }

        // Best effort, as the web client does before asking for cookies.
        match self.setup_call("generateSessionIdToken", dsid, None) {
            Ok((status, _)) => debug(&format!("generateSessionIdToken: {status}")),
            Err(err) => debug(&format!("generateSessionIdToken failed: {err}")),
        }

        let mut first = true;
        loop {
            let (status, v) = post(
                "requestPCS",
                json!({
                    "appName": PHOTOS_APP,
                    // Only the first asks the device; repeating re-notifies.
                    "derivedFromUserAction": first,
                    "isFinalAttempt": false,
                }),
            )?;
            first = false;
            let message = v.get("message").and_then(Value::as_str).unwrap_or("");
            if v.get("status").and_then(Value::as_str) == Some("success") {
                if !self.has_cookie(PHOTOS_COOKIE) {
                    debug("success, but no Photos cookie was set");
                }
                return Ok(Access::Granted);
            }
            if status == 412 || message.contains("iCloud DRS") {
                return Ok(Access::Ready);
            }
            if state(&v, "isWebAccessAllowed") == Some(false) {
                return Err(denied(
                    "Turn on \"Access iCloud Data on the Web\" on your iPhone (Settings › your name › iCloud), then sign in again.",
                ));
            }
            if status == 401 || status == 421 {
                return Err(AuthError::SessionExpired);
            }
            if !PENDING.contains(&message) {
                return Err(denied(format!(
                    "Apple didn't give access to your photos ({status}{}).",
                    if message.is_empty() {
                        String::new()
                    } else {
                        format!(": {message}")
                    }
                )));
            }
            wait(&mut on_waiting);
            if Instant::now() >= deadline {
                return Err(denied(
                    "Your iPhone didn't send access to your photos in time. Check that it's on and unlocked, then sign in again.",
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}
