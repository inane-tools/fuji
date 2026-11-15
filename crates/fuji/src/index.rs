//! A light index of every item in an album: just what the gallery needs to
//! lay itself out (date, shape, type, favorite), newest first.
//!
//! Cached on disk per album so the gallery appears at once on launch; a
//! background scan refreshes it from iCloud. Download URLs are not kept here
//! (they expire); full records are fetched by page when needed.

use std::path::{Path, PathBuf};

use fuji_icloud::{Album, Asset, AssetId, AssetKind, UserAlbum};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexItem {
    pub id: String,
    /// Capture time, ms since the Unix epoch (0 = unknown).
    pub date: i64,
    /// Width / height as displayed.
    pub aspect: f32,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub favorite: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Photo,
    Live,
    Video(u32),
}

impl IndexItem {
    pub fn asset_id(&self) -> AssetId {
        AssetId(self.id.clone())
    }
}

impl From<&Asset> for IndexItem {
    fn from(asset: &Asset) -> Self {
        Self {
            id: asset.id.0.clone(),
            date: asset.created_at_ms,
            aspect: asset.aspect,
            kind: match asset.kind {
                AssetKind::Photo => Kind::Photo,
                AssetKind::LivePhoto => Kind::Live,
                AssetKind::Video { duration_secs } => Kind::Video(duration_secs),
            },
            favorite: asset.favorite,
        }
    }
}

/// Bump when the format changes. v1 stored video durations in milliseconds.
const VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    items: Vec<IndexItem>,
}

pub fn cache_path(account_dir: &Path, album: &Album) -> PathBuf {
    let name = match album {
        Album::All => "library".into(),
        Album::Favorites => "favorites".into(),
        Album::Videos => "videos".into(),
        Album::RecentlyDeleted => "recently-deleted".into(),
        // Record names are UUIDs, but keep the file name safe whatever they are.
        Album::User(id) => {
            let safe: String = id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("album-{safe}")
        }
    };
    account_dir.join(format!("index-{name}.json"))
}

/// The user's albums as last listed, so the sidebar fills in at once.
#[derive(Serialize, Deserialize)]
struct AlbumListFile {
    albums: Vec<(String, String)>,
}

pub fn albums_path(account_dir: &Path) -> PathBuf {
    account_dir.join("albums.json")
}

pub fn load_albums(path: &Path) -> Option<Vec<UserAlbum>> {
    let file: AlbumListFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(
        file.albums
            .into_iter()
            .map(|(id, name)| UserAlbum {
                id: id.into(),
                name,
            })
            .collect(),
    )
}

pub fn save_albums(path: &Path, albums: &[UserAlbum]) {
    let file = AlbumListFile {
        albums: albums
            .iter()
            .map(|a| (a.id.to_string(), a.name.clone()))
            .collect(),
    };
    if let (Ok(json), Some(dir)) = (serde_json::to_vec(&file), path.parent()) {
        let _ = std::fs::create_dir_all(dir);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

pub fn load(path: &Path) -> Option<Vec<IndexItem>> {
    let bytes = std::fs::read(path).ok()?;
    let mut file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    match file.version {
        VERSION => Some(file.items),
        1 => {
            for item in &mut file.items {
                if let Kind::Video(ms) = &mut item.kind {
                    *ms = ((*ms as f32) / 1000.).round() as u32;
                }
            }
            Some(file.items)
        }
        _ => None,
    }
}

pub fn save(path: &Path, items: &[IndexItem]) {
    let file = CacheFile {
        version: VERSION,
        items: items.to_vec(),
    };
    let Ok(json) = serde_json::to_vec(&file) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("fuji-index-test-{}", std::process::id()));
        let path = cache_path(&dir, &Album::All);
        let items = vec![
            IndexItem {
                id: "A".into(),
                date: 1_700_000_000_000,
                aspect: 0.75,
                kind: Kind::Video(12),
                favorite: true,
            },
            IndexItem {
                id: "B".into(),
                date: 0,
                aspect: 1.5,
                kind: Kind::Live,
                favorite: false,
            },
        ];
        save(&path, &items);
        let loaded = load(&path);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded, Some(items));
    }

    #[test]
    fn album_cache_names_are_safe() {
        let dir = Path::new("acct");
        let path = cache_path(dir, &Album::User("AB-12/..\\x".into()));
        assert_eq!(path, dir.join("index-album-AB-12____x.json"));
    }

    #[test]
    fn album_list_round_trip() {
        let dir = std::env::temp_dir().join(format!("fuji-albums-test-{}", std::process::id()));
        let path = albums_path(&dir);
        let albums = vec![UserAlbum {
            id: "X1".into(),
            name: "Trip".into(),
        }];
        save_albums(&path, &albums);
        let loaded = load_albums(&path);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded, Some(albums));
    }

    #[test]
    fn missing_or_bad_cache_is_none() {
        assert_eq!(load(Path::new("definitely/not/here.json")), None);
    }
}
