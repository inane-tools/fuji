//! Updates from GitHub releases.
//!
//! Shortly after launch, and every few hours after, Fuji asks GitHub for
//! the newest release. A newer one is announced (a ring on the account
//! picture, the account menu, the About card in Settings) and nothing more
//! happens until the user chooses: Download fetches the build for this
//! system and checks it against its signature (made in CI with a key whose
//! public half is below, so a file swapped on GitHub isn't taken); Install
//! and restart runs the new installer silently on Windows (it closes Fuji
//! and starts the new version), and on Linux puts the new AppImage in
//! place of the running one and starts it. A Flatpak, or a Fuji built from
//! source, is pointed to the releases page instead.
//!
//! `FUJI_FAKE_UPDATE=1.2.3` pretends that version is out, to try all this
//! without a release: its download is made up, and installing only
//! restarts Fuji as it is. `FUJI_FAKE_UPDATE=1.2.3:fail` fails the download.
//!
//! A release carries the Windows installer, the Linux AppImage and the
//! Linux Flatpak, each with its signature (`<file>.sig`: ECDSA P-256 over
//! SHA-256, DER, as `openssl dgst -sha256 -sign` writes it). Development
//! builds look for updates but never install them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::{App, BorrowAppContext as _, Global, SharedString};
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::Deserialize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = "inane-tools/fuji";
/// The public half of the key releases are signed with (SEC1, uncompressed).
const PUBLIC_KEY: &str = "046dcfd7159a7ec3fe5b91eefe01b74e6798a93e18da6abba2c3ff372fefd69141066bee48e79cd9f6d4d2eda83d8e7d2692ce74fa45b334f9f06aff72a79babc1";
/// The first look, after launch settles.
const FIRST_CHECK: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Larger downloads aren't taken.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// How often the download's progress is shown.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Where updating stands, for the account menu and Settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// Newer than this one. With a file, it can be downloaded and installed
    /// from here; without (no build for this system, or a development
    /// build), it's on the releases page.
    Available {
        version: String,
        asset: Option<Asset>,
    },
    Downloading {
        version: String,
        percent: u8,
    },
    /// Downloaded and checked: install and restart to use it.
    Ready {
        version: String,
        file: PathBuf,
    },
    Installing,
    Failed(SharedString),
}

/// A newer version waiting on the user: its number, and whether it's
/// downloaded. For the account picture's ring and the account menu.
pub fn pending(cx: &App) -> Option<(String, bool)> {
    match state(cx) {
        State::Available { version, .. } | State::Downloading { version, .. } => {
            Some((version, false))
        }
        State::Ready { version, .. } => Some((version, true)),
        _ => None,
    }
}

/// "Update available · v1.2.3", or "Update ready · v1.2.3".
pub fn pending_label(cx: &App) -> Option<String> {
    pending(cx).map(|(version, ready)| {
        let word = if ready { "ready" } else { "available" };
        format!("Update {word} \u{00b7} v{version}")
    })
}

#[derive(Default)]
pub struct Updates {
    pub state: State,
    /// The user's choice: look for updates by themselves.
    pub automatic: bool,
}

impl Global for Updates {}

/// Start looking for updates in the background (if allowed).
pub fn init(automatic: bool, cx: &mut App) {
    clean_up();
    cx.set_global(Updates {
        state: State::Idle,
        automatic,
    });
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK).await;
        loop {
            let automatic = cx
                .update(|cx| cx.global::<Updates>().automatic)
                .unwrap_or(false);
            if automatic && cx.update(|cx| check(false, cx)).is_err() {
                return;
            }
            cx.background_executor().timer(CHECK_EVERY).await;
        }
    })
    .detach();
}

pub fn state(cx: &App) -> State {
    cx.try_global::<Updates>()
        .map(|u| u.state.clone())
        .unwrap_or_default()
}

fn set_state(state: State, cx: &mut App) {
    if let Some(updates) = cx.try_global::<Updates>()
        && updates.state == state
    {
        return;
    }
    cx.update_default_global::<Updates, _>(|updates, _| updates.state = state);
    cx.refresh_windows();
}

pub fn set_automatic(automatic: bool, cx: &mut App) {
    cx.update_default_global::<Updates, _>(|updates, _| updates.automatic = automatic);
}

