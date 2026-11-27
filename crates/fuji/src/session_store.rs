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

use std::path::{Path, PathBuf};

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
/// The Skybridge pairing key a passkey or two-factor sign-in handed over.
/// Apple's web client keeps it 90 days.
const PAIRING_KEY: &str = "pairing-key";
const PAIRING_DAYS: u64 = 90;
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
/// next sign-in on this machine needs the password but not a new code. The
/// pairing key belongs to the session, so it goes too.
pub fn sign_out() {
    write(SESSION_TOKEN, None);
    write(PAIRING_KEY, None);
    clear_bridge();
    clear_cookies();
}

#[derive(Serialize, Deserialize)]
struct PairingRecord {
    key: String,
    mid: Option<String>,
    /// Seconds since the Unix epoch.
    saved_at: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Keep the pairing key from a passkey sign-in.
pub fn save_pairing(pairing: &fuji_icloud::PairingKey) {
    let record = PairingRecord {
        key: pairing.key.clone(),
        mid: pairing.mid.clone(),
        saved_at: now_secs(),
    };
    if let Ok(json) = serde_json::to_string(&record) {
        write(PAIRING_KEY, Some(&json));
    }
}

/// Forget the pairing with the iPhone (and the device connection). Blocking.
pub fn clear_pairing() {
    write(PAIRING_KEY, None);
    clear_bridge();
}

/// The saved pairing key, unless it has expired.
pub fn load_pairing() -> Option<fuji_icloud::PairingKey> {
    let record: PairingRecord = serde_json::from_str(&read(PAIRING_KEY)?).ok()?;
    if now_secs().saturating_sub(record.saved_at) > PAIRING_DAYS * 24 * 3600 {
        write(PAIRING_KEY, None);
        return None;
    }
    Some(fuji_icloud::PairingKey {
        key: record.key,
        mid: record.mid,
    })
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

/// The device search connection, kept between runs (private keys, so
/// encrypted like the cookies).
fn bridge_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("fuji").join("device.bin"))
}

/// Keep the device search connection for the next run. Blocking.
pub fn save_bridge(saved: &fuji_icloud::skybridge::SavedBridge) {
    if let (Some(path), Ok(json)) = (bridge_file(), serde_json::to_string(saved)) {
        write_sealed(&path, &json);
    }
}

/// The device search connection from an earlier run, if any. Blocking.
pub fn load_bridge() -> Option<fuji_icloud::skybridge::SavedBridge> {
    serde_json::from_str(&read_sealed(&bridge_file()?)?).ok()
}

pub fn clear_bridge() {
    if let Some(path) = bridge_file() {
        let _ = std::fs::remove_file(path);
    }
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
    if let Some(path) = cookie_file() {
        write_sealed(&path, json);
    }
}

