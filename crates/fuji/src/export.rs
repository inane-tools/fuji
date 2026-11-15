//! Files handed to other apps: what a copy puts on the clipboard and what a
//! drag carries out of the window.
//!
//! Other apps need real files with sensible names, so the cached image is
//! copied to `<temp>/fuji-export/<id>/<name>`. That folder is emptied of
//! anything older than a day at launch.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fuji_icloud::{Asset, AssetId, Resource};

use crate::thumbs;

const KEPT_FOR: Duration = Duration::from_secs(24 * 60 * 60);

pub fn dir() -> PathBuf {
    std::env::temp_dir().join("fuji-export")
}

/// Whether `path` is one of our own exported files.
pub fn is_export(path: &Path) -> bool {
    path.starts_with(dir())
}

/// Remove exports from earlier days. Blocking; call off the UI thread.
pub fn clean() {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age > KEPT_FOR));
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Put a copy of `cached` in the export folder under `name`.
pub fn place(cached: &Path, id: &AssetId, name: &str) -> std::io::Result<PathBuf> {
    // One folder per item, so equal names from different items don't clash.
    let folder = thumbs::path_for(&dir(), id).with_extension("");
    std::fs::create_dir_all(&folder)?;
    let target = folder.join(name);
    if !target.exists() {
        std::fs::copy(cached, &target)?;
    }
    Ok(target)
}

/// Where the original of `asset` goes when it's exported directly.
pub fn original_path(asset: &Asset, original: &Resource) -> std::io::Result<PathBuf> {
    let folder = thumbs::path_for(&dir(), &asset.id).with_extension("");
    std::fs::create_dir_all(&folder)?;
    Ok(folder.join(original_name(asset, original)))
}

fn extension(file_type: &str) -> &'static str {
    match file_type {
        "public.heic" => "HEIC",
        "public.heif" => "HEIF",
        "public.png" => "PNG",
        "com.compuserve.gif" => "GIF",
        "com.apple.quicktime-movie" => "MOV",
        "public.mpeg-4" => "MP4",
        "com.adobe.raw-image" => "DNG",
        _ => "JPG",
    }
}

/// The name to save the original file under.
pub fn original_name(asset: &Asset, original: &Resource) -> String {
    if let Some(name) = asset.filename.as_deref().filter(|n| !n.is_empty()) {
        return safe_name(name);
    }
    format!("photo.{}", extension(&original.file_type))
}

/// The name for a rendition of `asset` (a JPEG made from a HEIC, say): the
/// original's name with the rendition's extension.
pub fn rendition_name(asset: &Asset, rendition: &Resource) -> String {
    let ext = extension(&rendition.file_type);
    let stem = asset
        .filename
        .as_deref()
        .and_then(|name| Path::new(name).file_stem()?.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("photo");
    safe_name(&format!("{stem}.{ext}"))
}

/// Names come from iCloud; keep them from escaping the folder.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        "photo".into()
    } else {
        cleaned.into()
    }
}

/// Whether `bytes` start like a JPEG.
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Re-encode a JPEG as PNG (upright), which is what most apps want from an
/// image on the clipboard. Slow for big photos: call off the UI thread.
pub fn png_from_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

    let mut decoder = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg)
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut picture = DynamicImage::from_decoder(decoder).ok()?;
    picture.apply_orientation(orientation);
    let mut png = Vec::new();
    let encoder = PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Sub);
    picture.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuji_icloud::AssetKind;

    fn resource(file_type: &str) -> Resource {
        Resource {
            url: String::new(),
            width: 0,
            height: 0,
            size: 0,
            file_type: file_type.into(),
        }
    }

    fn asset(filename: Option<&str>) -> Asset {
        Asset {
            id: AssetId("a".into()),
            kind: AssetKind::Photo,
            filename: filename.map(str::to_string),
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

    #[test]
    fn names_fall_back_to_type() {
        let heic = resource("public.heic");
        assert_eq!(original_name(&asset(None), &heic), "photo.HEIC");
        assert_eq!(
            original_name(&asset(Some("IMG_1234.HEIC")), &heic),
            "IMG_1234.HEIC"
        );
    }

    #[test]
    fn renditions_keep_the_stem() {
        let jpeg = resource("public.jpeg");
        assert_eq!(
            rendition_name(&asset(Some("IMG_1234.HEIC")), &jpeg),
            "IMG_1234.JPG"
        );
        assert_eq!(rendition_name(&asset(None), &jpeg), "photo.JPG");
    }

    #[test]
    fn names_cant_leave_the_folder() {
        assert_eq!(safe_name("..\\..\\evil.jpg"), "_.._evil.jpg");
        assert_eq!(safe_name("a/b:c.jpg"), "a_b_c.jpg");
        assert_eq!(safe_name("  .. "), "photo");
    }

    #[test]
    fn jpeg_becomes_png() {
        let mut jpeg = Vec::new();
        image::RgbImage::from_pixel(8, 6, image::Rgb([200, 40, 40]))
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        assert!(is_jpeg(&jpeg));
        let png = png_from_jpeg(&jpeg).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        let back = image::load_from_memory(&png).unwrap();
        assert_eq!((back.width(), back.height()), (8, 6));
        assert!(png_from_jpeg(b"not a jpeg").is_none());
    }
}