/// Look for an update now. `asked`: the user asked, so say what went
/// wrong rather than staying quiet.
pub fn check(asked: bool, cx: &mut App) {
    match state(cx) {
        State::Checking | State::Downloading { .. } | State::Ready { .. } | State::Installing => {
            return;
        }
        _ => {}
    }
    set_state(State::Checking, cx);
    cx.spawn(async move |cx| {
        let found = cx.background_executor().spawn(async { latest() }).await;
        let release = match found {
            Ok(Some(release)) if is_newer(&release.version, VERSION) => release,
            Ok(_) => {
                cx.update(|cx| set_state(State::UpToDate, cx)).ok();
                return;
            }
            Err(error) => {
                let state = if asked {
                    State::Failed(error.into())
                } else {
                    State::Idle
                };
                cx.update(|cx| set_state(state, cx)).ok();
                return;
            }
        };
        let asset = release.asset.filter(|_| can_install());
        cx.update(|cx| {
            set_state(
                State::Available {
                    version: release.version,
                    asset,
                },
                cx,
            )
        })
        .ok();
    })
    .detach();
}

/// Fetch the available version and check it, showing how far it's got.
pub fn download(cx: &mut App) {
    let State::Available {
        version,
        asset: Some(asset),
    } = state(cx)
    else {
        return;
    };
    set_state(
        State::Downloading {
            version: version.clone(),
            percent: 0,
        },
        cx,
    );
    let received = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    // Show the progress while it lasts.
    cx.spawn({
        let (received, done, version, total) =
            (received.clone(), done.clone(), version.clone(), asset.size);
        async move |cx| {
            loop {
                cx.background_executor().timer(PROGRESS_EVERY).await;
                if done.load(Ordering::Relaxed) {
                    return;
                }
                let percent = percent(received.load(Ordering::Relaxed), total);
                let shown = cx.update(|cx| {
                    let current = matches!(
                        state(cx),
                        State::Downloading { version: v, .. } if v == version
                    );
                    if current {
                        set_state(
                            State::Downloading {
                                version: version.clone(),
                                percent,
                            },
                            cx,
                        );
                    }
                    current
                });
                if !shown.unwrap_or(false) {
                    return;
                }
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let fetched = cx
            .background_executor()
            .spawn({
                let (version, received) = (version.clone(), received.clone());
                async move { fetch(&version, &asset, &received) }
            })
            .await;
        done.store(true, Ordering::Relaxed);
        let state = match fetched {
            Ok(file) => State::Ready { version, file },
            Err(error) => {
                State::Failed(format!("Couldn't download Fuji {version}: {error}.").into())
            }
        };
        cx.update(|cx| set_state(state, cx)).ok();
    })
    .detach();
}

fn percent(received: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    (received.saturating_mul(100) / total).min(100) as u8
}

/// Put the downloaded version in place of this one, start it, and quit.
/// If that fails, the error is shown and this one carries on.
pub fn install(cx: &mut App) {
    let State::Ready { file, version } = state(cx) else {
        return;
    };
    set_state(State::Installing, cx);
    cx.spawn(async move |cx| {
        // Let "Installing and restarting" show.
        cx.background_executor()
            .timer(Duration::from_millis(400))
            .await;
        let result = install_now(&file);
        cx.update(|cx| match result {
            Ok(()) => cx.quit(),
            Err(error) => set_state(
                State::Failed(format!("Couldn't install Fuji {version}: {error}.").into()),
                cx,
            ),
        })
        .ok();
    })
    .detach();
}

fn install_now(file: &Path) -> Result<(), String> {
    let target = install_target()?;
    // A made-up update only restarts.
    if fake().is_some() {
        return start(&target, &[]);
    }
    if cfg!(windows) {
        // The installer replaces this one (once it has quit) and starts
        // the new version.
        start(
            file,
            &["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"],
        )
    } else {
        replace(file, &target)?;
        start(&target, &[])
    }
}

fn start(program: &Path, args: &[&str]) -> Result<(), String> {
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("the new version didn't start ({e})"))
}

pub fn releases_page() -> String {
    format!("https://github.com/{REPOSITORY}/releases/latest")
}

// ---- GitHub ------------------------------------------------------------------

struct Release {
    version: String,
    /// The build for this system, if the release has one.
    asset: Option<Asset>,
}

/// A release's file for this system.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    url: String,
    size: u64,
    signature_url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// The release file that updates this copy of Fuji, if one does: the
/// installer on Windows, the AppImage when running as one on Linux.
fn asset_name() -> Option<&'static str> {
    let appimage = std::env::var_os("APPIMAGE").is_some();
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("fuji-windows-x86_64-setup.exe"),
        ("linux", "x86_64") if appimage => Some("fuji-linux-x86_64.AppImage"),
        _ => None,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(format!("fuji/{VERSION}"))
        .build()
        .into()
}

