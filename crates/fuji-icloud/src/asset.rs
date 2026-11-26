/// Stable identifier for an asset (iCloud's `CPLAsset` record name).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Photo,
    /// A photo with a short paired video.
    LivePhoto,
    Video {
        duration_secs: u32,
    },
}

/// One downloadable rendition of an asset (thumbnail, original, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    /// Uniform type identifier, for example `public.jpeg` or `public.heic`.
    pub file_type: String,
}

/// One photo or video in the library.
#[derive(Clone, Debug)]
pub struct Asset {
    pub id: AssetId,
    pub kind: AssetKind,
    pub filename: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Width / height as displayed (rotation applied). From the rendered
    /// thumbnail when known, since originals may be stored sideways.
    pub aspect: f32,
    pub favorite: bool,
    /// Capture time, milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// Small JPEG for the grid.
    pub thumbnail: Option<Resource>,
    /// Larger JPEG, good for full-screen viewing.
    pub preview: Option<Resource>,
    /// The original file (HEIC, JPEG, RAW, MOV, ...).
    pub original: Option<Resource>,
    /// The `CPLMaster` record behind it, for looking up its details.
    pub master: Option<String>,
    /// For videos, a rendition to play (H.264 at a medium size when iCloud
    /// made one, else the original); for Live Photos, their motion.
    pub video: Option<Resource>,
}

impl Asset {
    pub fn aspect_ratio(&self) -> f32 {
        if self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}
