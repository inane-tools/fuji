//! On-disk thumbnail cache, so the grid only downloads each thumbnail once.
//!
//! Windows: `%LOCALAPPDATA%\fuji\cache\<dsid>\thumbs`
//! Linux:   `~/.cache/fuji/<dsid>/thumbs`

use std::fs;
use std::path::{Path, PathBuf};

use fuji_icloud::{AssetId, PhotosClient, PhotosError, Resource};
use sha2::{Digest, Sha256};

pub fn cache_dir(dsid: &str) -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let base = if cfg!(windows) {
        base.join("fuji").join("cache")
    } else {
        base.join("fuji")
    };
    base.join(file_safe(dsid)).join("thumbs")
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
