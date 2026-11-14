/// Stable identifier for an asset (iCloud's record name).
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

/// One photo or video in the library.
#[derive(Clone, Debug)]
pub struct Asset {
    pub id: AssetId,
    pub kind: AssetKind,
    pub width: u32,
    pub height: u32,
    pub favorite: bool,
    /// Capture time, seconds since the Unix epoch.
    pub created_at: i64,
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
