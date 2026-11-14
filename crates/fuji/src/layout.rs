//! Mosaic layout: justified rows grouped under year, month and day headers.
//!
//! Each day's photos flow into rows of about `target_height`; a full row is
//! scaled so its photos, at their real proportions, exactly fill the width.
//! A day's last row keeps the target height and stays left-aligned. Every row
//! has a known position, so the gallery can draw only what's on screen.

use std::ops::Range;

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::index::IndexItem;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub width: f32,
    pub padding_x: f32,
    pub target_height: f32,
    pub gap: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Year(String),
    Month(String),
    Day(String),
    Tiles(Vec<Tile>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    /// Position in the index.
    pub index: usize,
    pub x: f32,
    pub width: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub y: f32,
    pub height: f32,
    pub kind: RowKind,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    pub height: f32,
    pub width: f32,
    /// For each tile row, (row index, first item index); for lookups.
    tile_rows: Vec<(usize, usize)>,
}

pub const YEAR_HEIGHT: f32 = 76.;
pub const MONTH_HEIGHT: f32 = 52.;
pub const DAY_HEIGHT: f32 = 36.;
/// Space after a day's photos.
const DAY_SPACING: f32 = 16.;
const TOP_PADDING: f32 = 4.;
const BOTTOM_PADDING: f32 = 32.;
/// Very wide or tall photos are cropped to these proportions in the mosaic.
const MIN_ASPECT: f32 = 0.5;
const MAX_ASPECT: f32 = 2.4;

impl Layout {
    /// Rows overlapping `top..bottom` (content coordinates).
    pub fn rows_in(&self, top: f32, bottom: f32) -> Range<usize> {
        let start = self.rows.partition_point(|row| row.y + row.height < top);
        let end = self.rows.partition_point(|row| row.y <= bottom);
        start..end.max(start)
    }

    /// The tile row containing item `index`.
    pub fn row_of_item(&self, index: usize) -> Option<&Row> {
        let pos = self.tile_rows.partition_point(|&(_, first)| first <= index);
        let (row, _) = *self.tile_rows.get(pos.checked_sub(1)?)?;
        let row = &self.rows[row];
        match &row.kind {
            RowKind::Tiles(tiles) if tiles.iter().any(|t| t.index == index) => Some(row),
            _ => None,
        }
    }
}

/// Lay out `items` (newest first). `today` and `tz` decide labels like
/// "Today" and which year counts as current.
pub fn build(items: &[IndexItem], params: Params, today: Date, tz: &TimeZone) -> Layout {
    let mut layout = Layout {
        width: params.width,
        ..Layout::default()
    };
    let inner = (params.width - 2. * params.padding_x).max(params.target_height);
    let mut y = TOP_PADDING;

    let mut last_year = None;
    let mut last_month = None;
    let mut start = 0;
    while start < items.len() {
        let date = day_of(items[start].date, tz);
        let mut end = start + 1;
        while end < items.len() && day_of(items[end].date, tz) == date {
            end += 1;
        }

        if let Some(date) = date {
            if last_year != Some(date.year()) {
                if date.year() != today.year() {
                    push(
                        &mut layout,
                        &mut y,
                        YEAR_HEIGHT,
                        RowKind::Year(date.year().to_string()),
                    );
                }
                last_year = Some(date.year());
                last_month = None;
            }
            if last_month != Some(date.month()) {
                push(
                    &mut layout,
                    &mut y,
                    MONTH_HEIGHT,
                    RowKind::Month(month_name(date.month()).into()),
                );
                last_month = Some(date.month());
            }
        } else if last_year.is_some() || start == 0 {
            push(
                &mut layout,
                &mut y,
                MONTH_HEIGHT,
                RowKind::Month("Unknown date".into()),
            );
            last_year = None;
            last_month = None;
        }
        push(
            &mut layout,
            &mut y,
            DAY_HEIGHT,
            RowKind::Day(day_label(date, today)),
        );

        for (height, tiles) in justify(&items[start..end], start, inner, params) {
            let tiles = tiles
                .into_iter()
                .map(|t| Tile {
                    x: t.x + params.padding_x,
                    ..t
                })
                .collect();
            push(&mut layout, &mut y, height, RowKind::Tiles(tiles));
            y += params.gap;
        }
        y += DAY_SPACING - params.gap;
        start = end;
    }
    layout.height = y + BOTTOM_PADDING;
    layout
}