/// The newest published release.
fn latest() -> Result<Option<Release>, String> {
    if let Some((version, _)) = fake() {
        std::thread::sleep(Duration::from_millis(600));
        return Ok(Some(Release {
            version,
            asset: Some(Asset {
                url: FAKE_URL.into(),
                size: 24 * 1024 * 1024,
                signature_url: FAKE_URL.into(),
            }),
        }));
    }
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut response = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => "No releases yet.".to_string(),
            e => format!("Couldn't reach GitHub ({e})."),
        })?;
    let release: GithubRelease = response
        .body_mut()
        .read_json()
        .map_err(|_| "GitHub's answer wasn't readable.".to_string())?;
    Ok(parse_release(release))
}

fn parse_release(release: GithubRelease) -> Option<Release> {
    if release.draft || release.prerelease {
        return None;
    }
    let version = release.tag_name.trim_start_matches('v').to_string();
    let asset = asset_name().and_then(|name| {
        let file = release.assets.iter().find(|a| a.name == name)?;
        let signature = release
            .assets
            .iter()
            .find(|a| a.name == format!("{name}.sig"))?;
        Some(Asset {
            url: file.browser_download_url.clone(),
            size: file.size,
            signature_url: signature.browser_download_url.clone(),
        })
    });
    Some(Release { version, asset })
}

/// The made-up update of `FUJI_FAKE_UPDATE`: its version, and whether its
/// download fails.
fn fake() -> Option<(String, bool)> {
    let value = std::env::var("FUJI_FAKE_UPDATE").ok()?;
    let (version, fail) = match value.split_once(':') {
        Some((version, rest)) => (version, rest == "fail"),
        None => (value.as_str(), false),
    };
    let version = version.trim().trim_start_matches('v');
    (!version.is_empty())
        .then(|| version.to_string())
        .map(|v| (v, fail))
}

const FAKE_URL: &str = "fake:";

/// "1.2.10" is newer than "1.2.9"; anything after a `-` is ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        version
            .split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

// ---- Download and check ----------------------------------------------------------

fn updates_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("fuji").join("updates"))
}

/// Read `url` whole, counting what's arrived into `received`.
fn read_all(url: &str, limit: u64, received: &AtomicU64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut response = agent()
        .get(url)
        .call()
        .map_err(|e| format!("the download failed ({e})"))?;
    let mut reader = response.body_mut().with_config().limit(limit).reader();
    let mut bytes = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| format!("the download failed ({e})"))?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..n]);
        received.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// A made-up download: a few seconds of progress, then a file that's
/// never installed.
fn fake_fetch(asset: &Asset, fail: bool, received: &AtomicU64) -> Result<Vec<u8>, String> {
    let steps = 40;
    for step in 1..=steps {
        std::thread::sleep(Duration::from_millis(80));
        received.store(asset.size * step / steps, Ordering::Relaxed);
        if fail && step == steps * 3 / 5 {
            return Err("the connection dropped (a test failure)".into());
        }
    }
    Ok(b"fake update".to_vec())
}

/// Fetch `asset`, check its signature, and keep it ready to install.
fn fetch(version: &str, asset: &Asset, received: &AtomicU64) -> Result<PathBuf, String> {
    if asset.size > MAX_DOWNLOAD {
        return Err("it's larger than expected".into());
    }
    let bytes = if let Some((_, fail)) = fake().filter(|_| asset.url == FAKE_URL) {
        fake_fetch(asset, fail, received)?
    } else {
        let signature = read_all(&asset.signature_url, 4096, &AtomicU64::new(0))?;
        let bytes = read_all(&asset.url, MAX_DOWNLOAD, received)?;
        if !verify(&bytes, &signature) {
            return Err("its signature doesn't match, so it wasn't kept".into());
        }
        bytes
    };
    let dir = updates_dir().ok_or("there's no cache folder")?;
    // Only the newest download is kept.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("fuji-{version}{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755));
    }
    Ok(file)
}

/// Whether `signature` is the release key's signature of `bytes`.
fn verify(bytes: &[u8], signature: &[u8]) -> bool {
    verify_with(PUBLIC_KEY, bytes, signature)
}

fn verify_with(public_key_hex: &str, bytes: &[u8], signature: &[u8]) -> bool {
    let Some(key) = hex_bytes(public_key_hex).and_then(|k| VerifyingKey::from_sec1_bytes(&k).ok())
    else {
        return false;
    };
    let Ok(signature) = Signature::from_der(signature) else {
        return false;
    };
    // OpenSSL doesn't normalize s; the signature is as good either way.
    let signature = signature.normalize_s();
    key.verify(bytes, &signature).is_ok()
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

// ---- Install ---------------------------------------------------------------------

/// Whether this build installs updates itself. Development builds don't
/// (they'd replace `target/debug`); nor does a Flatpak or a system
/// package, which their package manager updates.
fn can_install() -> bool {
    if fake().is_some() {
        return true;
    }
    if cfg!(debug_assertions) || std::env::var_os("FUJI_NO_UPDATE").is_some() {
        return false;
    }
    install_target().is_ok_and(|path| is_writable_dir(path.parent()))
}

/// The file to replace: the AppImage on Linux, else the program itself.
fn install_target() -> Result<PathBuf, String> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(appimage));
    }
    std::env::current_exe().map_err(|e| e.to_string())
}

