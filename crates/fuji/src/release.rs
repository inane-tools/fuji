//! Release codenames.
//!
//! Each minor version is named after a print from Hokusai's Thirty-six Views
//! of Mount Fuji, in the series' order (the 36 prints, then the ten added
//! later): every release is another view of Fuji. The name advances with
//! the minor version; patch releases share their minor's codename.

/// (codename: a short form of the print's Japanese title in romaji, the
/// print's English title), in series order.
const VIEWS: &[(&str, &str)] = &[
    ("Kanagawa-oki", "The Great Wave off Kanagawa"),
    ("Gaifū Kaisei", "Fine Wind, Clear Morning"),
    ("Sanka Hakuu", "Thunderstorm Beneath the Summit"),
    ("Mannen-bashi", "Under Mannen Bridge at Fukagawa"),
    ("Sundai", "Sundai, Edo"),
    ("Enza-no-matsu", "The Cushion Pine at Aoyama"),
    ("Senju", "Senju in Musashi Province"),
    ("Tama-gawa", "Tama River in Musashi Province"),
    ("Inume-tōge", "Inume Pass in Kai Province"),
    ("Fujimigahara", "Fujimigahara Field in Owari Province"),
    ("Hongan-ji", "Asakusa Hongan Temple in Edo"),
    ("Tsukuda-jima", "Tsukuda Island in Edo"),
    ("Shichirigahama", "Shichirigahama in Sagami Province"),
    ("Umezawa", "Umezawa in Sagami Province"),
    ("Kajikazawa", "Kajikazawa in Kai Province"),
    ("Mishima-goe", "Mishima Pass in Kai Province"),
    ("Suwa-ko", "A View of Mount Fuji Across Lake Suwa"),
    ("Ejiri", "Ejiri in Suruga Province"),
    ("Tōtōmi", "In the Mountains of Tōtōmi Province"),
    ("Ushibori", "Ushibori in Hitachi Province"),
    (
        "Mitsui-mise",
        "A Sketch of the Mitsui Shop in Suruga in Edo",
    ),
    (
        "Ryōgoku-bashi",
        "Sunset Across Ryōgoku Bridge from Ommayagashi",
    ),
    ("Sazaidō", "Sazaidō at Gohyakurakan Temple"),
    ("Koishikawa", "Morning After a Snowfall at Koishikawa"),
    ("Shimomeguro", "Shimomeguro"),
    ("Onden", "Watermill at Onden"),
    ("Enoshima", "Enoshima in Sagami Province"),
    ("Tago-no-ura", "Sketch of Tago Bay, Ejiri Along the Tōkaidō"),
    ("Yoshida", "Yoshida Along the Tōkaidō"),
    ("Kazusa", "Kazusa Province Sea Route"),
    ("Nihonbashi", "Nihonbashi in Edo"),
    ("Sekiya-no-sato", "Sekiya Village on the Sumida River"),
    ("Noboto-ura", "Noboto Bay"),
    ("Hakone-kosui", "Hakone Lake in Sagami Province"),
    ("Misaka", "Reflection from Misaka Pass in Kai Province"),
    ("Hodogaya", "Hodogaya Along the Tōkaidō"),
    ("Tatekawa", "Tate River from the Timberyard at Honjo"),
    (
        "Hana-machi",
        "A View of Mount Fuji from the Pleasure District at Senju",
    ),
    ("Goten'yama", "Gotenyama, Shinagawa Along the Tōkaidō"),
    ("Nakahara", "Nakahara in Sagami Province"),
    ("Isawa", "Dawn at Isawa in Kai Province"),
    ("Minobu-gawa", "The Back of Fuji from the Minobu River"),
    ("Ōno-shinden", "Ōno Field in Suruga Province"),
    ("Katakura", "Tea Plantation at Katakura in Suruga Province"),
    ("Kanaya", "Kanaya Along the Tōkaidō"),
    ("Shōjin Tozan", "Climbing on Fuji"),
];

/// The view a version is named after: 1.0 is the first, 1.1 the second, and
/// 2.0 the eleventh (ten minors per major, as in Cherry). `None` past the end.
pub fn view(version: &str) -> Option<(&'static str, &'static str)> {
    let mut parts = version.split('.').map(|n| n.parse::<usize>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let index = major.saturating_sub(1) * 10 + minor;
    VIEWS.get(index).copied()
}

/// "1.4.2" → "1.4".
pub fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codenames_follow_the_minor_version() {
        assert_eq!(view("1.0.0").map(|v| v.0), Some("Kanagawa-oki"));
        assert_eq!(view("1.0.3").map(|v| v.0), Some("Kanagawa-oki"));
        assert_eq!(view("1.1.0").map(|v| v.0), Some("Gaifū Kaisei"));
        assert_eq!(view("2.0.0").map(|v| v.0), Some("Hongan-ji"));
        assert_eq!(view("9.9.0"), None);
        assert_eq!(VIEWS.len(), 46);
    }

    #[test]
    fn short_versions() {
        assert_eq!(major_minor("1.0.0"), "1.0");
        assert_eq!(major_minor("12.3"), "12.3");
    }
}