fn push(layout: &mut Layout, y: &mut f32, height: f32, kind: RowKind) {
    if let RowKind::Tiles(tiles) = &kind
        && let Some(first) = tiles.first()
    {
        layout.tile_rows.push((layout.rows.len(), first.index));
    }
    layout.rows.push(Row {
        y: *y,
        height,
        kind,
    });
    *y += height;
}

/// Break one day's items into justified rows: (row height, tiles).
fn justify(items: &[IndexItem], first: usize, width: f32, params: Params) -> Vec<(f32, Vec<Tile>)> {
    let target = params.target_height;
    let gap = params.gap;
    let mut rows = Vec::new();
    let mut row: Vec<(usize, f32)> = Vec::new();
    let mut sum = 0.;

    let finish = |row: &[(usize, f32)], height: f32| {
        let mut x = 0.;
        let mut tiles = Vec::with_capacity(row.len());
        for (i, &(index, aspect)) in row.iter().enumerate() {
            let mut w = aspect * height;
            // Absorb rounding so a full row ends exactly at the edge.
            if i + 1 == row.len() && height < target + 0.01 && x + w > width - 1. {
                w = width - x;
            }
            tiles.push(Tile { index, x, width: w });
            x += w + gap;
        }
        tiles
    };

    for (offset, item) in items.iter().enumerate() {
        let aspect = if item.aspect.is_finite() && item.aspect > 0. {
            item.aspect.clamp(MIN_ASPECT, MAX_ASPECT)
        } else {
            1.
        };
        row.push((first + offset, aspect));
        sum += aspect;
        let gaps = gap * (row.len() - 1) as f32;
        if sum * target + gaps >= width {
            let height = (width - gaps) / sum;
            rows.push((height, finish(&row, height)));
            row.clear();
            sum = 0.;
        }
    }
    if !row.is_empty() {
        rows.push((target, finish(&row, target)));
    }
    rows
}

fn day_of(ms: i64, tz: &TimeZone) -> Option<Date> {
    if ms <= 0 {
        return None;
    }
    let ts = jiff::Timestamp::from_millisecond(ms).ok()?;
    Some(ts.to_zoned(tz.clone()).date())
}

fn month_name(month: i8) -> &'static str {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES[(month.clamp(1, 12) - 1) as usize]
}

