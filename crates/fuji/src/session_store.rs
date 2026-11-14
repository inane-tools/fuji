//! Keeps the Apple ID session in the OS keychain: Windows Credential Manager,
//! or the Secret Service (GNOME Keyring, KWallet) on Linux.
//!
//! Each value is its own entry, because Windows limits one credential to
//! 2560 bytes and Apple's tokens are long. The password is never stored.

use fuji_icloud::SavedSession;
use keyring::Entry;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "fuji";
const ACCOUNT: &str = "account";
const SESSION_TOKEN: &str = "session-token";
const TRUST_TOKEN: &str = "trust-token";

#[derive(Serialize, Deserialize)]
struct AccountInfo {
    apple_id: String,
    client_id: String,
    account_country: Option<String>,
}

fn entry(name: &str) -> Option<Entry> {
    Entry::new(SERVICE, name).ok()
}

fn read(name: &str) -> Option<String> {
    entry(name)?.get_password().ok()
}

fn write(name: &str, value: Option<&str>) {
    let Some(entry) = entry(name) else { return };
    let result = match value {
        Some(value) => entry.set_password(value),
        None => match entry.delete_credential() {
            Err(keyring::Error::NoEntry) => Ok(()),
            other => other,
        },
    };
    if let Err(err) = result {
        eprintln!("fuji: couldn't update keychain entry {name}: {err}");
    }
}

/// The saved session, if there is one.
pub fn load() -> Option<SavedSession> {
    let info: AccountInfo = serde_json::from_str(&read(ACCOUNT)?).ok()?;
    Some(SavedSession {
        apple_id: info.apple_id,
        client_id: info.client_id,
        account_country: info.account_country,
        session_token: read(SESSION_TOKEN),
        trust_token: read(TRUST_TOKEN),
    })
}

pub fn save(session: &SavedSession) {
    let info = AccountInfo {
        apple_id: session.apple_id.clone(),
        client_id: session.client_id.clone(),
        account_country: session.account_country.clone(),
    };
    if let Ok(json) = serde_json::to_string(&info) {
        write(ACCOUNT, Some(&json));
    }
    write(SESSION_TOKEN, session.session_token.as_deref());
    write(TRUST_TOKEN, session.trust_token.as_deref());
}

/// Sign out: drop the session but keep the trust token and Apple ID, so the
/// next sign-in on this machine needs the password but not a new code.
pub fn sign_out() {
    write(SESSION_TOKEN, None);
}