fn is_writable_dir(dir: Option<&Path>) -> bool {
    let Some(dir) = dir else { return false };
    let probe = dir.join(".fuji-update-probe");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

fn set_aside(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    target.with_file_name(name)
}

/// Put `new` where `target` is. A running program can be renamed but not
/// overwritten on Windows, so the old one is moved aside first (and
/// removed on the next launch).
fn replace(new: &Path, target: &Path) -> Result<(), String> {
    let old = set_aside(target);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(target, &old)
        .map_err(|e| format!("Couldn't set the old version aside: {e}"))?;
    if let Err(error) = std::fs::copy(new, target) {
        // Put things back as they were.
        let _ = std::fs::rename(&old, target);
        return Err(format!("Couldn't put the new version in place: {error}"));
    }
    if let Some(dir) = updates_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

/// Remove what the last update left: the old program it set aside, and
/// the download (nothing is waiting on it at launch).
fn clean_up() {
    if let Ok(target) = install_target() {
        let _ = std::fs::remove_file(set_aside(&target));
    }
    if let Some(dir) = updates_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_a_percentage() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(50, 200), 25);
        assert_eq!(percent(300, 200), 100);
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.10.0", "1.9.3"));
        assert!(is_newer("2.0", "1.99.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"));
    }

    #[test]
    #[cfg(windows)]
    fn releases_pick_this_systems_file() {
        let name = asset_name().unwrap();
        assert_eq!(name, "fuji-windows-x86_64-setup.exe");
        let asset = |n: &str| GithubAsset {
            name: n.into(),
            browser_download_url: format!("https://example.invalid/{n}"),
            size: 10,
        };
        let release = GithubRelease {
            tag_name: "v1.2.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![asset("other"), asset(name), asset(&format!("{name}.sig"))],
        };
        let found = parse_release(release).unwrap();
        assert_eq!(found.version, "1.2.0");
        let file = found.asset.unwrap();
        assert!(file.url.ends_with(name));
        assert!(file.signature_url.ends_with(".sig"));

        // No signature: not installable.
        let unsigned = GithubRelease {
            tag_name: "v1.2.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![asset(name)],
        };
        assert!(parse_release(unsigned).unwrap().asset.is_none());

        let pre = GithubRelease {
            tag_name: "v2.0.0-rc1".into(),
            draft: false,
            prerelease: true,
            assets: vec![],
        };
        assert!(parse_release(pre).is_none());
    }

    #[test]
    fn signatures_are_checked() {
        use p256::ecdsa::SigningKey;
        use p256::ecdsa::signature::Signer as _;
        let key = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let public: String = key
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let signature: Signature = key.sign(b"fuji");
        let der = signature.to_der();
        assert!(verify_with(&public, b"fuji", der.as_bytes()));
        assert!(!verify_with(&public, b"fuj1", der.as_bytes()));
        assert!(!verify_with(PUBLIC_KEY, b"fuji", der.as_bytes()));
        assert!(!verify_with(&public, b"fuji", b"not a signature"));
    }

    /// A file signed as CI signs it (`openssl dgst -sha256 -sign`) with the
    /// real key passes. Run with FUJI_SIGNED=<file> (its `.sig` beside it).
    #[test]
    #[ignore]
    fn openssl_signatures_verify() {
        let file = std::env::var("FUJI_SIGNED").expect("FUJI_SIGNED");
        let bytes = std::fs::read(&file).unwrap();
        let signature = std::fs::read(format!("{file}.sig")).unwrap();
        assert!(verify(&bytes, &signature));
        let mut tampered = bytes.clone();
        tampered.push(0);
        assert!(!verify(&tampered, &signature));
    }

    #[test]
    fn the_release_key_reads() {
        let key = hex_bytes(PUBLIC_KEY).unwrap();
        assert!(VerifyingKey::from_sec1_bytes(&key).is_ok());
    }

    #[test]
    fn replacing_sets_the_old_one_aside() {
        let dir = std::env::temp_dir().join(format!("fuji-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (target, new) = (dir.join("fuji.exe"), dir.join("fuji-1.2.0.exe"));
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(set_aside(&target)).unwrap(), b"old");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
