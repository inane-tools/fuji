//! Mosaic layout: justified rows grouped under month and day headers.
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
    /// Extra room on the right, for the timeline scrubber.
    pub padding_right: f32,
    pub target_height: f32,
    pub gap: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    /// "March 2024".
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
    /// The first item the row is about: a header's day or month starts
    /// there, a tile row's first photo is it.
    pub first: usize,
    pub kind: RowKind,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    pub height: f32,
    pub width: f32,
    /// Where each month starts, top to bottom (newest first).
    pub months: Vec<MonthMark>,
    /// For each tile row, (row index, first item index); for lookups.
    tile_rows: Vec<(usize, usize)>,
}

/// The top of a month's section, for the timeline scrubber.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonthMark {
    pub y: f32,
    pub year: i16,
    pub month: i8,
}

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

    /// The first tile row that ends below `y`: what's at the top of a
    /// viewport scrolled to `y`.
    pub fn tile_row_at(&self, y: f32) -> Option<&Row> {
        let start = self.rows.partition_point(|row| row.y + row.height <= y);
        self.rows[start..]
            .iter()
            .find(|row| matches!(row.kind, RowKind::Tiles(_)))
    }

    /// The month showing at content position `y`.
    pub fn month_at(&self, y: f32) -> Option<MonthMark> {
        let after = self.months.partition_point(|mark| mark.y <= y);
        self.months
            .get(after.saturating_sub(1))
            .or(self.months.first())
            .copied()
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

/// A stretch of the timeline to act on at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    Day,
    Month,
    Year,
}

/// The run of items around `index` from the same day, month or year.
/// `days` is newest first, so such items are next to each other.
pub fn span(days: &[Option<Date>], index: usize, span: Span) -> Range<usize> {
    let Some(&day) = days.get(index) else {
        return 0..0;
    };
    let same = |other: &Option<Date>| match (day, *other) {
        (Some(a), Some(b)) => match span {
            Span::Day => a == b,
            Span::Month => (a.year(), a.month()) == (b.year(), b.month()),
            Span::Year => a.year() == b.year(),
        },
        (None, None) => true,
        _ => false,
    };
    let start = days[..index]
        .iter()
        .rposition(|d| !same(d))
        .map_or(0, |i| i + 1);
    let end = days[index..]
        .iter()
        .position(|d| !same(d))
        .map_or(days.len(), |i| index + i);
    start..end
}

/// The local calendar day of every item (`None` when the date is unknown).
/// Worked out once per index, so re-laying out (resizing, zooming) is cheap.
pub fn days(items: &[IndexItem], tz: &TimeZone) -> Vec<Option<Date>> {
    items.iter().map(|item| day_of(item.date, tz)).collect()
}

/// Lay out `items` (newest first), whose days are `days` (see [`days`]).
/// `today` decides labels like "Today".
pub fn build(items: &[IndexItem], days: &[Option<Date>], params: Params, today: Date) -> Layout {
    debug_assert_eq!(items.len(), days.len());
    let mut layout = Layout {
        width: params.width,
        ..Layout::default()
    };
    let inner =
        (params.width - 2. * params.padding_x - params.padding_right).max(params.target_height);
    let mut y = TOP_PADDING;

    let mut last_month = None;
    let mut seen_unknown = false;
    let mut start = 0;
    let count = items.len().min(days.len());
    while start < count {
        let date = days[start];
        let mut end = start + 1;
        while end < count && days[end] == date {
            end += 1;
        }

        if let Some(date) = date {
            if last_month != Some((date.year(), date.month())) {
                layout.months.push(MonthMark {
                    y,
                    year: date.year(),
                    month: date.month(),
                });
                push(
                    &mut layout,
                    &mut y,
                    MONTH_HEIGHT,
                    start,
                    RowKind::Month(format!("{} {}", month_name(date.month()), date.year())),
                );
                last_month = Some((date.year(), date.month()));
            }
        } else if !seen_unknown || last_month.is_some() {
            push(
                &mut layout,
                &mut y,
                MONTH_HEIGHT,
                start,
                RowKind::Month("Unknown date".into()),
            );
            seen_unknown = true;
            last_month = None;
        }
        push(
            &mut layout,
            &mut y,
            DAY_HEIGHT,
            start,
            RowKind::Day(day_label(date, today)),
        );

        for (height, tiles) in justify(&items[start..end], start, inner, params) {
            let tiles: Vec<Tile> = tiles
                .into_iter()
                .map(|t| Tile {
                    x: t.x + params.padding_x,
                    ..t
                })
                .collect();
            let first = tiles.first().map_or(start, |t| t.index);
            push(&mut layout, &mut y, height, first, RowKind::Tiles(tiles));
            y += params.gap;
        }
        y += DAY_SPACING - params.gap;
        start = end;
    }
    layout.height = y + BOTTOM_PADDING;
    layout
}

