//! What took a photo and how: camera, lens, exposure, place and the app that
//! saved it, for the viewer's info panel.
//!
//! iCloud keeps the image's own metadata on its `CPLMaster` record as
//! `mediaMetaDataEnc`: a binary property list in the shape Apple's ImageIO
//! uses, with `{TIFF}`, `{Exif}` and `{GPS}` dictionaries. `importedByDisplayNameEnc`
//! names the app that saved it ("Camera", "WhatsApp"). Both are base64; for
//! accounts without Advanced Data Protection they aren't encrypted further.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use plist::{Dictionary, Value as Plist};
use serde_json::Value;

/// The keys [`crate::PhotosClient::details`] asks for.
pub(crate) const DETAIL_KEYS: &[&str] = &["mediaMetaDataEnc", "importedByDisplayNameEnc"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotoDetails {
    /// "Apple iPhone 17".
    pub camera: Option<String>,
    /// The lens as the camera names it, without the camera's name in front:
    /// "Back dual wide camera 2.22mm f/2.2".
    pub lens: Option<String>,
    /// "iOS 27.2", or the software as written.
    pub software: Option<String>,
    pub iso: Option<u32>,
    /// Focal length in millimetres, as on a 35 mm camera.
    pub focal_length_35mm: Option<u32>,
    pub f_number: Option<f64>,
    /// Seconds.
    pub exposure_time: Option<f64>,
    /// Exposure compensation in stops.
    pub exposure_bias: Option<f64>,
    pub flash_fired: bool,
    pub location: Option<Location>,
    /// The app that saved it: "Camera", "WhatsApp", "Screenshots".
    pub saved_from: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    /// Degrees, north positive.
    pub latitude: f64,
    /// Degrees, east positive.
    pub longitude: f64,
    /// Metres above sea level.
    pub altitude: Option<f64>,
}

impl PhotoDetails {
    /// Whether there's anything to show beyond what the asset already says.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Details from a `CPLMaster` record's fields.
pub(crate) fn from_fields(fields: &Value) -> PhotoDetails {
    let bytes = |name: &str| {
        fields
            .get(name)?
            .get("value")?
            .as_str()
            .and_then(|v| BASE64.decode(v).ok())
    };
    let mut details = bytes("mediaMetaDataEnc")
        .map(|b| from_metadata(&b))
        .unwrap_or_default();
    details.saved_from = bytes("importedByDisplayNameEnc")
        .and_then(|b| String::from_utf8(b).ok())
        .map(|name| clean(&name))
        .filter(|name| !name.is_empty())
        // iOS's home screen saves screenshots.
        .map(|name| match name.as_str() {
            "SpringBoard" => "Screenshots".to_string(),
            _ => name,
        });
    details
}

/// Details from ImageIO-style metadata (a binary or XML property list).
pub fn from_metadata(bytes: &[u8]) -> PhotoDetails {
    let Ok(Plist::Dictionary(meta)) = plist::from_bytes::<Plist>(bytes) else {
        return PhotoDetails::default();
    };
    let empty = Dictionary::new();
    let section = |name: &str| {
        meta.get(name)
            .and_then(Plist::as_dictionary)
            .unwrap_or(&empty)
    };
    let (tiff, exif, gps) = (section("{TIFF}"), section("{Exif}"), section("{GPS}"));

    let make = text(tiff, "Make");
    let model = text(tiff, "Model");
    let camera = match (&make, &model) {
        // Most models say who made them; don't say it twice.
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model.clone())
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (None, Some(model)) => Some(model.clone()),
        (Some(make), None) => Some(make.clone()),
        (None, None) => None,
    };
    let lens = text(exif, "LensModel").map(|lens| {
        // "iPhone 17 back dual wide camera 2.22mm f/2.2" → "Back dual wide …".
        let trimmed = model
            .as_deref()
            .and_then(|model| lens.strip_prefix(model))
            .map(str::trim_start)
            .unwrap_or(&lens);
        let mut chars = trimmed.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => lens.clone(),
        }
    });
    let software = text(tiff, "Software").map(|software| {
        // Apple devices write just the OS version.
        let is_version = software.chars().all(|c| c.is_ascii_digit() || c == '.');
        match model.as_deref() {
            Some(m) if is_version && m.starts_with("iPhone") => format!("iOS {software}"),
            Some(m) if is_version && m.starts_with("iPad") => format!("iPadOS {software}"),
            _ => software,
        }
    });

    let iso = match exif.get("ISOSpeedRatings") {
        Some(Plist::Array(values)) => values.first().and_then(number),
        Some(value) => number(value),
        None => None,
    }
    .map(|n| n.round() as u32);

    let location = (|| {
        let mut latitude = gps.get("Latitude").and_then(number)?;
        let mut longitude = gps.get("Longitude").and_then(number)?;
        if text(gps, "LatitudeRef").as_deref() == Some("S") {
            latitude = -latitude;
        }
        if text(gps, "LongitudeRef").as_deref() == Some("W") {
            longitude = -longitude;
        }
        let altitude = gps.get("Altitude").and_then(number).map(|a| {
            // AltitudeRef 1: below sea level.
            if gps.get("AltitudeRef").and_then(number) == Some(1.) {
                -a
            } else {
                a
            }
        });
        Some(Location {
            latitude,
            longitude,
            altitude,
        })
    })();

    PhotoDetails {
        camera,
        lens,
        software,
        iso,
        focal_length_35mm: exif
            .get("FocalLenIn35mmFilm")
            .and_then(number)
            .filter(|f| *f > 0.)
            .map(|f| f.round() as u32),
        f_number: exif.get("FNumber").and_then(number).filter(|f| *f > 0.),
        exposure_time: exif
            .get("ExposureTime")
            .and_then(number)
            .filter(|t| *t > 0.),
        exposure_bias: exif.get("ExposureBiasValue").and_then(number),
        // Bit 0 of the EXIF flash value: it fired.
        flash_fired: exif
            .get("Flash")
            .and_then(number)
            .is_some_and(|f| (f as i64) & 1 == 1),
        location,
        saved_from: None,
    }
}