fn day_label(date: Option<Date>, today: Date) -> String {
    let Some(date) = date else {
        return "No date".into();
    };
    if date == today {
        return "Today".into();
    }
    if today.yesterday().ok() == Some(date) {
        return "Yesterday".into();
    }
    let weekday = match date.weekday() {
        jiff::civil::Weekday::Monday => "Monday",
        jiff::civil::Weekday::Tuesday => "Tuesday",
        jiff::civil::Weekday::Wednesday => "Wednesday",
        jiff::civil::Weekday::Thursday => "Thursday",
        jiff::civil::Weekday::Friday => "Friday",
        jiff::civil::Weekday::Saturday => "Saturday",
        jiff::civil::Weekday::Sunday => "Sunday",
    };
    format!("{weekday} {} {}", date.day(), month_name(date.month()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Kind;
    use jiff::civil::date;

    const PARAMS: Params = Params {
        width: 1000.,
        padding_x: 0.,
        target_height: 200.,
        gap: 4.,
    };

    fn item(day: Date, aspect: f32) -> IndexItem {
        let ms = day
            .at(12, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
            .as_millisecond();
        IndexItem {
            id: format!("{day}-{aspect}"),
            date: ms,
            aspect,
            kind: Kind::Photo,
            favorite: false,
        }
    }

    fn tile_rows(layout: &Layout) -> Vec<(f32, Vec<Tile>)> {
        layout
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Tiles(t) => Some((r.height, t.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_rows_fill_the_width_exactly() {
        let today = date(2026, 11, 14);
        let items: Vec<_> = (0..12)
            .map(|i| item(today, [1.5, 0.75, 1.0, 1.33][i % 4]))
            .collect();
        let layout = build(&items, PARAMS, today, &TimeZone::UTC);
        let rows = tile_rows(&layout);
        let (last, full) = rows.split_last().unwrap();
        for (height, tiles) in full {
            let end = tiles.last().map(|t| t.x + t.width).unwrap();
            assert!((end - 1000.).abs() < 0.5, "row ends at {end}");
            assert!(*height <= 200. + 0.01, "full rows shrink to fit: {height}");
            for pair in tiles.windows(2) {
                assert!((pair[1].x - (pair[0].x + pair[0].width + 4.)).abs() < 0.01);
            }
        }
        assert!((last.0 - 200.).abs() < 0.01, "last row keeps target height");
    }

    #[test]
    fn photos_keep_their_proportions() {
        let today = date(2026, 11, 14);
        let items = vec![item(today, 1.5), item(today, 0.75), item(today, 1.0)];
        let layout = build(&items, PARAMS, today, &TimeZone::UTC);
        for (height, tiles) in tile_rows(&layout) {
            for tile in tiles {
                let aspect = items[tile.index].aspect;
                assert!((tile.width / height - aspect).abs() < 0.01);
            }
        }
    }

    #[test]
    fn headers_for_years_months_and_days() {
        let today = date(2026, 11, 14);
        let items = vec![
            item(today, 1.),
            item(date(2026, 11, 13), 1.),
            item(date(2026, 11, 2), 1.),
            item(date(2026, 10, 30), 1.),
            item(date(2025, 12, 24), 1.),
        ];
        let layout = build(&items, PARAMS, today, &TimeZone::UTC);
        let headers: Vec<String> = layout
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Year(s) => Some(format!("Y {s}")),
                RowKind::Month(s) => Some(format!("M {s}")),
                RowKind::Day(s) => Some(format!("D {s}")),
                RowKind::Tiles(_) => None,
            })
            .collect();
        assert_eq!(
            headers,
            [
                "M November",
                "D Today",
                "D Yesterday",
                "D Monday 2 November",
                "M October",
                "D Friday 30 October",
                "Y 2025",
                "M December",
                "D Wednesday 24 December",
            ]
        );
    }

    #[test]
    fn rows_are_stacked_and_searchable() {
        let today = date(2026, 11, 14);
        let items: Vec<_> = (0..40)
            .map(|i| {
                item(
                    date(2026, 11, 14 - (i / 8) as i8),
                    1.0 + (i % 3) as f32 * 0.3,
                )
            })
            .collect();
        let layout = build(&items, PARAMS, today, &TimeZone::UTC);
        for pair in layout.rows.windows(2) {
            assert!(pair[1].y >= pair[0].y + pair[0].height - 0.01);
        }
        // Every item appears exactly once and can be found.
        let mut seen = vec![false; items.len()];
        for (_, tiles) in tile_rows(&layout) {
            for t in tiles {
                assert!(!seen[t.index]);
                seen[t.index] = true;
            }
        }
        assert!(seen.iter().all(|s| *s));
        for i in 0..items.len() {
            assert!(layout.row_of_item(i).is_some(), "item {i}");
        }
        // Viewport queries return the overlapping rows only.
        let range = layout.rows_in(300., 700.);
        assert!(!range.is_empty());
        for row in &layout.rows[range] {
            assert!(row.y <= 700. && row.y + row.height >= 300.);
        }
    }

    #[test]
    fn extreme_shapes_are_clamped() {
        let today = date(2026, 11, 14);
        let items = vec![item(today, 10.), item(today, 0.05), item(today, f32::NAN)];
        let layout = build(&items, PARAMS, today, &TimeZone::UTC);
        for (height, tiles) in tile_rows(&layout) {
            for t in tiles {
                let a = t.width / height;
                assert!((MIN_ASPECT - 0.01..=MAX_ASPECT + 0.01).contains(&a), "{a}");
            }
        }
    }

    #[test]
    fn unknown_dates_are_grouped() {
        let today = date(2026, 11, 14);
        let mut undated = item(today, 1.);
        undated.date = 0;
        let layout = build(&[item(today, 1.), undated], PARAMS, today, &TimeZone::UTC);
        assert!(
            layout
                .rows
                .iter()
                .any(|r| r.kind == RowKind::Month("Unknown date".into()))
        );
        assert!(
            layout
                .rows
                .iter()
                .any(|r| r.kind == RowKind::Day("No date".into()))
        );
    }
}
