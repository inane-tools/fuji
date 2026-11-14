//! Keeps iCloud's cookies across runs, so a restart continues the existing web
//! session (`/validate`) instead of signing in again. Every fresh sign-in
//! makes Apple email the account holder.
//!
//! The agent's cookie jar can't export session cookies or host-only cookies'
//! hosts, so this records each `Set-Cookie` header with the host it came from
//! and replays them into a new agent on restore.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use ureq::http::Response;
use ureq::{Agent, ResponseExt as _};

#[derive(Clone, Default)]
pub struct CookieLog(Arc<Mutex<BTreeMap<String, Entry>>>);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    host: String,
    set_cookie: String,
}

impl CookieLog {
    /// Remember the cookies a response set.
    pub(crate) fn record(&self, response: &Response<ureq::Body>) {
        let Some(host) = response.get_uri().host().map(str::to_string) else {
            return;
        };
        let mut entries = self.0.lock().unwrap();
        for value in response.headers().get_all("set-cookie") {
            let Ok(raw) = value.to_str() else { continue };
            let Some(name) = raw.split(['=', ';']).next().map(str::trim) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            entries.insert(
                format!("{host}\u{0}{name}"),
                Entry {
                    host: host.clone(),
                    set_cookie: raw.to_string(),
                },
            );
        }
    }

    /// The recorded cookies, as JSON. Contains secrets: store it encrypted.
    pub fn export(&self) -> String {
        let entries: Vec<Entry> = self.0.lock().unwrap().values().cloned().collect();
        serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
    }

    /// Load previously exported cookies into this log and into `agent`.
    /// Expired or malformed cookies are skipped. Returns how many were loaded.
    pub(crate) fn import(&self, json: &str, agent: &Agent) -> usize {
        let Ok(entries) = serde_json::from_str::<Vec<Entry>>(json) else {
            return 0;
        };
        let mut jar = agent.cookie_jar_lock();
        let mut log = self.0.lock().unwrap();
        let mut loaded = 0;
        for entry in entries {
            let Ok(uri) = format!("https://{}/", entry.host).parse::<ureq::http::Uri>() else {
                continue;
            };
            // An owned String makes this a `Cookie<'static>`, as `insert` needs.
            let Ok(cookie) = ureq::Cookie::parse(entry.set_cookie.clone(), &uri) else {
                continue;
            };
            let name = cookie.name().to_string();
            if jar.insert(cookie, &uri).is_ok() {
                loaded += 1;
                log.insert(format!("{}\u{0}{name}", entry.host), entry);
            }
        }
        jar.release();
        loaded
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_import_round_trip() {
        let log = CookieLog::default();
        log.0.lock().unwrap().insert(
            "setup.icloud.com\u{0}X-APPLE-WEBAUTH-TOKEN".into(),
            Entry {
                host: "setup.icloud.com".into(),
                set_cookie: "X-APPLE-WEBAUTH-TOKEN=\"v=2:t=abc\"; Domain=.icloud.com; Path=/; Secure; HttpOnly".into(),
            },
        );
        let json = log.export();

        let agent = Agent::new_with_defaults();
        let restored = CookieLog::default();
        assert_eq!(restored.import(&json, &agent), 1);
        assert_eq!(restored.export(), json);
        let jar = agent.cookie_jar_lock();
        let cookie = jar.get("icloud.com", "/", "X-APPLE-WEBAUTH-TOKEN");
        assert!(cookie.is_some(), "cookie should be usable for *.icloud.com");
    }

    #[test]
    fn bad_input_is_ignored() {
        let agent = Agent::new_with_defaults();
        assert_eq!(CookieLog::default().import("not json", &agent), 0);
        assert_eq!(CookieLog::default().import("[]", &agent), 0);
    }
}