/// Write `text` to `path`, encrypted with the key kept in the keychain.
fn write_sealed(path: &Path, text: &str) {
    let Some(cipher) = cookie_cipher(true) else {
        return;
    };
    let nonce = Nonce::generate();
    let Ok(ciphertext) = cipher.encrypt(&nonce, text.as_bytes()) else {
        return;
    };
    let mut data = nonce.to_vec();
    data.extend_from_slice(&ciphertext);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn load_cookies() -> Option<String> {
    read_sealed(&cookie_file()?)
}

fn read_sealed(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
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

#[cfg(test)]
mod live_ids {
    use super::*;
    use fuji_icloud::ids::{ICLOUD_WEB_TOPIC, IdsIdentity};
    use fuji_icloud::push::{PROD_HOST, PushConnection};

    /// Registers fuji with IDS (as icloud.com does in a new browser), with
    /// the saved session. Prints counts only. Run with `cargo test -p fuji
    /// live_ids_register -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_ids_register() {
        let saved = load().expect("a saved session");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let mut push = PushConnection::connect(PROD_HOST, &[ICLOUD_WEB_TOPIC]).expect("push");
        let handles = auth.ids_handles(&push, ICLOUD_WEB_TOPIC).expect("handles");
        println!("handles: {}", handles.len());
        let identity = IdsIdentity::generate().expect("keys");
        match auth.ids_register(&mut push, &identity, &account.dsid) {
            Ok(reg) => {
                println!(
                    "registered: {} handles, cert {} chars",
                    reg.handles.len(),
                    reg.cert.len()
                );
                let me = format!("mailto:{}", account.apple_id);
                match auth.ids_query(&push, &reg, &me, ICLOUD_WEB_TOPIC) {
                    Ok(endpoints) => {
                        println!("endpoints: {}", endpoints.len());
                        for e in &endpoints {
                            let mut keys: Vec<_> = e.client_data.keys().cloned().collect();
                            keys.sort();
                            println!(
                                "  device {}: legacy {}, client-data [{}]",
                                e.is_device,
                                e.legacy_identity.is_some(),
                                keys.join(", ")
                            );
                        }
                    }
                    Err(err) => println!("query failed: {err}"),
                }
            }
            Err(err) => println!("register failed: {err}"),
        }
        persist(&auth);
    }
}

#[cfg(test)]
mod live_skybridge {
    use super::*;
    use fuji_icloud::skybridge::Skybridge;

    fn kind(v: &serde_json::Value) -> &'static str {
        match v {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        }
    }

    /// Key verification with the paired device, using the saved session and
    /// pairing key. Run with `cargo test -p fuji live_skybridge_verify --
    /// --ignored --nocapture`.
    /// Connects twice, as two launches would, through the saved connection
    /// (`device.bin`). Prints only whether the registration was kept and
    /// the device verified the key. Run with `cargo test -p fuji
    /// live_bridge_kept -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_bridge_kept() {
        let saved = load().expect("a saved session");
        let pairing = load_pairing().expect("a pairing key");
        let key = BASE64.decode(&pairing.key).expect("pairing key is base64");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let token = auth.session_id_token().expect("session id token");
        let handle = format!("mailto:{}", account.apple_id);
        for run in 1..=2 {
            let started = std::time::Instant::now();
            let kept = load_bridge();
            let mut sky =
                Skybridge::connect(&auth, kept.as_ref(), &account.dsid, &handle).expect("connect");
            let connected = started.elapsed();
            let verified = sky.verify_key(&key, &token).is_ok();
            println!(
                "run {run}: had saved {}, kept registration {}, devices {}, verified {verified},                  connected in {connected:?}, total {:?}",
                kept.is_some(),
                sky.reused(),
                sky.device_count(),
                started.elapsed()
            );
            save_bridge(&sky.saved());
            sky.close();
        }
    }

    #[test]
    #[ignore]
    fn live_skybridge_verify() {
        let saved = load().expect("a saved session");
        let pairing = load_pairing().expect("a pairing key: sign in with device approval first");
        let key = BASE64.decode(&pairing.key).expect("pairing key is base64");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let token = auth.session_id_token().expect("session id token");
        let handle = format!("mailto:{}", account.apple_id);
        let index_path = crate::thumbs::account_dir(&account.dsid).join("index-library.json");
        let index_ids: std::collections::HashSet<String> = std::fs::read(&index_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        println!("fuji's index: {} ids", index_ids.len());
        let mut sky = Skybridge::connect(&auth, None, &account.dsid, &handle).expect("connect");
        println!("devices: {}", sky.device_count());
        match sky.verify_key(&key, &token) {
            Ok((device, name)) => {
                println!("verified by device {device}: {name:?}");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let body = serde_json::json!({ "photos": { "search": {
                    "string": "computer",
                    "assetsLimit": 500,
                    "collectionsLimit": 20,
                    "clientTimestamp": now,
                } } });
                let id = sky.send(device, body, Some(&token)).expect("send");
                let mut parts = 0;
                while let Some(reply) = sky
                    .receive(std::time::Duration::from_secs(30))
                    .expect("receive")
                {
                    if reply.msg_id != id {
                        continue;
                    }
                    parts += 1;
                    let found = &reply.body["photos"]["search"]["resultsFound"];
                    let keys = |v: &serde_json::Value| {
                        v.as_object()
                            .map(|o| {
                                o.iter()
                                    .map(|(k, v)| format!("{k}:{}", kind(v)))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_else(|| kind(v).to_string())
                    };
                    println!("part {:?}: body [{}]", reply.part, keys(&reply.body));
                    println!("  resultsFound [{}]", keys(found));
                    if let Some(first) = found["results"].as_array().and_then(|r| r.first()) {
                        println!("  a result: [{}]", keys(first));
                    }
                    println!("  totalAssetCount {}", found["totalAssetCount"]);
                    for list in ["results", "recommendedResults"] {
                        let ids: Vec<&str> = found[list]
                            .as_array()
                            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                            .unwrap_or_default();
                        if ids.is_empty() {
                            continue;
                        }
                        let in_index = ids.iter().filter(|id| index_ids.contains(**id)).count();
                        let lens: std::collections::BTreeSet<usize> =
                            ids.iter().map(|i| i.len()).collect();
                        let shape: String = ids[0]
                            .chars()
                            .map(|c| {
                                if c.is_ascii_digit() {
                                    '9'
                                } else if c.is_ascii_alphabetic() {
                                    'a'
                                } else {
                                    c
                                }
                            })
                            .collect();
                        println!(
                            "  {list}: {} ids, {in_index} in fuji's index, lengths {lens:?}, shape {shape}",
                            ids.len()
                        );
                    }
                    if reply.part.is_none_or(|(i, t)| t == 0 || i + 1 >= t) {
                        break;
                    }
                }
                println!("parts: {parts}");
            }
            Err(err) => println!("verification failed: {err}"),
        }
        sky.close();
        persist(&auth);
    }
}

#[cfg(test)]
mod live_collections {
    use super::*;
    use fuji_icloud::{Album, PhotosClient, SmartAlbum};

    /// Reads (never writes) the shared albums and a media type with the
    /// saved session. Counts only. Run with `cargo test -p fuji
    /// live_collections -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_collections() {
        let saved = load().expect("saved session");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("resume");
        persist(&auth);
        let client = PhotosClient::new(&auth, &account).expect("photos");
        let shared = client.shared_albums().expect("shared albums");
        println!(
            "{} shared albums ({} owned)",
            shared.len(),
            shared.iter().filter(|a| a.owned).count()
        );
        for album in &shared {
            let a = album.album();
            let count = client.count(&a).expect("count");
            let items = client.fetch(&a, 0, count.min(5)).expect("fetch");
            let dated = items.iter().filter(|i| i.created_at_ms > 0).count();
            let thumbs = items.iter().filter(|i| i.thumbnail.is_some()).count();
            let newest_first = items
                .windows(2)
                .all(|w| w[0].created_at_ms >= w[1].created_at_ms);
            println!(
                "  {count} items; fetched {}, {dated} dated, {thumbs} with thumbnails, newest first: {newest_first}",
                items.len()
            );
        }
        let memories = client.memories().expect("memories");
        let with_titles = memories.iter().filter(|m| !m.title.is_empty()).count();
        println!("{} memories, {with_titles} titled", memories.len());
        if let Some(memory) = memories.first() {
            let album = Album::Memory(memory.id.clone());
            let count = client.count(&album).expect("memory count");
            let items = client.fetch(&album, 0, count.min(8)).expect("memory fetch");
            let thumbs = items.iter().filter(|i| i.thumbnail.is_some()).count();
            let dated = items.iter().filter(|i| i.created_at_ms > 0).count();
            let key = memory
                .key_asset
                .as_deref()
                .map(|k| client.asset(k).map(|a| a.is_some()));
            println!(
                "  newest: {count} photos; fetched {}, {dated} dated, {thumbs} with thumbnails; key photo found: {key:?}",
                items.len()
            );
        }
        let panoramas = Album::Smart(SmartAlbum::Panoramas);
        let items = client.scan(&panoramas, 0, 3).expect("scan");
        println!("panoramas: scanned {}", items.len());
        let ids: Vec<String> = SmartAlbum::ALL
            .iter()
            .filter_map(|s| Album::Smart(*s).count_id())
            .collect();
        println!(
            "media type counts: {:?}",
            client
                .index_counts(&ids)
                .map(|c| c.into_iter().map(|(_, n)| n).collect::<Vec<_>>())
        );
        persist(&auth);
    }
    /// Reads (never writes) the library's change feed with the saved
    /// session. Prints counts only. Run with `cargo test -p fuji
    /// live_changes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_changes() {
        let saved = load().expect("a saved session");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let client = fuji_icloud::PhotosClient::new(&auth, &account).expect("photos service");
        let started = std::time::Instant::now();
        let token = client.sync_token().expect("sync token");
        let changes = client.changes(&token).expect("changes since now");
        println!(
            "changes since now: {} assets, {} removed, {} other, token moved: {}, {:?}",
            changes.assets.len(),
            changes.removed.len(),
            changes.other,
            changes.token != token,
            started.elapsed()
        );
        let count = client.count(&fuji_icloud::Album::All).expect("count");
        println!("library count read ({} digits)", count.to_string().len());
    }

    /// Uploads one small generated image, waits for it in the change feed,
    /// then moves it to Recently Deleted. Changes the library: run only when
    /// asked to. Prints counts and timings only. Run with `cargo test -p fuji
    /// live_upload_delete -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_upload_delete() {
        use std::time::{Duration, Instant};
        let saved = load().expect("a saved session");
        let auth = AuthClient::from_saved(&saved);
        restore_cookies(&auth);
        let account = auth.resume().expect("session resumes");
        persist(&auth);
        let client = fuji_icloud::PhotosClient::new(&auth, &account).expect("photos service");

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("fuji-upload-test-{stamp}.png");
        let path = std::env::temp_dir().join(&name);
        image::RgbaImage::from_fn(96, 64, |x, y| {
            image::Rgba([(x * 2) as u8, (y * 3) as u8, 160, 255])
        })
        .save(&path)
        .unwrap();

        let mut token = client.sync_token().expect("sync token");
        let before = client.count(&fuji_icloud::Album::All).expect("count");
        let started = Instant::now();
        client.upload_file(&name, &path).expect("upload");
        std::fs::remove_file(&path).ok();
        println!("uploaded in {:?}", started.elapsed());

        // Find it in the change feed by its file name.
        let mut found = None;
        while found.is_none() && started.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_secs(3));
            let changes = client.changes(&token).expect("changes");
            token = changes.token;
            found = changes
                .assets
                .into_iter()
                .find(|a| a.filename.as_deref() == Some(name.as_str()));
        }
        let asset = found.expect("the upload shows in the change feed");
        let after = client.count(&fuji_icloud::Album::All).expect("count");
        println!(
            "in the feed after {:?}: aspect {:.2}, dated {}, count +{}",
            started.elapsed(),
            asset.aspect,
            asset.created_at_ms > 0,
            after as i64 - before as i64
        );

        let deleting = Instant::now();
        client
            .set_deleted(std::slice::from_ref(&asset.id.0), true)
            .expect("delete");
        println!("deleted in {:?}", deleting.elapsed());
        let mut gone = false;
        while !gone && deleting.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_secs(2));
            let changes = client.changes(&token).expect("changes");
            token = changes.token;
            gone = changes.removed.contains(&asset.id.0);
        }
        let last = client.count(&fuji_icloud::Album::All).expect("count");
        println!(
            "removal in the feed: {gone} after {:?}; count back to before: {}",
            deleting.elapsed(),
            last == before
        );
        assert!(gone);
    }
}
