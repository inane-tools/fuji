//! A light index of every item in an album: just what the gallery needs to
//! lay itself out (date, shape, type, favorite), newest first.
//!
//! Cached on disk per album so the gallery appears at once on launch; a
//! background scan refreshes it from iCloud. Download URLs are not kept here
//! (they expire); full records are fetched by page when needed.

use std::collections::HashSet;
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
    /// The original's file name, for search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
            name: asset.filename.clone(),
        }
    }
}

/// Bump when the format changes. v1 stored video durations in milliseconds;
/// v2 had no file names (the next scan adds them).
const VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    items: Vec<IndexItem>,
    /// Where the library's change feed stood when `items` were current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// How many more items iCloud counts than it lists.
    #[serde(default)]
    gap: isize,
}

/// An album's index as cached, with the change-feed token it's current to.
pub struct Cached {
    pub items: Vec<IndexItem>,
    pub token: Option<String>,
    pub gap: isize,
}

fn safe_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A short, file-safe name for an album: "library", "type-panoramas".
pub fn album_slug(album: &Album) -> String {
    match album {
        Album::All => "library".into(),
        Album::Favorites => "favorites".into(),
        Album::Videos => "videos".into(),
        Album::RecentlyDeleted => "recently-deleted".into(),
        Album::Hidden => "hidden".into(),
        Album::Smart(smart) => format!("type-{}", smart.slug()),
        // Record names and GUIDs are UUIDs, but keep the file name safe
        // whatever they are.
        Album::User(id) => format!("album-{}", safe_name(id)),
        Album::Shared(guid) => format!("shared-{}", safe_name(guid)),
        Album::Memory(id) => format!("memory-{}", safe_name(id)),
    }
}

pub fn cache_path(account_dir: &Path, album: &Album) -> PathBuf {
    let name = album_slug(album);
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

pub fn load(path: &Path) -> Option<Cached> {
    let bytes = std::fs::read(path).ok()?;
    let mut file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    match file.version {
        VERSION | 2 => {}
        1 => {
            for item in &mut file.items {
                if let Kind::Video(ms) = &mut item.kind {
                    *ms = ((*ms as f32) / 1000.).round() as u32;
                }
            }
        }
        _ => return None,
    }
    Some(Cached {
        items: file.items,
        token: file.token,
        gap: file.gap,
    })
}

pub fn save(path: &Path, items: Vec<IndexItem>, token: Option<String>, gap: isize) {
    let file = CacheFile {
        version: VERSION,
        items,
        token,
        gap,
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

/// Bring an index (newest first) up to date: drop `removed`, and put each
/// of `changed` where its date belongs, replacing any older copy. Returns
/// whether anything moved.
pub fn apply(
    items: &mut Vec<IndexItem>,
    mut changed: Vec<IndexItem>,
    removed: &HashSet<String>,
) -> bool {
    // An item that's already here as it is needn't move.
    let current: std::collections::HashMap<&str, &IndexItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();
    changed.retain(|item| current.get(item.id.as_str()) != Some(&item));
    let had = items.len();
    let replaced: HashSet<String> = changed.iter().map(|c| c.id.clone()).collect();
    items.retain(|item| !removed.contains(&item.id) && !replaced.contains(&item.id));
    if changed.is_empty() {
        return items.len() != had;
    }
    // Merge, newest first; an arrival goes after items of the same time.
    changed.sort_by_key(|item| std::cmp::Reverse(item.date));
    let old = std::mem::take(items);
    items.reserve(old.len() + changed.len());
    let mut arrivals = changed.into_iter().peekable();
    for item in old {
        while arrivals.peek().is_some_and(|a| a.date > item.date) {
            items.extend(arrivals.next());
        }
        items.push(item);
    }
    items.extend(arrivals);
    true
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
                name: Some("IMG_0001.MOV".into()),
            },
            IndexItem {
                id: "B".into(),
                date: 0,
                aspect: 1.5,
                kind: Kind::Live,
                favorite: false,
                name: None,
            },
        ];
        save(&path, items.clone(), Some("T".into()), 2);
        let loaded = load(&path).expect("cache reads back");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded.items, items);
        assert_eq!(loaded.token.as_deref(), Some("T"));
        assert_eq!(loaded.gap, 2);
    }

    fn item(id: &str, date: i64) -> IndexItem {
        IndexItem {
            id: id.into(),
            date,
            aspect: 1.,
            kind: Kind::Photo,
            favorite: false,
            name: None,
        }
    }

    fn ids(items: &[IndexItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn changes_land_by_date() {
        let mut items = vec![item("a", 50), item("b", 40), item("c", 30), item("d", 10)];
        let removed: HashSet<String> = ["b".to_string(), "zz".to_string()].into();
        // New (newest, middle, oldest), and "c" edited to a later date.
        let changed = vec![item("n", 60), item("m", 35), item("c", 45), item("o", 1)];
        assert!(apply(&mut items, changed, &removed));
        assert_eq!(ids(&items), ["n", "a", "c", "m", "d", "o"]);
    }

    #[test]
    fn unchanged_items_stay_put() {
        let mut items = vec![item("a", 50), item("b", 50), item("c", 30)];
        assert!(!apply(&mut items, vec![item("b", 50)], &HashSet::new()));
        assert_eq!(ids(&items), ["a", "b", "c"]);
        let mut fav = item("b", 50);
        fav.favorite = true;
        assert!(apply(&mut items, vec![fav], &HashSet::new()));
        assert!(items.iter().find(|i| i.id == "b").unwrap().favorite);
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
        assert!(load(Path::new("definitely/not/here.json")).is_none());
    }
}