fn text(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key)
        .and_then(Plist::as_string)
        .map(clean)
        .filter(|s| !s.is_empty())
}

fn number(value: &Plist) -> Option<f64> {
    match value {
        Plist::Real(n) => Some(*n),
        Plist::Integer(n) => n.as_signed().map(|n| n as f64),
        _ => None,
    }
}

/// Without the invisible direction marks some apps put around names.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'))
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> Vec<u8> {
        let mut tiff = Dictionary::new();
        tiff.insert("Make".into(), "Apple".into());
        tiff.insert("Model".into(), "iPhone 17".into());
        tiff.insert("Software".into(), "27.2".into());
        let mut exif = Dictionary::new();
        exif.insert(
            "LensModel".into(),
            "iPhone 17 back dual wide camera 2.22mm f/2.2".into(),
        );
        exif.insert("FocalLenIn35mmFilm".into(), 27.into());
        exif.insert("FNumber".into(), 2.2.into());
        exif.insert("ExposureTime".into(), (1. / 11.).into());
        exif.insert("ISOSpeedRatings".into(), Plist::Array(vec![1250.into()]));
        exif.insert("ExposureBiasValue".into(), (-0.07421875).into());
        exif.insert("Flash".into(), 16.into());
        let mut gps = Dictionary::new();
        gps.insert("Latitude".into(), 60.17.into());
        gps.insert("LatitudeRef".into(), "N".into());
        gps.insert("Longitude".into(), 24.94.into());
        gps.insert("LongitudeRef".into(), "W".into());
        gps.insert("Altitude".into(), 12.5.into());
        gps.insert("AltitudeRef".into(), 0.into());
        let mut meta = Dictionary::new();
        meta.insert("{TIFF}".into(), Plist::Dictionary(tiff));
        meta.insert("{Exif}".into(), Plist::Dictionary(exif));
        meta.insert("{GPS}".into(), Plist::Dictionary(gps));
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &Plist::Dictionary(meta)).unwrap();
        out
    }

    #[test]
    fn reads_a_phone_photo() {
        let d = from_metadata(&metadata());
        assert_eq!(d.camera.as_deref(), Some("Apple iPhone 17"));
        assert_eq!(
            d.lens.as_deref(),
            Some("Back dual wide camera 2.22mm f/2.2")
        );
        assert_eq!(d.software.as_deref(), Some("iOS 27.2"));
        assert_eq!(d.iso, Some(1250));
        assert_eq!(d.focal_length_35mm, Some(27));
        assert_eq!(d.f_number, Some(2.2));
        assert!(!d.flash_fired);
        let place = d.location.unwrap();
        assert_eq!((place.latitude, place.longitude), (60.17, -24.94));
        assert_eq!(place.altitude, Some(12.5));
    }

    #[test]
    fn reads_the_saving_app() {
        let fields = serde_json::json!({
            "mediaMetaDataEnc": { "type": "ENCRYPTED_BYTES", "value": BASE64.encode(metadata()) },
            "importedByDisplayNameEnc": { "type": "ENCRYPTED_BYTES", "value": BASE64.encode("\u{200e}WhatsApp") },
        });
        let d = from_fields(&fields);
        assert_eq!(d.saved_from.as_deref(), Some("WhatsApp"));
        let screenshot = serde_json::json!({
            "importedByDisplayNameEnc": { "type": "ENCRYPTED_BYTES", "value": BASE64.encode("SpringBoard") },
        });
        assert_eq!(
            from_fields(&screenshot).saved_from.as_deref(),
            Some("Screenshots")
        );
        assert_eq!(d.iso, Some(1250));
    }

    #[test]
    fn nothing_from_nothing() {
        assert!(from_metadata(b"not a plist").is_empty());
        assert!(from_fields(&serde_json::json!({})).is_empty());
    }
}