fn push(layout: &mut Layout, y: &mut f32, height: f32, first: usize, kind: RowKind) {
    if let RowKind::Tiles(_) = &kind {
        layout.tile_rows.push((layout.rows.len(), first));
    }
    layout.rows.push(Row {
        y: *y,
        height,
        first,
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

pub fn month_name(month: i8) -> &'static str {
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
        padding_right: 0.,
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
            name: None,
        }
    }

    fn build_utc(items: &[IndexItem], params: Params, today: Date) -> Layout {
        build(items, &days(items, &TimeZone::UTC), params, today)
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
        let layout = build_utc(&items, PARAMS, today);
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
        let layout = build_utc(&items, PARAMS, today);
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
        let layout = build_utc(&items, PARAMS, today);
        let headers: Vec<String> = layout
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Month(s) => Some(format!("M {s}")),
                RowKind::Day(s) => Some(format!("D {s}")),
                RowKind::Tiles(_) => None,
            })
            .collect();
        assert_eq!(
            headers,
            [
                "M November 2026",
                "D Today",
                "D Yesterday",
                "D Monday 2 November",
                "M October 2026",
                "D Friday 30 October",
                "M December 2025",
                "D Wednesday 24 December",
            ]
        );
        // One mark per month, top to bottom, findable by position.
        let marks: Vec<(i16, i8)> = layout.months.iter().map(|m| (m.year, m.month)).collect();
        assert_eq!(marks, [(2026, 11), (2026, 10), (2025, 12)]);
        assert_eq!(layout.month_at(0.).map(|m| m.month), Some(11));
        let october = layout.months[1];
        assert_eq!(layout.month_at(october.y + 1.), Some(october));
        assert_eq!(layout.month_at(layout.height).map(|m| m.year), Some(2025));
        // Days, months and years as runs of items.
        let days = days(&items, &TimeZone::UTC);
        assert_eq!(span(&days, 1, Span::Day), 1..2);
        assert_eq!(span(&days, 1, Span::Month), 0..3);
        assert_eq!(span(&days, 3, Span::Month), 3..4);
        assert_eq!(span(&days, 0, Span::Year), 0..4);
        assert_eq!(span(&days, 4, Span::Year), 4..5);
        assert_eq!(span(&days, 9, Span::Day), 0..0);
        // Headers know where their day or month starts.
        let firsts: Vec<usize> = layout
            .rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Month(_)))
            .map(|r| r.first)
            .collect();
        assert_eq!(firsts, [0, 3, 4]);
        // The first photos below a position.
        let row = layout.tile_row_at(october.y).unwrap();
        assert!(matches!(&row.kind, RowKind::Tiles(t) if t[0].index == 3));
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
        let layout = build_utc(&items, PARAMS, today);
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
        let layout = build_utc(&items, PARAMS, today);
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
        let layout = build_utc(&[item(today, 1.), undated], PARAMS, today);
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
