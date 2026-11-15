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
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use ureq::Agent;

use crate::asset::{Asset, AssetId, AssetKind, Resource};
use crate::auth::{Account, AuthClient};
use crate::cookies::CookieLog;
use crate::details::{self, PhotoDetails};

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
            Album::Shared(_) => return None,
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
            // Not listed through CloudKit.
            Album::Shared(_) => "",
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
    /// Where new photos are sent, if the account has the upload service.
    upload_endpoint: Option<String>,
    /// Shared albums' service, if the account has it.
    shared_endpoint: Option<String>,
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
                .get("uploadimagews")
                .map(|root| format!("{}/upload", root.trim_end_matches('/'))),
            shared_endpoint: account.webservices.get("sharedstreams").map(|root| {
                format!(
                    "{}/{}/sharedstreams",
                    root.trim_end_matches('/'),
                    account.dsid
                )
            }),
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

    /// Add a photo or video file to the library, the way icloud.com's
    /// upload button does. `filename` decides the type (by its extension).
    pub fn upload(&self, filename: &str, bytes: &[u8]) -> Result<(), PhotosError> {
        let endpoint = self
            .upload_endpoint
            .as_deref()
            .ok_or(PhotosError::NotAvailable)?;
        let mut resp = self
            .agent
            .post(endpoint)
            .query("filename", filename)
            .query("dsid", &self.dsid)
            .header("Origin", "https://www.icloud.com")
            .header("Referer", "https://www.icloud.com/")
            .content_type("application/octet-stream")
            .send(bytes)?;
        self.cookies.record(&resp);
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(error_for(status, &text));
        }
        match upload_error(&text) {
            Some(message) => Err(PhotosError::Unexpected { status, message }),
            None => Ok(()),
        }
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

/// The upload service reports failures in a 200 response's `errors` list.
fn upload_error(body: &str) -> Option<String> {
    let data: Value = serde_json::from_str(body).ok()?;
    let errors = data.get("errors")?.as_array()?;
    let first = errors.first()?;
    Some(
        first
            .get("message")
            .or_else(|| first.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("iCloud didn't accept the file")
            .to_string(),
    )
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

/// Just enough to index and lay out the library.
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
    let video = matches!(kind, AssetKind::Video { .. })
        .then(|| {
            resource_either(asset, master, "resVidMed")
                .or_else(|| resource_either(asset, master, "resVidSmall"))
                .or_else(|| resource(master, "resOriginal"))
        })
        .flatten();

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
        assert_eq!(
            parse_asset(&live, &live_master).unwrap().kind,
            AssetKind::LivePhoto
        );
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
    fn upload_errors_are_read() {
        assert_eq!(upload_error(r#"{"records":[]}"#), None);
        assert_eq!(upload_error(r#"{"errors":[]}"#), None);
        assert_eq!(
            upload_error(r#"{"errors":[{"message":"Unsupported type"}]}"#).as_deref(),
            Some("Unsupported type")
        );
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
