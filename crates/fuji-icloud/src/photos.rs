//! iCloud Photos over CloudKit, the way icloud.com reads it.
//!
//! The library lives in the `PrimarySync` zone of the private
//! `com.apple.photos.cloud` database. Each item is a `CPLAsset` record (what
//! the user sees: date, favorite, edits) pointing at a `CPLMaster` record (the
//! original file and its renditions). List queries return both, ranked by
//! date, newest first; `startRank` pages through them.
//!
//! Blocking I/O: call these from a background thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use ureq::Agent;

use crate::asset::{Asset, AssetId, AssetKind, Resource};
use crate::auth::{Account, AuthClient};
use crate::cookies::CookieLog;
use crate::details::{self, PhotoDetails};
use crate::memories::{self, Memory};

const CLIENT_BUILD_NUMBER: &str = "2522Project44";
const CLIENT_MASTERING_NUMBER: &str = "2522B2";
const MAX_PAGE: usize = 200;
/// CloudKit rejects a `resultsLimit` of 2 or less ("Result size limit must
/// be greater than 2").
const MIN_RESULTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum PhotosError {
    #[error(
        "iCloud Photos isn't available for this account. Check that iCloud Photos is turned on."
    )]
    NotAvailable,
    #[error("iCloud is still preparing your photo library. Try again in a few minutes.")]
    Indexing,
    #[error(
        "Apple won't share this library with the web. If Advanced Data Protection is on, turn on \"Access iCloud Data on the Web\" on your iPhone."
    )]
    WebAccessDisabled,
    #[error("Your iCloud session has expired. Sign in again.")]
    SessionExpired,
    #[error("Couldn't reach iCloud: {0}")]
    Network(#[from] ureq::Error),
    #[error("iCloud returned an error ({status}): {message}")]
    Unexpected { status: u16, message: String },
}

/// A view of the library: one of the built-in ones, or an album the user made.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Album {
    All,
    Favorites,
    Videos,
    RecentlyDeleted,
    Hidden,
    /// One of the media types Photos sorts photos into.
    Smart(SmartAlbum),
    /// A user album, by its CloudKit record name.
    User(Arc<str>),
    /// A shared album, by its GUID (served by the shared streams service,
    /// not CloudKit).
    Shared(Arc<str>),
    /// A memory, by its `CPLMemory` record name: its photos are looked up
    /// one by one rather than listed by an index.
    Memory(Arc<str>),
}

/// What changed in the library since a sync token: see
/// [`PhotosClient::changes`].
#[derive(Debug, Default)]
pub struct LibraryChanges {
    /// Where the feed stands now.
    pub token: String,
    /// Items new to the library or changed (layout fields only).
    pub assets: Vec<Asset>,
    /// Record names of items no longer in the library.
    pub removed: Vec<String>,
    /// Other records that changed (albums, memories, people).
    pub other: usize,
}

/// The media types Photos collects on its own, as icloud.com lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SmartAlbum {
    LivePhotos,
    Portrait,
    LongExposure,
    Panoramas,
    TimeLapse,
    SloMo,
    Bursts,
    Screenshots,
    Animated,
}

impl SmartAlbum {
    pub const ALL: [SmartAlbum; 9] = [
        SmartAlbum::LivePhotos,
        SmartAlbum::Portrait,
        SmartAlbum::LongExposure,
        SmartAlbum::Panoramas,
        SmartAlbum::TimeLapse,
        SmartAlbum::SloMo,
        SmartAlbum::Bursts,
        SmartAlbum::Screenshots,
        SmartAlbum::Animated,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SmartAlbum::LivePhotos => "Live Photos",
            SmartAlbum::Portrait => "Portrait",
            SmartAlbum::LongExposure => "Long Exposure",
            SmartAlbum::Panoramas => "Panoramas",
            SmartAlbum::TimeLapse => "Time-lapse",
            SmartAlbum::SloMo => "Slo-mo",
            SmartAlbum::Bursts => "Bursts",
            SmartAlbum::Screenshots => "Screenshots",
            SmartAlbum::Animated => "Animated",
        }
    }

    /// A name for file names and ids.
    pub fn slug(self) -> &'static str {
        match self {
            SmartAlbum::LivePhotos => "live",
            SmartAlbum::Portrait => "portrait",
            SmartAlbum::LongExposure => "long-exposure",
            SmartAlbum::Panoramas => "panoramas",
            SmartAlbum::TimeLapse => "time-lapse",
            SmartAlbum::SloMo => "slo-mo",
            SmartAlbum::Bursts => "bursts",
            SmartAlbum::Screenshots => "screenshots",
            SmartAlbum::Animated => "animated",
        }
    }

    /// The count index's name and the list query's `smartAlbum` filter
    /// (icloud.com's names). Bursts have their own index instead.
    fn names(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            SmartAlbum::LivePhotos => ("Live", "LIVE"),
            SmartAlbum::Portrait => ("Depth", "DEPTH"),
            SmartAlbum::LongExposure => ("Exposure", "EXPOSURE"),
            SmartAlbum::Panoramas => ("Panorama", "PANORAMA"),
            SmartAlbum::TimeLapse => ("Timelapse", "TIMELAPSE"),
            SmartAlbum::SloMo => ("Slomo", "SLOMO"),
            SmartAlbum::Screenshots => ("Screenshot", "SCREENSHOT"),
            SmartAlbum::Animated => ("Animated", "ANIMATED"),
            SmartAlbum::Bursts => return None,
        })
    }
}

impl Album {
    /// The id of the count index for [`PhotosClient::index_counts`]; `None`
    /// for shared albums, which count another way.
    pub fn count_id(&self) -> Option<String> {
        Some(match self {
            Album::All => "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            Album::Favorites => "CPLAssetInSmartAlbumByAssetDate:Favorite".into(),
            Album::Videos => "CPLAssetInSmartAlbumByAssetDate:Video".into(),
            Album::RecentlyDeleted => "CPLAssetDeletedByExpungedDate".into(),
            Album::Hidden => "CPLAssetHiddenByAssetDate".into(),
            Album::Smart(smart) => match smart.names() {
                Some((index, _)) => format!("CPLAssetInSmartAlbumByAssetDate:{index}"),
                None => "CPLAssetBurstStackAssetByAssetDate".into(),
            },
            Album::User(id) => format!("CPLContainerRelationNotDeletedByAssetDate:{id}"),
            Album::Shared(_) | Album::Memory(_) => return None,
        })
    }

    /// Record type used for listing.
    fn list_type(&self) -> &'static str {
        match self {
            Album::All => "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
            Album::Favorites | Album::Videos => "CPLAssetAndMasterInSmartAlbumByAssetDate",
            Album::Smart(SmartAlbum::Bursts) => "CPLBurstStackAssetAndMasterByAssetDate",
            Album::Smart(_) => "CPLAssetAndMasterInSmartAlbumByAssetDate",
            Album::RecentlyDeleted => "CPLAssetAndMasterDeletedByExpungedDate",
            Album::Hidden => "CPLAssetAndMasterHiddenByAssetDate",
            Album::User(_) => "CPLContainerRelationLiveByAssetDate",
            // Not listed through a CloudKit index.
            Album::Shared(_) | Album::Memory(_) => "",
        }
    }

    /// Records a list query returns per item: a master and an asset, plus
    /// the album membership (`CPLContainerRelation`) in a user album.
    fn records_per_item(&self) -> usize {
        match self {
            Album::User(_) => 3,
            _ => 2,
        }
    }

    fn smart_album(&self) -> Option<&'static str> {
        match self {
            Album::Favorites => Some("FAVORITE"),
            Album::Videos => Some("VIDEO"),
            Album::Smart(smart) => smart.names().map(|(_, filter)| filter),
            _ => None,
        }
    }
}

/// A shared album the user owns or was invited to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedAlbum {
    pub guid: Arc<str>,
    pub name: String,
    /// The user made it (rather than joined it).
    pub owned: bool,
}

