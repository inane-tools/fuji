//! On-disk image cache, so each thumbnail and preview downloads only once.
//!
//! Windows: `%LOCALAPPDATA%\fuji\cache\<dsid>\{thumbs,previews2}`
//! Linux:   `~/.cache/fuji/<dsid>/{thumbs,previews2}`
//!
//! Thumbnails are small and kept. Previews are large (a few MB each), so
//! that folder is trimmed to the most recently used [`PREVIEWS_KEPT`].

use std::fs;
use std::path::{Path, PathBuf};

use fuji_icloud::{AssetId, PhotosClient, PhotosError, Resource};
use sha2::{Digest, Sha256};

pub const PREVIEWS_KEPT: usize = 300;

/// Everything cached for one account.
pub fn account_dir(dsid: &str) -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let base = if cfg!(windows) {
        base.join("fuji").join("cache")
    } else {
        base.join("fuji")
    };
    base.join(file_safe(dsid))
}

pub fn cache_dir(dsid: &str) -> PathBuf {
    account_dir(dsid).join("thumbs")
}

pub fn preview_dir(dsid: &str) -> PathBuf {
    // "previews" (v1) could hold thumbnails cached as previews; drop it.
    let old = account_dir(dsid).join("previews");
    if old.exists() {
        let _ = fs::remove_dir_all(old);
    }
    account_dir(dsid).join("previews2")
}

/// Delete all but the `keep` most recently used files in `dir`.
pub fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), e.path()))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

pub fn path_for(dir: &Path, id: &AssetId) -> PathBuf {
    // Hashed: record names are case-sensitive, Windows file names are not.
    let digest = Sha256::digest(id.0.as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.jpg"))
}

/// Download the thumbnail unless it's already cached. Returns its path.
pub fn ensure(
    client: &PhotosClient,
    dir: &Path,
    id: &AssetId,
    resource: &Resource,
) -> Result<PathBuf, PhotosError> {
    let path = path_for(dir, id);
    if path.exists() {
        // Mark as recently used, for pruning.
        if let Ok(file) = fs::File::options().append(true).open(&path) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        return Ok(path);
    }
    let bytes = client.download(resource)?;
    let io_error = |err: std::io::Error| PhotosError::Unexpected {
        status: 0,
        message: format!("couldn't write thumbnail cache: {err}"),
    };
    fs::create_dir_all(dir).map_err(io_error)?;
    // Write then rename, so a half-written file is never shown.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, &bytes).map_err(io_error)?;
    fs::rename(&tmp, &path).map_err(io_error)?;
    Ok(path)
}

/// Make an identifier safe as a directory name: letters, digits, `-` and `_`
/// stay; anything else becomes `%XX`.
fn file_safe(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_names_are_safe() {
        assert_eq!(file_safe("123456"), "123456");
        assert_eq!(file_safe("a/b+c="), "a%2Fb%2Bc%3D");
    }

    #[test]
    fn prune_keeps_newest() {
        let dir = std::env::temp_dir().join(format!("fuji-prune-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        for i in 0..5u64 {
            let path = dir.join(format!("{i}.jpg"));
            fs::write(&path, b"x").unwrap();
            let file = fs::File::options().append(true).open(&path).unwrap();
            file.set_modified(now - std::time::Duration::from_secs(100 - i))
                .unwrap();
        }
        prune(&dir, 2);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, ["3.jpg", "4.jpg"]);
    }

    #[test]
    fn ids_differing_only_by_case_get_different_files() {
        let dir = Path::new("cache");
        let a = path_for(dir, &AssetId("AbC".into()));
        let b = path_for(dir, &AssetId("abc".into()));
        assert_ne!(
            a.to_string_lossy().to_lowercase(),
            b.to_string_lossy().to_lowercase()
        );
    }
}
