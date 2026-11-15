//! Keeps the Apple ID session in the OS keychain: Windows Credential Manager,
//! or the Secret Service (GNOME Keyring, KWallet) on Linux.
//!
//! Each value is its own entry, because Windows limits one credential to
//! 2560 bytes and Apple's tokens are long. The password is never stored.
//!
//! iCloud's web cookies are too large for a keychain entry, so they're kept
//! in a file encrypted with AES-256-GCM; only the key is in the keychain.
//! Keeping them lets a restart continue the same iCloud session instead of
//! signing in again (which makes Apple email the account holder).

use std::path::PathBuf;

use aes_gcm::aead::{Aead, Generate, Key, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fuji_icloud::{AuthClient, SavedSession};
use keyring::Entry;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "fuji";
const ACCOUNT: &str = "account";
const SESSION_TOKEN: &str = "session-token";
const TRUST_TOKEN: &str = "trust-token";
const COOKIE_KEY: &str = "cookie-key";
const NONCE_LEN: usize = 12;

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
    clear_cookies();
}

/// Save everything needed to pick the session up again: tokens and cookies.
/// Blocking (keychain and disk); call off the UI thread.
pub fn persist(auth: &AuthClient) {
    save(&auth.saved_session());
    save_cookies(&auth.export_cookies());
}

/// Restore saved cookies into `auth`. Returns how many were loaded.
pub fn restore_cookies(auth: &AuthClient) -> usize {
    load_cookies().map_or(0, |json| auth.import_cookies(&json))
}

fn cookie_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("fuji").join("session.bin"))
}

fn cookie_cipher(create: bool) -> Option<Aes256Gcm> {
    let stored = read(COOKIE_KEY).and_then(|b64| BASE64.decode(b64).ok());
    let bytes = match stored {
        Some(bytes) if bytes.len() == 32 => bytes,
        _ if create => {
            let key = Key::<Aes256Gcm>::generate();
            write(COOKIE_KEY, Some(&BASE64.encode(key.as_slice())));
            key.to_vec()
        }
        _ => return None,
    };
    Aes256Gcm::new_from_slice(&bytes).ok()
}

fn save_cookies(json: &str) {
    let (Some(path), Some(cipher)) = (cookie_file(), cookie_cipher(true)) else {
        return;
    };
    let nonce = Nonce::generate();
    let Ok(ciphertext) = cipher.encrypt(&nonce, json.as_bytes()) else {
        return;
    };
    let mut data = nonce.to_vec();
    data.extend_from_slice(&ciphertext);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn load_cookies() -> Option<String> {
    let data = std::fs::read(cookie_file()?).ok()?;
    if data.len() <= NONCE_LEN {
        return None;
    }
    let (nonce, ciphertext) = data.split_at(NONCE_LEN);
    let nonce = Nonce::try_from(nonce).ok()?;
    let plain = cookie_cipher(false)?.decrypt(&nonce, ciphertext).ok()?;
    String::from_utf8(plain).ok()
}

fn clear_cookies() {
    if let Some(path) = cookie_file() {
        let _ = std::fs::remove_file(path);
    }
    write(COOKIE_KEY, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuji_icloud::PhotosClient;

    /// Reads (never writes) the signed-in library's albums with the saved
    /// session. Run with `cargo test -p fuji live_albums -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_albums() {
        let saved = load().expect("a saved session: sign in to fuji first");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let client = PhotosClient::new(&auth, &account).expect("photos service");
        let albums = client.albums().expect("album list");
        println!("{} albums", albums.len());
        for (n, album) in albums.iter().enumerate() {
            let a = album.album();
            let count = client.count(&a);
            println!("album {n}: count {count:?}");
            if let Ok(count) = count {
                for limit in [count.min(200), 1, 2, 7] {
                    match client.scan(&a, 0, limit) {
                        Ok(items) => {
                            let undated = items.iter().filter(|i| i.created_at_ms == 0).count();
                            println!("  scan {limit}: {} items, {undated} undated", items.len());
                        }
                        Err(err) => println!("  scan {limit}: {err}"),
                    }
                }
                match client.fetch(&a, 0, count.min(100)) {
                    Ok(items) => {
                        let thumbs = items.iter().filter(|i| i.thumbnail.is_some()).count();
                        println!("  fetch: {} items, {thumbs} with thumbnails", items.len());
                    }
                    Err(err) => println!("  fetch: {err}"),
                }
            }
        }
        persist(&auth);
    }
}