impl SharedAlbum {
    pub fn album(&self) -> Album {
        Album::Shared(self.guid.clone())
    }
}

/// An album the user made, as listed in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserAlbum {
    /// CloudKit record name; stable across renames.
    pub id: Arc<str>,
    pub name: String,
}

impl UserAlbum {
    pub fn album(&self) -> Album {
        Album::User(self.id.clone())
    }
}

pub struct PhotosClient {
    agent: Agent,
    cookies: CookieLog,
    endpoint: String,
    /// The `photosupload` service, where new photos are sent, if the
    /// account has it. (The older `uploadimagews` upload answers 410 Gone.)
    upload_endpoint: Option<String>,
    /// Shared albums' service, if the account has it.
    shared_endpoint: Option<String>,
    /// Each memory's photos, once read.
    memory_assets: Mutex<HashMap<Arc<str>, Arc<Vec<String>>>>,
    dsid: String,
    query: Vec<(&'static str, String)>,
}

impl PhotosClient {
    /// Client for the signed-in account's primary library.
    pub fn new(auth: &AuthClient, account: &Account) -> Result<Self, PhotosError> {
        let root = account
            .webservices
            .get("ckdatabasews")
            .ok_or(PhotosError::NotAvailable)?;
        Ok(Self {
            agent: auth.agent(),
            cookies: auth.cookie_log(),
            endpoint: format!(
                "{}/database/1/com.apple.photos.cloud/production/private",
                root.trim_end_matches('/')
            ),
            upload_endpoint: account
                .webservices
                .get("photosupload")
                .map(|root| root.trim_end_matches('/').to_string()),
            shared_endpoint: account.webservices.get("sharedstreams").map(|root| {
                format!(
                    "{}/{}/sharedstreams",
                    root.trim_end_matches('/'),
                    account.dsid
                )
            }),
            memory_assets: Mutex::default(),
            dsid: account.dsid.clone(),
            query: vec![
                ("clientBuildNumber", CLIENT_BUILD_NUMBER.into()),
                ("clientMasteringNumber", CLIENT_MASTERING_NUMBER.into()),
                ("clientId", auth.client_id()),
                ("dsid", account.dsid.clone()),
                ("remapEnums", "true".into()),
                ("getCurrentSyncToken", "true".into()),
            ],
        })
    }

