//! Memories: the collections Photos makes on the devices ("Lisbon",
//! "Summer 2025"), synced as `CPLMemory` records.
//!
//! A memory's photos are a protobuf in its `assetList` field (the same
//! message icloud.com decodes, `CPLMemoryAssetList`): each entry names an
//! asset (its `CPLAsset` record name) and flags it, for example as the key
//! photo or as part of the curated selection Photos shows.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use crate::push::pb;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    /// The `CPLMemory` record name.
    pub id: Arc<str>,
    pub title: String,
    /// Usually when it was: "14 Nov 2026".
    pub subtitle: String,
    /// The photo that stands for it.
    pub key_asset: Option<String>,
    pub favorite: bool,
    /// Milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// Photos it shows, in order (by `CPLAsset` record name).
    pub assets: Arc<Vec<String>>,
}

/// The photos a memory shows and its key photo, from its `assetList`:
/// the user's own picks if there are any, else Photos' curated ones, else
/// all of them.
pub(crate) fn asset_list(bytes: &[u8]) -> (Vec<String>, Option<String>) {
    let mut all = Vec::new();
    let mut curated = Vec::new();
    let mut user = Vec::new();
    let mut key = None;
    let Some(list) = pb::fields(bytes) else {
        return (all, key);
    };
    for (field, value) in list {
        let (1, pb::Value::Bytes(entry)) = (field, value) else {
            continue;
        };
        let Some(entry) = pb::fields(entry) else {
            continue;
        };
        let Some(id) = pb::get_bytes(&entry, 1).and_then(|b| std::str::from_utf8(b).ok()) else {
            continue;
        };
        let flags = pb::get_bytes(&entry, 2)
            .and_then(pb::fields)
            .unwrap_or_default();
        let flag = |n| pb::get_int(&flags, n) == Some(1);
        if flag(4) {
            key = Some(id.to_string());
        }
        if flag(2) {
            curated.push(id.to_string());
        }
        if flag(6) {
            user.push(id.to_string());
        }
        all.push(id.to_string());
    }
    let shown = if !user.is_empty() {
        user
    } else if !curated.is_empty() {
        curated
    } else {
        all
    };
    let key = key.or_else(|| shown.first().cloned());
    (shown, key)
}

/// A memory from its record; `None` if it has no photos.
pub(crate) fn parse(record: &Value) -> Option<Memory> {
    let fields = &record["fields"];
    let text = |name: &str| {
        fields[name]["value"]
            .as_str()
            .and_then(|v| BASE64.decode(v).ok())
            .and_then(|b| String::from_utf8(b).ok())
            // Titles may break a line where Photos sets them in two.
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let bytes = fields["assetList"]["value"]
        .as_str()
        .and_then(|v| BASE64.decode(v).ok())?;
    let (assets, key_asset) = asset_list(&bytes);
    if assets.is_empty() {
        return None;
    }
    Some(Memory {
        id: record["recordName"].as_str()?.into(),
        title: text("title"),
        subtitle: text("subtitle"),
        key_asset,
        favorite: fields["isFavorite"]["value"].as_i64() == Some(1),
        created_at_ms: fields["createDate"]["value"].as_i64().unwrap_or(0),
        assets: Arc::new(assets),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, flags: &[u32]) -> Vec<u8> {
        let mut flag_bytes = Vec::new();
        for &f in flags {
            pb::varint_field(&mut flag_bytes, f, 1);
        }
        let mut out = Vec::new();
        pb::bytes_field(&mut out, 1, id.as_bytes());
        pb::bytes_field(&mut out, 2, &flag_bytes);
        out
    }

    fn list(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            pb::bytes_field(&mut out, 1, e);
        }
        pb::varint_field(&mut out, 2, 1);
        out
    }

    #[test]
    fn shows_the_curated_photos_and_finds_the_key() {
        let bytes = list(&[entry("A", &[]), entry("B", &[2]), entry("C", &[2, 4])]);
        let (shown, key) = asset_list(&bytes);
        assert_eq!(shown, ["B", "C"]);
        assert_eq!(key.as_deref(), Some("C"));
    }

    #[test]
    fn the_users_picks_win_and_nothing_flagged_means_all() {
        let picks = list(&[entry("A", &[2, 6]), entry("B", &[2])]);
        assert_eq!(asset_list(&picks).0, ["A"]);
        let plain = list(&[entry("A", &[]), entry("B", &[])]);
        let (shown, key) = asset_list(&plain);
        assert_eq!(shown, ["A", "B"]);
        assert_eq!(key.as_deref(), Some("A"));
    }

    #[test]
    fn reads_a_record() {
        let record = serde_json::json!({
            "recordName": "M1",
            "fields": {
                "title": { "value": BASE64.encode("Taavetti &
        Heituinlahti") },
                "subtitle": { "value": BASE64.encode("14 Nov 2026") },
                "assetList": { "value": BASE64.encode(list(&[entry("A", &[2, 4])])) },
                "isFavorite": { "value": 1 },
                "createDate": { "value": 1_700_000_000_000i64 },
            }
        });
        let memory = parse(&record).unwrap();
        assert_eq!(memory.title, "Taavetti & Heituinlahti");
        assert_eq!(memory.subtitle, "14 Nov 2026");
        assert_eq!(*memory.assets, ["A"]);
        assert!(memory.favorite);
        assert!(parse(&serde_json::json!({ "recordName": "M2", "fields": {} })).is_none());
    }
}
