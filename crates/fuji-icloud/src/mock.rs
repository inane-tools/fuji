use crate::{Asset, AssetId, AssetKind, Library};

/// A fake library with a fixed number of generated assets.
pub struct MockLibrary {
    count: usize,
}

impl MockLibrary {
    pub fn new(count: usize) -> Self {
        Self { count }
    }
}

impl Library for MockLibrary {
    fn len(&self) -> usize {
        self.count
    }

    fn asset(&self, index: usize) -> Option<Asset> {
        if index >= self.count {
            return None;
        }
        // Cheap deterministic variety, so the grid looks like a real library.
        let kind = match index % 11 {
            3 => AssetKind::Video {
                duration_secs: (index % 90) as u32 + 5,
            },
            7 => AssetKind::LivePhoto,
            _ => AssetKind::Photo,
        };
        let (width, height) = if index.is_multiple_of(4) {
            (3024, 4032)
        } else {
            (4032, 3024)
        };
        Some(Asset {
            id: AssetId(format!("mock-{index}")),
            kind,
            width,
            height,
            favorite: index.is_multiple_of(13),
            created_at: 1_763_000_000 - index as i64 * 3_600,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_are_bounded() {
        let lib = MockLibrary::new(3);
        assert_eq!(lib.len(), 3);
        assert!(lib.asset(2).is_some());
        assert!(lib.asset(3).is_none());
    }
}