    /// Check that the library is ready to query. Apple sometimes reports it as
    /// still indexing on one call and ready on the next, so retry a few times.
    pub fn check_ready(&self) -> Result<(), PhotosError> {
        for attempt in 0..3 {
            let data = self.post(
                "records/query",
                &json!({
                    "query": { "recordType": "CheckIndexingState" },
                    "zoneID": { "zoneName": "PrimarySync" },
                }),
            )?;
            if data["records"][0]["fields"]["state"]["value"].as_str() == Some("FINISHED") {
                return Ok(());
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        Err(PhotosError::Indexing)
    }

    /// Number of items in an album.
    /// The shared albums the user owns or has joined, as icloud.com lists
    /// them.
    pub fn shared_albums(&self) -> Result<Vec<SharedAlbum>, PhotosError> {
        let data = self.shared_post("webgetalbumslist", &json!({}))?;
        Ok(data["albums"]
            .as_array()
            .map(|albums| {
                albums
                    .iter()
                    .filter_map(|album| {
                        Some(SharedAlbum {
                            guid: album["albumguid"].as_str()?.into(),
                            name: album["attributes"]["name"]
                                .as_str()
                                .unwrap_or("Shared Album")
                                .to_string(),
                            owned: album["ownerdsid"].as_str() == Some(self.dsid.as_str()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn shared_post(&self, endpoint: &str, body: &Value) -> Result<Value, PhotosError> {
        let root = self
            .shared_endpoint
            .as_ref()
            .ok_or(PhotosError::NotAvailable)?;
        let mut resp = self
            .agent
            .post(format!("{root}/{endpoint}"))
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .content_type("text/plain;charset=UTF-8")
            .send(serde_json::to_vec(body).expect("serializable query"))?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(error_for(status, &text));
        }
        serde_json::from_str(&text).map_err(|_| PhotosError::Unexpected {
            status,
            message: "response wasn't JSON".into(),
        })
    }

    /// The memories Photos made, newest first, each with the photos it
    /// shows (remembered, so opening one needn't ask again).
    pub fn memories(&self) -> Result<Vec<Memory>, PhotosError> {
        // The index only goes oldest first, a page per request, and iCloud
        // takes its time per record: ask for a wave of pages at once.
        let mut found = Vec::new();
        let mut rank = 0;
        'waves: loop {
            let pages: Vec<Result<Vec<Value>, PhotosError>> = std::thread::scope(|s| {
                let wave: Vec<_> = (0..MEMORY_WAVE)
                    .map(|i| {
                        let start = rank + i * MEMORY_PAGE;
                        s.spawn(move || self.memory_page(start, MEMORY_PAGE))
                    })
                    .collect();
                wave.into_iter()
                    .map(|page| {
                        page.join().unwrap_or_else(|_| {
                            Err(PhotosError::Unexpected {
                                status: 0,
                                message: "reading memories failed".into(),
                            })
                        })
                    })
                    .collect()
            });
            for page in pages {
                let records = page?;
                rank += records.len();
                found.extend(records.iter().filter_map(memories::parse));
                if records.len() < MEMORY_PAGE || rank > 10_000 {
                    break 'waves;
                }
            }
        }
        found.sort_by_key(|m| std::cmp::Reverse(m.created_at_ms));
        if let Ok(mut known) = self.memory_assets.lock() {
            for memory in &found {
                known.insert(memory.id.clone(), memory.assets.clone());
            }
        }
        Ok(found)
    }

    /// The photos a memory shows, asking iCloud if they aren't known yet.
    /// The newest memories, about `n` of them, without reading the whole
    /// list. The index reads up from a rank and has no count, and its order
    /// isn't simply by date: so first read one memory at a few ranks, all at
    /// once, to see which end is newer. Newest at the start: read the start.
    /// At the end: find the end (one memory every 512 ranks, then every 32)
    /// and read that. Neither: read them all.
    pub fn newest_memories(&self, n: usize) -> Result<Vec<Memory>, PhotosError> {
        const COARSE: usize = 512;
        const FINE: usize = 32;
        // A few ranks are enough to see the order, and few requests at once
        // leave room for the album covers loading meanwhile.
        let coarse: Vec<usize> = vec![0, 512, 1024, 2048, 4096, 8192];
        // The start is read meanwhile: it's the answer when the index is
        // newest first, as it has been.
        let (probed, head) = std::thread::scope(|s| {
            let head = s.spawn(|| self.memory_range(0, n + FINE));
            let probed = self.probe_memories(&coarse);
            (probed, head.join())
        });
        let probed = probed?;
        let present: Vec<(usize, i64)> = coarse
            .iter()
            .zip(&probed)
            .filter_map(|(rank, date)| Some((*rank, (*date)?)))
            .collect();
        let Some(&(last, _)) = present.last() else {
            return Ok(Vec::new());
        };
        let dates: Vec<i64> = present.iter().map(|(_, d)| *d).collect();
        let newest_first = dates.windows(2).all(|w| w[0] >= w[1]);
        let oldest_first = dates.windows(2).all(|w| w[0] <= w[1]);
        let records = if newest_first && !oldest_first {
            debug("memories: the index is newest first");
            match head {
                Ok(head) => head?,
                Err(_) => self.memory_range(0, n + FINE)?,
            }
        } else if oldest_first && !newest_first {
            debug("memories: the index is oldest first");
            // The next probed rank past the last there, or the cap.
            let next = coarse
                .iter()
                .find(|r| **r > last)
                .copied()
                .unwrap_or(10_000);
            let fine: Vec<usize> = (last..next).step_by(COARSE).collect();
            let last = self
                .probe_memories(&fine)?
                .iter()
                .zip(&fine)
                .filter(|(date, _)| date.is_some())
                .map(|(_, rank)| *rank)
                .last()
                .unwrap_or(last);
            let fine: Vec<usize> = (0..COARSE / FINE).map(|i| last + i * FINE).collect();
            let end = fine
                .iter()
                .zip(self.probe_memories(&fine)?)
                .filter(|(_, date)| date.is_some())
                .map(|(rank, _)| *rank)
                .last()
                .unwrap_or(last);
            // The list ends somewhere in [end, end + FINE).
            self.memory_range((end + 1).saturating_sub(n), n + FINE)?
        } else {
            debug("memories: no order to the index (or one page); reading them all");
            let mut all = self.memories()?;
            all.truncate(n);
            return Ok(all);
        };
        let mut found: Vec<Memory> = records.iter().filter_map(memories::parse).collect();
        found.sort_by_key(|m| std::cmp::Reverse(m.created_at_ms));
        found.truncate(n);
        if let Ok(mut known) = self.memory_assets.lock() {
            for memory in &found {
                known.insert(memory.id.clone(), memory.assets.clone());
            }
        }
        Ok(found)
    }

    /// `count` memory records from `start`, read in small pieces at once
    /// (iCloud takes its time per record, not per request).
    fn memory_range(&self, start: usize, count: usize) -> Result<Vec<Value>, PhotosError> {
        const PIECE: usize = 8;
        let pieces: Vec<Result<Vec<Value>, PhotosError>> = std::thread::scope(|s| {
            let reads: Vec<_> = (start..start + count)
                .step_by(PIECE)
                .map(|from| {
                    let len = PIECE.min(start + count - from);
                    s.spawn(move || self.memory_page(from, len))
                })
                .collect();
            reads
                .into_iter()
                .map(|read| {
                    read.join().unwrap_or_else(|_| {
                        Err(PhotosError::Unexpected {
                            status: 0,
                            message: "reading memories failed".into(),
                        })
                    })
                })
                .collect()
        });
        let mut records = Vec::with_capacity(count);
        for piece in pieces {
            records.extend(piece?);
        }
        Ok(records)
    }

    /// The date of the memory at each of `ranks` (`None` past the end),
    /// reading them all at once.
    fn probe_memories(&self, ranks: &[usize]) -> Result<Vec<Option<i64>>, PhotosError> {
        let found: Vec<Result<Option<i64>, PhotosError>> = std::thread::scope(|s| {
            let probes: Vec<_> = ranks
                .iter()
                .map(|&rank| s.spawn(move || self.memory_page(rank, 1)))
                .collect();
            probes
                .into_iter()
                .map(|probe| match probe.join() {
                    Ok(page) => page.map(|records| {
                        records
                            .first()
                            .map(|r| r["fields"]["createDate"]["value"].as_i64().unwrap_or(0))
                    }),
                    Err(_) => Err(PhotosError::Unexpected {
                        status: 0,
                        message: "reading memories failed".into(),
                    }),
                })
                .collect()
        });
        found.into_iter().collect()
    }

    /// One page of `CPLMemory` records from `rank`, oldest first.
    fn memory_page(&self, rank: usize, limit: usize) -> Result<Vec<Value>, PhotosError> {
        let data = self.post(
            "records/query",
            &json!({
                "query": {
                    "recordType": "CPLMemoryNotIsRejectedByCreateDate",
                    "filterBy": [
                        {
                            "fieldName": "startRank",
                            "fieldValue": { "type": "INT64", "value": rank },
                            "comparator": "EQUALS",
                        },
                        {
                            "fieldName": "direction",
                            "fieldValue": { "type": "STRING", "value": "ASCENDING" },
                            "comparator": "EQUALS",
                        },
                    ],
                },
                "resultsLimit": limit,
                "desiredKeys": ["title", "subtitle", "assetList", "isFavorite", "createDate"],
                "zoneID": { "zoneName": "PrimarySync" },
            }),
        )?;
        Ok(data["records"].as_array().cloned().unwrap_or_default())
    }

    fn memory_assets(&self, id: &Arc<str>) -> Result<Arc<Vec<String>>, PhotosError> {
        if let Some(known) = self
            .memory_assets
            .lock()
            .ok()
            .and_then(|m| m.get(id).cloned())
        {
            return Ok(known);
        }
        let record = self
            .lookup(&[id.to_string()], &["assetList"])?
            .into_iter()
            .next()
            .unwrap_or(Value::Null);
        let assets = record["fields"]["assetList"]["value"]
            .as_str()
            .and_then(|v| BASE64.decode(v).ok())
            .map(|b| memories::asset_list(&b).0)
            .unwrap_or_default();
        let assets = Arc::new(assets);
        if let Ok(mut known) = self.memory_assets.lock() {
            known.insert(id.clone(), assets.clone());
        }
        Ok(assets)
    }

    /// Records by name, in the order asked for (missing ones as errors in
    /// place, which parse as nothing).
    fn lookup(&self, names: &[String], keys: &[&str]) -> Result<Vec<Value>, PhotosError> {
        let mut out = Vec::with_capacity(names.len());
        for chunk in names.chunks(MAX_PAGE) {
            let records: Vec<Value> = chunk.iter().map(|n| json!({ "recordName": n })).collect();
            let data = self.post(
                "records/lookup",
                &json!({
                    "records": records,
                    "zoneID": { "zoneName": "PrimarySync" },
                    "desiredKeys": keys,
                }),
            )?;
            out.extend(data["records"].as_array().cloned().unwrap_or_default());
        }
        Ok(out)
    }

    /// Assets by `CPLAsset` record name, in that order, with their masters.
    fn assets_by_name(&self, names: &[String], keys: &[&str]) -> Result<Vec<Asset>, PhotosError> {
        let assets = self.lookup(names, keys)?;
        let master_names: Vec<String> = assets
            .iter()
            .filter_map(|a| a["fields"]["masterRef"]["value"]["recordName"].as_str())
            .map(str::to_string)
            .collect();
        let masters: HashMap<String, Value> = self
            .lookup(&master_names, keys)?
            .into_iter()
            .filter_map(|m| Some((m["recordName"].as_str()?.to_string(), m)))
            .collect();
        Ok(assets
            .iter()
            .map(|asset| {
                let master = asset["fields"]["masterRef"]["value"]["recordName"]
                    .as_str()
                    .and_then(|name| masters.get(name));
                master
                    .and_then(|master| parse_asset(asset, master))
                    .unwrap_or_else(|| missing_asset(asset))
            })
            .collect())
    }

    /// Assets by record name, looked up a page at once (memories' key
    /// photos, say). Ones that are gone come back without a thumbnail.
    pub fn assets(&self, names: &[String]) -> Result<Vec<Asset>, PhotosError> {
        self.assets_by_name(names, DESIRED_KEYS)
    }

    /// One asset by its record name (a memory's key photo, say).
    pub fn asset(&self, name: &str) -> Result<Option<Asset>, PhotosError> {
        Ok(self
            .assets_by_name(&[name.to_string()], DESIRED_KEYS)?
            .into_iter()
            .next()
            .filter(|a| a.thumbnail.is_some()))
    }

    /// Where the library's change feed stands now: hand it to
    /// [`Self::changes`] later to hear only what changed since. One small
    /// query (CloudKit returns the zone's token with any query).
    pub fn sync_token(&self) -> Result<String, PhotosError> {
        let data = self.post(
            "records/query",
            &list_query(&Album::All, 0, MIN_RESULTS, &["recordName"]),
        )?;
        data["syncToken"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| PhotosError::Unexpected {
                status: 200,
                message: "no sync token".into(),
            })
    }

    /// What changed in the library since `token`: items added or edited
    /// (still in the library), and items gone from it (deleted, hidden or
    /// expunged). Masters a changed item points at but that didn't change
    /// themselves are looked up, so each item comes back complete.
    pub fn changes(&self, token: &str) -> Result<LibraryChanges, PhotosError> {
        let mut token = token.to_string();
        let mut assets: HashMap<String, Value> = HashMap::new();
        let mut masters: HashMap<String, Value> = HashMap::new();
        let mut removed = Vec::new();
        let mut other = 0;
        let mut seen = 0;
        loop {
            let data = self.post(
                "changes/zone",
                &json!({
                    "zones": [{
                        "zoneID": { "zoneName": "PrimarySync" },
                        "syncToken": token,
                        "resultsLimit": CHANGES_PAGE,
                        "desiredKeys": CHANGE_KEYS,
                    }]
                }),
            )?;
            let zone = &data["zones"][0];
            if let Some(code) = zone["serverErrorCode"].as_str() {
                return Err(cloudkit_error(
                    200,
                    code,
                    zone["reason"].as_str().unwrap_or(""),
                ));
            }
            let records = zone["records"].as_array().cloned().unwrap_or_default();
            seen += records.len();
            for record in records {
                let Some(name) = record["recordName"].as_str().map(str::to_string) else {
                    continue;
                };
                if record["deleted"].as_bool() == Some(true) {
                    // Expunged. If it was an asset, it goes; anything else
                    // just isn't found.
                    assets.remove(&name);
                    masters.remove(&name);
                    removed.push(name);
                    continue;
                }
                match record["recordType"].as_str() {
                    Some("CPLAsset") => {
                        let flag = |f: &str| field(&record, f).and_then(Value::as_i64) == Some(1);
                        if flag("isDeleted") || flag("isHidden") {
                            assets.remove(&name);
                            removed.push(name);
                        } else {
                            assets.insert(name, record);
                        }
                    }
                    Some("CPLMaster") => {
                        masters.insert(name, record);
                    }
                    _ => other += 1,
                }
            }
            if let Some(next) = zone["syncToken"].as_str() {
                token = next.to_string();
            }
            if zone["moreComing"].as_bool() != Some(true) {
                break;
            }
            if seen > MAX_CHANGES {
                return Err(PhotosError::Unexpected {
                    status: 200,
                    message: "too many changes to apply one by one".into(),
                });
            }
        }
        // Items whose master didn't change (unhidden, edited, favorited).
        let missing: Vec<String> = assets
            .values()
            .filter_map(master_name)
            .filter(|m| !masters.contains_key(*m))
            .map(str::to_string)
            .collect();
        if !missing.is_empty() {
            for master in self.lookup(&missing, SCAN_KEYS)? {
                if let Some(name) = master["recordName"].as_str() {
                    masters.insert(name.to_string(), master.clone());
                }
            }
        }
        let assets = assets
            .values()
            .filter_map(|asset| parse_asset(asset, masters.get(master_name(asset)?)?))
            .collect();
        Ok(LibraryChanges {
            token,
            assets,
            removed,
            other,
        })
    }

    /// Move items to Recently Deleted (`deleted`), or bring them back from
    /// it, the way icloud.com does: by setting `isDeleted` on each asset.
    pub fn set_deleted(&self, names: &[String], deleted: bool) -> Result<(), PhotosError> {
        self.set_flag(names, "isDeleted", deleted)
    }

    /// Mark items as favorites, or not, as icloud.com's heart does.
    pub fn set_favorite(&self, names: &[String], favorite: bool) -> Result<(), PhotosError> {
        self.set_flag(names, "isFavorite", favorite)
    }

    /// Set a 0/1 field on each asset.
    fn set_flag(&self, names: &[String], flag: &str, on: bool) -> Result<(), PhotosError> {
        for chunk in names.chunks(MAX_PAGE) {
            let mut pending: Vec<String> = chunk.to_vec();
            // A record changed elsewhere between the lookup and the update
            // has a new tag: look it up again, once.
            for attempt in 0..2 {
                let current = self.lookup(&pending, &[flag])?;
                let operations: Vec<Value> = current
                    .iter()
                    .filter(|r| r["serverErrorCode"].is_null())
                    .filter_map(|r| {
                        Some(json!({
                            "operationType": "update",
                            "record": {
                                "recordName": r["recordName"].as_str()?,
                                "recordType": "CPLAsset",
                                "recordChangeTag": r["recordChangeTag"].as_str()?,
                                "fields": { flag: { "value": i64::from(on) } },
                            },
                        }))
                    })
                    .collect();
                if operations.is_empty() {
                    break;
                }
                let data = self.post(
                    "records/modify",
                    &json!({
                        "operations": operations,
                        "zoneID": { "zoneName": "PrimarySync" },
                        "atomic": false,
                        "desiredKeys": [flag],
                    }),
                )?;
                let failed: Vec<&Value> = data["records"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["serverErrorCode"].is_string())
                    .collect();
                let Some(first) = failed.first() else {
                    break;
                };
                let conflicts: Vec<String> = failed
                    .iter()
                    .filter(|r| r["serverErrorCode"] == "CONFLICT")
                    .filter_map(|r| r["recordName"].as_str().map(str::to_string))
                    .collect();
                if attempt == 1 || conflicts.len() != failed.len() {
                    return Err(cloudkit_error(
                        200,
                        first["serverErrorCode"].as_str().unwrap_or(""),
                        first["reason"].as_str().unwrap_or(""),
                    ));
                }
                pending = conflicts;
            }
        }
        Ok(())
    }

    /// Item counts for several of iCloud's count indexes in one request:
    /// (index id, count) for each index iCloud knows.
    pub fn index_counts(&self, ids: &[String]) -> Result<Vec<(String, usize)>, PhotosError> {
        let data = self.post(
            "internal/records/query/batch",
            &json!({
                "batch": [{
                    "resultsLimit": ids.len().max(1),
                    "query": {
                        "filterBy": {
                            "fieldName": "indexCountID",
                            "fieldValue": { "type": "STRING_LIST", "value": ids },
                            "comparator": "IN",
                        },
                        "recordType": "HyperionIndexCountLookup",
                    },
                    "zoneWide": true,
                    "zoneID": { "zoneName": "PrimarySync" },
                }]
            }),
        )?;
        Ok(data["batch"][0]["records"]
            .as_array()
            .map(|records| {
                records
                    .iter()
                    .filter_map(|r| {
                        let id = r["recordName"].as_str()?.to_string();
                        let n = r["fields"]["itemCount"]["value"].as_u64()? as usize;
                        Some((id, n))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn count(&self, album: &Album) -> Result<usize, PhotosError> {
        if let Album::Memory(id) = album {
            return Ok(self.memory_assets(id)?.len());
        }
        if let Album::Shared(guid) = album {
            let data = self.shared_post("webgetassetcount", &json!({ "albumguid": &**guid }))?;
            return data["albumassetcount"]
                .as_u64()
                .map(|n| n as usize)
                .ok_or_else(|| PhotosError::Unexpected {
                    status: 200,
                    message: "missing item count".into(),
                });
        }
        let Some(count_id) = album.count_id() else {
            return Err(PhotosError::NotAvailable);
        };
        let data = self.post(
            "internal/records/query/batch",
            &json!({
                "batch": [{
                    "resultsLimit": 1,
                    "query": {
                        "filterBy": {
                            "fieldName": "indexCountID",
                            "fieldValue": { "type": "STRING_LIST", "value": [count_id] },
                            "comparator": "IN",
                        },
                        "recordType": "HyperionIndexCountLookup",
                    },
                    "zoneWide": true,
                    "zoneID": { "zoneName": "PrimarySync" },
                }]
            }),
        )?;
        data["batch"][0]["records"][0]["fields"]["itemCount"]["value"]
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| PhotosError::Unexpected {
                status: 200,
                message: "missing item count".into(),
            })
    }

    /// Up to `limit` items starting at `start_rank`, newest first (rank 0 is
    /// the most recent). The result may be shorter at the end of the album.
    pub fn fetch(
        &self,
        album: &Album,
        start_rank: usize,
        limit: usize,
    ) -> Result<Vec<Asset>, PhotosError> {
        self.fetch_with(album, start_rank, limit, DESIRED_KEYS)
    }

    /// Like [`Self::fetch`], but only the fields needed to lay out the
    /// library (date, shape, type, favorite): much smaller responses, no
    /// download URLs. Used to index the whole library quickly.
    pub fn scan(
        &self,
        album: &Album,
        start_rank: usize,
        limit: usize,
    ) -> Result<Vec<Asset>, PhotosError> {
        self.fetch_with(album, start_rank, limit, SCAN_KEYS)
    }

    fn fetch_with(
        &self,
        album: &Album,
        start_rank: usize,
        limit: usize,
        keys: &[&str],
    ) -> Result<Vec<Asset>, PhotosError> {
        if let Album::Memory(id) = album {
            let names = self.memory_assets(id)?;
            let end = (start_rank + limit).min(names.len());
            let page = names.get(start_rank..end).unwrap_or_default();
            return self.assets_by_name(page, keys);
        }
        let mut assets = Vec::with_capacity(limit);
        let mut masters: HashMap<String, Value> = HashMap::new();
        while assets.len() < limit {
            let want = (limit - assets.len()).min(MAX_PAGE);
            let rank = start_rank + assets.len();
            let data = match album {
                // `limit` there is where the page ends, not its length.
                Album::Shared(guid) => self.shared_post(
                    "webgetassets",
                    &json!({
                        "albumguid": &**guid,
                        "offset": rank.to_string(),
                        "limit": (rank + want).to_string(),
                    }),
                )?,
                _ => self.post("records/query", &list_query(album, rank, want, keys))?,
            };
            let records = data["records"].as_array().cloned().unwrap_or_default();

            let mut page_assets = Vec::new();
            for record in records {
                match record["recordType"].as_str() {
                    Some("CPLMaster") => {
                        if let Some(name) = record["recordName"].as_str() {
                            masters.insert(name.to_string(), record.clone());
                        }
                    }
                    Some("CPLAsset") => page_assets.push(record),
                    _ => {}
                }
            }
            if page_assets.is_empty() {
                break;
            }
            for record in page_assets.into_iter().take(want) {
                let master_name = record["fields"]["masterRef"]["value"]["recordName"]
                    .as_str()
                    .unwrap_or_default();
                // Every asset takes a rank, even one we can't show, so keep a
                // placeholder to keep indexes aligned.
                let asset = masters
                    .get(master_name)
                    .and_then(|master| parse_asset(&record, master))
                    .unwrap_or_else(|| missing_asset(&record));
                assets.push(asset);
            }
        }
        Ok(assets)
    }

    /// What took a photo and how (camera, lens, exposure, place, the app
    /// that saved it), from its master record. Asked for when the viewer's
    /// info panel opens, so library pages stay small.
    pub fn details(&self, master: &str) -> Result<PhotoDetails, PhotosError> {
        let body = json!({
            "records": [{ "recordName": master }],
            "zoneID": { "zoneName": "PrimarySync" },
            "desiredKeys": details::DETAIL_KEYS,
        });
        let data = self.post("records/lookup", &body)?;
        let record = data["records"].get(0).cloned().unwrap_or(Value::Null);
        if let Some(code) = record["serverErrorCode"].as_str() {
            return Err(cloudkit_error(
                200,
                code,
                record["reason"].as_str().unwrap_or(""),
            ));
        }
        Ok(details::from_fields(&record["fields"]))
    }

    /// The albums the user made, in the order Photos shows them.
    pub fn albums(&self) -> Result<Vec<UserAlbum>, PhotosError> {
        let mut albums = Vec::new();
        let mut marker: Option<Value> = None;
        // Paged by continuation marker; a library rarely needs a second page.
        for _ in 0..50 {
            let mut body = json!({
                "query": { "recordType": "CPLAlbumByPositionLive" },
                "zoneID": { "zoneName": "PrimarySync" },
            });
            if let Some(marker) = marker.take() {
                body["continuationMarker"] = marker;
            }
            let data = self.post("records/query", &body)?;
            if let Some(records) = data["records"].as_array() {
                albums.extend(records.iter().filter_map(parse_album));
            }
            match data.get("continuationMarker") {
                Some(next) if !next.is_null() => marker = Some(next.clone()),
                _ => break,
            }
        }
        Ok(albums)
    }

    /// Download a small resource (thumbnail or preview) into memory.
    pub fn download(&self, resource: &Resource) -> Result<Vec<u8>, PhotosError> {
        let mut resp = self.agent.get(&resource.url).call()?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(error_for(status, ""));
        }
        Ok(resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()?)
    }

    /// Stream a resource (for example an original video) to a file.
    /// Writes to a temporary file next to `path` and renames it when complete.
    pub fn download_to(&self, resource: &Resource, path: &Path) -> Result<u64, PhotosError> {
        let mut resp = self.agent.get(&resource.url).call()?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(error_for(status, ""));
        }
        let io_error = |err: std::io::Error| PhotosError::Unexpected {
            status: 0,
            message: format!("couldn't save {}: {err}", path.display()),
        };
        let partial = path.with_extension(match path.extension() {
            Some(ext) => format!("{}.part", ext.to_string_lossy()),
            None => "part".into(),
        });
        let result = (|| {
            let mut file = std::fs::File::create(&partial)?;
            let mut body = resp.body_mut().with_config().limit(u64::MAX).reader();
            let written = std::io::copy(&mut body, &mut file)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&partial, path)?;
            Ok(written)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result.map_err(io_error)
    }

    /// Add a photo or video to the library, the way icloud.com's upload
    /// button does. `filename` decides the type (by its extension);
    /// `modified_ms` is the file's modification time.
    pub fn upload(
        &self,
        filename: &str,
        bytes: &[u8],
        modified_ms: i64,
    ) -> Result<(), PhotosError> {
        self.send_upload(filename, bytes.len() as u64, modified_ms, bytes)
    }

    /// [`Self::upload`] straight from a file, without reading it into memory.
    pub fn upload_file(&self, filename: &str, path: &Path) -> Result<(), PhotosError> {
        let unreadable = |err: std::io::Error| PhotosError::Unexpected {
            status: 0,
            message: format!("couldn't read {filename}: {err}"),
        };
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let meta = file.metadata().map_err(unreadable)?;
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        self.send_upload(filename, meta.len(), modified_ms, &file)
    }

    /// icloud.com's upload, in four steps on the `photosupload` service: ask
    /// for a place to put the file, send it there, add it to the library,
    /// and wait while iCloud makes its renditions.
    fn send_upload(
        &self,
        filename: &str,
        size: u64,
        modified_ms: i64,
        body: impl ureq::AsSendBody,
    ) -> Result<(), PhotosError> {
        let unexpected = |message: &str| PhotosError::Unexpected {
            status: 200,
            message: message.into(),
        };
        let key = uuid::Uuid::new_v4().to_string();
        let urls = self.upload_post(
            "createUploadUrl",
            &json!({ "zoneName": "PrimarySync", "assets": { &key: size } }),
        )?;
        let url = urls["uploadUrls"][&key]
            .as_str()
            .ok_or_else(|| unexpected("no upload address"))?;

        let mut resp = self
            .agent
            .post(url)
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .content_type("text/plain")
            .send(body)?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            if text.contains("QUOTA_EXCEEDED") {
                return Err(unexpected("Your iCloud storage is full."));
            }
            return Err(error_for(status, &text));
        }
        let sent: Value =
            serde_json::from_str(&text).map_err(|_| unexpected("upload response wasn't JSON"))?;
        if sent["isDuplicate"].as_bool() == Some(true) {
            return Err(unexpected("That's already in your library."));
        }
        let receipt = sent
            .get("singleFile")
            .cloned()
            .ok_or_else(|| unexpected("no upload receipt"))?;

        let now = jiff::Zoned::now();
        let added = self.upload_post(
            "putAsset",
            &json!({
                "zoneName": "PrimarySync",
                "localTimeZoneId": now.time_zone().iana_name().unwrap_or("UTC"),
                "importGroup": uuid::Uuid::new_v4().to_string(),
                "files": [{
                    "fileName": filename,
                    "lastModDate": modified_ms,
                    // As JavaScript's getTimezoneOffset: minutes behind UTC.
                    "timeZoneOffset": -(now.offset().seconds() / 60),
                    "singleFileUploadRequest": receipt,
                }],
            }),
        )?;
        let result = &added[0];
        let Some(job) = result["uploadJobId"].as_str() else {
            let message = result["response"]["errorMessage"]
                .as_str()
                .unwrap_or("iCloud didn't accept the file");
            return Err(unexpected(message));
        };

        // Wait for iCloud to finish with it (renditions, a video's transcode).
        for attempt in 0..UPLOAD_POLLS {
            std::thread::sleep(Duration::from_millis(if attempt < 5 { 600 } else { 2_000 }));
            let status = self.upload_post("uploadStatus", &json!({ "uploadJobIds": [job] }))?;
            let job = &status[job];
            if let Some(code) = job["errorCode"].as_i64() {
                return Err(PhotosError::Unexpected {
                    status: code as u16,
                    message: "iCloud couldn't process the file".into(),
                });
            }
            if job["progress"].as_f64().is_some_and(|p| p >= 100.) {
                return Ok(());
            }
        }
        // Still processing: it's in, and will show when done.
        Ok(())
    }

    /// A request to the `photosupload` service.
    fn upload_post(&self, endpoint: &str, body: &Value) -> Result<Value, PhotosError> {
        let root = self
            .upload_endpoint
            .as_deref()
            .ok_or(PhotosError::NotAvailable)?;
        let mut resp = self
            .agent
            .post(format!("{root}/photosupload/{endpoint}"))
            .query("dsid", &self.dsid)
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .content_type("text/plain;charset=UTF-8")
            .send(serde_json::to_vec(body).expect("serializable request"))?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(error_for(status, &text));
        }
        serde_json::from_str(&text).map_err(|_| PhotosError::Unexpected {
            status,
            message: format!("{endpoint} response wasn't JSON"),
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, PhotosError> {
        let mut req = self.agent.post(format!("{}/{}", self.endpoint, path));
        for (key, value) in &self.query {
            req = req.query(*key, value);
        }
        // icloud.com sends JSON as text/plain; CloudKit expects the same.
        let mut resp = req
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .content_type("text/plain")
            .send(serde_json::to_vec(body).expect("serializable query"))?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(error_for(status, &text));
        }
        let data: Value = serde_json::from_str(&text).map_err(|_| PhotosError::Unexpected {
            status,
            message: "response wasn't JSON".into(),
        })?;
        // Some failures come back as 200 with an error in the body.
        if let Some(code) = data["serverErrorCode"].as_str() {
            return Err(cloudkit_error(
                status,
                code,
                data["reason"].as_str().unwrap_or(""),
            ));
        }
        Ok(data)
    }
}

fn error_for(status: u16, body: &str) -> PhotosError {
    if status == 401 || status == 421 {
        return PhotosError::SessionExpired;
    }
    let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match data["serverErrorCode"].as_str() {
        Some(code) => cloudkit_error(status, code, data["reason"].as_str().unwrap_or("")),
        None => PhotosError::Unexpected {
            status,
            message: if body.is_empty() {
                "no details".into()
            } else {
                body.chars().take(200).collect()
            },
        },
    }
}

fn cloudkit_error(status: u16, code: &str, reason: &str) -> PhotosError {
    match code {
        "AUTHENTICATION_FAILED" | "AUTHENTICATION_REQUIRED" => PhotosError::SessionExpired,
        "ZONE_NOT_FOUND" => PhotosError::NotAvailable,
        "ACCESS_DENIED" | "PRIVATE_DB_DISABLED" => PhotosError::WebAccessDisabled,
        _ => PhotosError::Unexpected {
            status,
            message: format!("{code}: {reason}"),
        },
    }
}

fn list_query(album: &Album, start_rank: usize, limit: usize, keys: &[&str]) -> Value {
    let mut filters = vec![
        json!({
            "fieldName": "startRank",
            "fieldValue": { "type": "INT64", "value": start_rank },
            "comparator": "EQUALS",
        }),
        json!({
            "fieldName": "direction",
            "fieldValue": { "type": "STRING", "value": "ASCENDING" },
            "comparator": "EQUALS",
        }),
    ];
    if let Album::User(id) = album {
        filters.push(json!({
            "fieldName": "parentId",
            "fieldValue": { "type": "STRING", "value": &**id },
            "comparator": "EQUALS",
        }));
    }
    if let Some(smart) = album.smart_album() {
        filters.push(json!({
            "fieldName": "smartAlbum",
            "fieldValue": { "type": "STRING", "value": smart },
            "comparator": "EQUALS",
        }));
    }
    json!({
        "query": { "filterBy": filters, "recordType": album.list_type() },
        "resultsLimit": (limit * album.records_per_item()).max(MIN_RESULTS),
        "desiredKeys": keys,
        "zoneID": { "zoneName": "PrimarySync" },
    })
}

/// How many times to ask whether an upload is processed (about 2 min).
const UPLOAD_POLLS: usize = 65;

/// Change records asked for at a time.
const CHANGES_PAGE: usize = 400;
/// Past this many changes, a full rescan is quicker.
const MAX_CHANGES: usize = 40_000;

/// What the change feed is asked for: what the index needs, and whether
/// each item is still in the library.
const CHANGE_KEYS: &[&str] = &[
    "recordName",
    "recordType",
    "masterRef",
    "assetDate",
    "isFavorite",
    "isDeleted",
    "isHidden",
    "duration",
    "itemType",
    "resOriginalWidth",
    "resOriginalHeight",
    "resJPEGThumbWidth",
    "resJPEGThumbHeight",
    "resOriginalVidComplFileType",
    "filenameEnc",
];

/// The master record an asset points at.
fn master_name(asset: &Value) -> Option<&str> {
    asset["fields"]["masterRef"]["value"]["recordName"].as_str()
}

/// Just enough to index and lay out the library.
/// Memories asked for at a time.
const MEMORY_PAGE: usize = 50;
/// Memory pages asked for at once.
const MEMORY_WAVE: usize = 16;

const SCAN_KEYS: &[&str] = &[
    "recordName",
    "recordType",
    "masterRef",
    "assetDate",
    "isFavorite",
    "duration",
    "itemType",
    "resOriginalWidth",
    "resOriginalHeight",
    "resJPEGThumbWidth",
    "resJPEGThumbHeight",
    "resOriginalVidComplFileType",
    // For searching by name.
    "filenameEnc",
];

const DESIRED_KEYS: &[&str] = &[
    "resJPEGFullWidth",
    "resJPEGFullHeight",
    "resJPEGFullFileType",
    "resJPEGFullRes",
    "resJPEGMedWidth",
    "resJPEGMedHeight",
    "resJPEGMedFileType",
    "resJPEGMedRes",
    "resJPEGThumbWidth",
    "resJPEGThumbHeight",
    "resJPEGThumbFileType",
    "resJPEGThumbRes",
    "resVidSmallWidth",
    "resVidSmallHeight",
    "resVidSmallFileType",
    "resVidSmallRes",
    "resVidMedWidth",
    "resVidMedHeight",
    "resVidMedFileType",
    "resVidMedRes",
    "resOriginalWidth",
    "resOriginalHeight",
    "resOriginalFileType",
    "resOriginalRes",
    "resOriginalVidComplRes",
    "resOriginalVidComplFileType",
    "itemType",
    "filenameEnc",
    "recordName",
    "recordType",
    "masterRef",
    "assetDate",
    "addedDate",
    "isFavorite",
    "isHidden",
    "isDeleted",
    "duration",
    "orientation",
];

/// A field's `value`, if present.
fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    record["fields"].get(name)?.get("value")
}

fn resource(record: &Value, prefix: &str) -> Option<Resource> {
    let res = field(record, &format!("{prefix}Res"))?;
    let num = |suffix: &str| {
        field(record, &format!("{prefix}{suffix}"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    Some(Resource {
        url: res.get("downloadURL")?.as_str()?.to_string(),
        width: num("Width"),
        height: num("Height"),
        size: res.get("size").and_then(Value::as_u64).unwrap_or(0),
        file_type: field(record, &format!("{prefix}FileType"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Edited renditions live on the asset; originals on the master.
fn resource_either(asset: &Value, master: &Value, prefix: &str) -> Option<Resource> {
    resource(asset, prefix).or_else(|| resource(master, prefix))
}

fn is_movie(item_type: &str) -> bool {
    matches!(
        item_type,
        "com.apple.quicktime-movie" | "public.mpeg-4" | "public.movie" | "public.avi"
    )
}

/// Originals small enough, and in a format, that the viewer can show directly.
fn is_viewable_original(resource: &Resource) -> bool {
    const MAX_BYTES: u64 = 40 * 1024 * 1024;
    matches!(resource.file_type.as_str(), "public.jpeg" | "public.png")
        && resource.size <= MAX_BYTES
}

/// Width / height as shown: the rendered thumbnail's proportions when known
/// (edited asset first, then master), else the original's.
fn display_aspect(asset: &Value, master: &Value) -> f32 {
    let dims = |record: &Value, prefix: &str| {
        let w = field(record, &format!("{prefix}Width"))?.as_f64()?;
        let h = field(record, &format!("{prefix}Height"))?.as_f64()?;
        (w > 0. && h > 0.).then_some((w / h) as f32)
    };
    dims(asset, "resJPEGThumb")
        .or_else(|| dims(master, "resJPEGThumb"))
        .or_else(|| dims(master, "resOriginal"))
        .unwrap_or(1.)
}

fn parse_asset(asset: &Value, master: &Value) -> Option<Asset> {
    let id = asset["recordName"].as_str()?.to_string();
    let item_type = field(master, "itemType")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let kind = if is_movie(item_type) {
        // CloudKit stores the duration in milliseconds.
        let secs = field(asset, "duration")
            .and_then(|d| d.as_f64().or_else(|| d.as_i64().map(|n| n as f64)))
            .unwrap_or(0.0)
            / 1000.;
        AssetKind::Video {
            duration_secs: secs.round().max(0.0) as u32,
        }
    } else if field(master, "resOriginalVidComplRes").is_some()
        || field(master, "resOriginalVidComplFileType").is_some()
    {
        AssetKind::LivePhoto
    } else {
        AssetKind::Photo
    };

    let filename = text_field(master, "filenameEnc");
    let video = match kind {
        AssetKind::Video { .. } => resource_either(asset, master, "resVidMed")
            .or_else(|| resource_either(asset, master, "resVidSmall"))
            .or_else(|| resource(master, "resOriginal")),
        // The motion of a Live Photo, as iCloud renders it (H.264).
        AssetKind::LivePhoto => resource_either(asset, master, "resVidMed")
            .or_else(|| resource_either(asset, master, "resVidSmall")),
        AssetKind::Photo => None,
    };

    let dim = |name: &str| field(master, name).and_then(Value::as_u64).unwrap_or(0) as u32;
    let thumbnail = resource_either(asset, master, "resJPEGThumb");
    let original = resource(master, "resOriginal");
    // Apple only makes JPEG renditions when the original can't be shown as
    // is; for JPEG and PNG originals, the original is the preview.
    let preview = resource(asset, "resJPEGFull")
        .or_else(|| resource_either(asset, master, "resJPEGMed"))
        .or_else(|| resource(master, "resJPEGFull"))
        .or_else(|| original.clone().filter(is_viewable_original));

    Some(Asset {
        id: AssetId(id),
        kind,
        filename,
        width: dim("resOriginalWidth"),
        aspect: display_aspect(asset, master),
        height: dim("resOriginalHeight"),
        favorite: field(asset, "isFavorite").and_then(Value::as_i64) == Some(1),
        created_at_ms: field(asset, "assetDate")
            .or_else(|| field(master, "originalCreationDate"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        thumbnail,
        preview,
        original,
        master: field(asset, "masterRef")
            .and_then(|m| m["recordName"].as_str())
            .map(str::to_string),
        video,
    })
}

/// A text field, plain or as base64 UTF-8 (the `...Enc` fields).
fn text_field(record: &Value, name: &str) -> Option<String> {
    let f = record["fields"].get(name)?;
    let value = f.get("value")?.as_str()?;
    match f.get("type").and_then(Value::as_str) {
        Some("ENCRYPTED_BYTES") => String::from_utf8(BASE64.decode(value).ok()?).ok(),
        Some("STRING") | None => Some(value.to_string()),
        Some(_) => None,
    }
}

/// The album type of folders, which hold albums rather than photos.
const FOLDER_ALBUM_TYPE: i64 = 3;

fn parse_album(record: &Value) -> Option<UserAlbum> {
    let id = record["recordName"].as_str()?;
    // The library's root folders ("----Root-Folder----") are records too.
    if id.starts_with("----")
        || field(record, "isDeleted").and_then(Value::as_i64) == Some(1)
        || field(record, "albumType").and_then(Value::as_i64) == Some(FOLDER_ALBUM_TYPE)
    {
        return None;
    }
    let name = text_field(record, "albumNameEnc")?;
    Some(UserAlbum {
        id: id.into(),
        name: if name.trim().is_empty() {
            "Untitled Album".into()
        } else {
            name
        },
    })
}

/// An item whose master record didn't come back; shown as an empty tile.
fn missing_asset(asset: &Value) -> Asset {
    Asset {
        id: AssetId(asset["recordName"].as_str().unwrap_or_default().to_string()),
        kind: AssetKind::Photo,
        filename: None,
        width: 0,
        height: 0,
        aspect: 1.,
        favorite: false,
        created_at_ms: 0,
        thumbnail: None,
        preview: None,
        original: None,
        master: None,
        video: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(url: &str) -> Value {
        json!({ "value": { "downloadURL": url, "size": 1234, "fileChecksum": "x" } })
    }

    #[test]
    fn parses_photo_with_edited_thumbnail() {
        let master = json!({
            "recordName": "M1",
            "recordType": "CPLMaster",
            "fields": {
                "itemType": { "value": "public.heic" },
                "filenameEnc": { "type": "ENCRYPTED_BYTES", "value": BASE64.encode("IMG_0001.HEIC") },
                "resOriginalWidth": { "value": 4032 },
                "resOriginalHeight": { "value": 3024 },
                "resOriginalRes": res("https://cvws.icloud-content.com/orig"),
                "resOriginalFileType": { "value": "public.heic" },
                "resJPEGThumbRes": res("https://cvws.icloud-content.com/master-thumb"),
                "resJPEGMedRes": res("https://cvws.icloud-content.com/med"),
            }
        });
        let asset = json!({
            "recordName": "A1",
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": { "value": { "recordName": "M1" } },
                "assetDate": { "value": 1_700_000_000_000i64 },
                "isFavorite": { "value": 1 },
                "resJPEGThumbRes": res("https://cvws.icloud-content.com/edited-thumb"),
                "resJPEGThumbWidth": { "value": 360 },
            }
        });
        let a = parse_asset(&asset, &master).unwrap();
        assert_eq!(a.id.0, "A1");
        assert_eq!(a.kind, AssetKind::Photo);
        assert_eq!(a.filename.as_deref(), Some("IMG_0001.HEIC"));
        assert!(a.favorite);
        assert_eq!((a.width, a.height), (4032, 3024));
        assert_eq!(a.created_at_ms, 1_700_000_000_000);
        let thumb = a.thumbnail.unwrap();
        assert_eq!(thumb.url, "https://cvws.icloud-content.com/edited-thumb");
        assert_eq!(thumb.width, 360);
        assert_eq!(
            a.preview.unwrap().url,
            "https://cvws.icloud-content.com/med"
        );
        assert_eq!(a.original.unwrap().file_type, "public.heic");
    }

    #[test]
    fn jpeg_original_is_used_as_preview() {
        let master = json!({ "fields": {
            "itemType": { "value": "public.jpeg" },
            "resOriginalRes": res("https://x/original.jpg"),
            "resOriginalFileType": { "value": "public.jpeg" },
            "resJPEGThumbRes": res("https://x/thumb.jpg"),
        }});
        let asset = json!({ "recordName": "J", "fields": {} });
        let a = parse_asset(&asset, &master).unwrap();
        assert_eq!(a.preview.unwrap().url, "https://x/original.jpg");

        // A HEIC original isn't shown directly.
        let heic = json!({ "fields": {
            "itemType": { "value": "public.heic" },
            "resOriginalRes": res("https://x/original.heic"),
            "resOriginalFileType": { "value": "public.heic" },
        }});
        assert!(parse_asset(&asset, &heic).unwrap().preview.is_none());
    }

    #[test]
    fn aspect_prefers_rendered_thumbnail() {
        // Original stored landscape with a rotation; the thumbnail is portrait.
        let master = json!({ "fields": {
            "itemType": { "value": "public.heic" },
            "resOriginalWidth": { "value": 4032 },
            "resOriginalHeight": { "value": 3024 },
            "resJPEGThumbWidth": { "value": 360 },
            "resJPEGThumbHeight": { "value": 480 },
        }});
        let asset = json!({ "recordName": "R", "fields": {} });
        assert!((parse_asset(&asset, &master).unwrap().aspect - 0.75).abs() < 1e-6);

        let bare = json!({ "fields": { "resOriginalWidth": { "value": 200 }, "resOriginalHeight": { "value": 100 } } });
        assert!((parse_asset(&asset, &bare).unwrap().aspect - 2.).abs() < 1e-6);
    }

    #[test]
    fn live_photo_detected_from_scan_fields() {
        let master = json!({ "fields": {
            "itemType": { "value": "public.heic" },
            "resOriginalVidComplFileType": { "value": "com.apple.quicktime-movie" },
        }});
        let asset = json!({ "recordName": "L", "fields": {} });
        assert_eq!(
            parse_asset(&asset, &master).unwrap().kind,
            AssetKind::LivePhoto
        );
    }

    #[test]
    fn detects_videos_and_live_photos() {
        let video_master =
            json!({ "fields": { "itemType": { "value": "com.apple.quicktime-movie" } } });
        let video = json!({ "recordName": "V", "fields": { "duration": { "value": 75_400 } } });
        assert_eq!(
            parse_asset(&video, &video_master).unwrap().kind,
            AssetKind::Video { duration_secs: 75 }
        );

        let live_master = json!({ "fields": {
            "itemType": { "value": "public.jpeg" },
            "resOriginalVidComplRes": res("https://x/live.mov"),
        }});
        let live = json!({ "recordName": "L", "fields": {} });
        let parsed = parse_asset(&live, &live_master).unwrap();
        assert_eq!(parsed.kind, AssetKind::LivePhoto);
        // No rendition of the motion: nothing to play (not the HEVC original).
        assert!(parsed.video.is_none());

        let live_master = json!({ "fields": {
            "itemType": { "value": "public.heic" },
            "resOriginalRes": res("https://x/photo.heic"),
            "resOriginalVidComplRes": res("https://x/live.mov"),
            "resVidSmallRes": res("https://x/small.mp4"),
            "resVidMedRes": res("https://x/med.mp4"),
        }});
        let parsed = parse_asset(&live, &live_master).unwrap();
        assert_eq!(parsed.video.unwrap().url, "https://x/med.mp4");
    }

    #[test]
    fn list_query_shape() {
        let q = list_query(&Album::Favorites, 40, 100, DESIRED_KEYS);
        assert_eq!(q["resultsLimit"], 200);
        assert_eq!(
            q["query"]["recordType"],
            "CPLAssetAndMasterInSmartAlbumByAssetDate"
        );
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters[0]["fieldValue"]["value"], 40);
        assert_eq!(filters[2]["fieldValue"]["value"], "FAVORITE");
        assert_eq!(
            list_query(&Album::All, 0, 10, SCAN_KEYS)["query"]["filterBy"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn user_album_query_filters_by_parent() {
        let album = Album::User("ABC-123".into());
        let q = list_query(&album, 0, 10, SCAN_KEYS);
        assert_eq!(
            q["query"]["recordType"],
            "CPLContainerRelationLiveByAssetDate"
        );
        // Each item is three records here, and Apple wants more than two.
        assert_eq!(q["resultsLimit"], 30);
        assert_eq!(list_query(&album, 0, 1, SCAN_KEYS)["resultsLimit"], 3);
        assert_eq!(list_query(&Album::All, 0, 1, SCAN_KEYS)["resultsLimit"], 3);
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters[2]["fieldName"], "parentId");
        assert_eq!(filters[2]["fieldValue"]["value"], "ABC-123");
        assert_eq!(
            album.count_id().as_deref(),
            Some("CPLContainerRelationNotDeletedByAssetDate:ABC-123")
        );
    }

    #[test]
    fn media_types_query_like_icloud_com() {
        let panoramas = Album::Smart(SmartAlbum::Panoramas);
        assert_eq!(
            panoramas.count_id().as_deref(),
            Some("CPLAssetInSmartAlbumByAssetDate:Panorama")
        );
        let q = list_query(&panoramas, 0, 10, SCAN_KEYS);
        assert_eq!(
            q["query"]["recordType"],
            "CPLAssetAndMasterInSmartAlbumByAssetDate"
        );
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters[2]["fieldValue"]["value"], "PANORAMA");
        // Bursts and Hidden have their own indexes and no filter.
        let bursts = list_query(&Album::Smart(SmartAlbum::Bursts), 0, 10, SCAN_KEYS);
        assert_eq!(
            bursts["query"]["recordType"],
            "CPLBurstStackAssetAndMasterByAssetDate"
        );
        assert_eq!(bursts["query"]["filterBy"].as_array().unwrap().len(), 2);
        assert_eq!(
            Album::Hidden.count_id().as_deref(),
            Some("CPLAssetHiddenByAssetDate")
        );
        assert_eq!(Album::Shared("G".into()).count_id(), None);
    }

    #[test]
    fn parses_albums_and_skips_folders() {
        let record = |id: &str, name: &str, extra: Value| {
            let mut fields = json!({
                "albumNameEnc": { "type": "ENCRYPTED_BYTES", "value": BASE64.encode(name) },
            });
            let extra = extra.as_object().unwrap().clone();
            fields.as_object_mut().unwrap().extend(extra);
            json!({ "recordName": id, "recordType": "CPLAlbum", "fields": fields })
        };
        let trip = parse_album(&record(
            "A1",
            "Trip ✈",
            json!({ "albumType": { "value": 0 } }),
        ));
        assert_eq!(trip.unwrap().name, "Trip ✈");
        assert!(parse_album(&record("----Root-Folder----", "", json!({}))).is_none());
        let folder = record("F", "Folder", json!({ "albumType": { "value": 3 } }));
        assert!(parse_album(&folder).is_none());
        let deleted = record("D", "Gone", json!({ "isDeleted": { "value": 1 } }));
        assert!(parse_album(&deleted).is_none());
        let blank = parse_album(&record("U", " ", json!({}))).unwrap();
        assert_eq!(blank.name, "Untitled Album");
    }

    #[test]
    fn maps_cloudkit_errors() {
        assert!(matches!(error_for(421, ""), PhotosError::SessionExpired));
        assert!(matches!(
            error_for(403, r#"{"serverErrorCode":"ACCESS_DENIED","reason":"no"}"#),
            PhotosError::WebAccessDisabled
        ));
        assert!(matches!(
            error_for(500, "oops"),
            PhotosError::Unexpected { status: 500, .. }
        ));
    }
}

fn debug(message: &str) {
    if std::env::var_os("FUJI_DEBUG").is_some() {
        eprintln!("fuji-icloud: photos: {message}");
    }
}
