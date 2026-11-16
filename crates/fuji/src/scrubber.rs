//! Geometry of the timeline scrubber: the strip beside the gallery that maps
//! the whole library onto the height of the window.
//!
//! A position on the track stands for a scroll position: the top of the
//! track is the newest photo, the bottom the oldest. Year labels sit where
//! the gallery would be scrolled to show the start of that year.
//!
//! The track isn't linear: half of it is shared out evenly between months,
//! the other half by the square root of each month's height. One huge month
//! (a big import) doesn't take most of the track, and the quiet months
//! around it keep enough room for their ticks to show.

use std::rc::Rc;

use crate::layout::MonthMark;

/// The share of the track given out evenly, whatever each month holds.
const EVEN_SHARE: f32 = 0.5;

#[derive(Clone, Debug)]
pub struct Track {
    /// Height of the whole gallery.
    pub content: f32,
    /// Height of the visible part.
    pub viewport: f32,
    /// Length of the track on screen.
    pub length: f32,
    /// (scroll position, track position) pairs, both increasing.
    stops: Rc<[(f32, f32)]>,
}

impl Track {
    /// A track over sections starting at `marks` (content positions, top
    /// to bottom).
    pub fn new(content: f32, viewport: f32, length: f32, marks: &[f32]) -> Self {
        let max = (content - viewport).max(0.);
        let mut edges = vec![0.];
        edges.extend(marks.iter().map(|y| y.clamp(0., max)));
        edges.push(max);
        let heights: Vec<f32> = edges.windows(2).map(|w| (w[1] - w[0]).max(0.)).collect();
        // Sections squeezed to nothing (past the end of scrolling) get none.
        let shown = heights.iter().filter(|h| **h > 0.).count().max(1) as f32;
        let root_total: f32 = heights.iter().map(|h| h.sqrt()).sum();
        let weights: Vec<f32> = heights
            .iter()
            .map(|&h| {
                if h <= 0. || root_total <= 0. {
                    0.
                } else {
                    EVEN_SHARE / shown + (1. - EVEN_SHARE) * h.sqrt() / root_total
                }
            })
            .collect();
        let total: f32 = weights.iter().sum();
        let mut stops = Vec::with_capacity(edges.len());
        stops.push((0., 0.));
        let mut sum = 0.;
        for (edge, weight) in edges[1..].iter().zip(&weights) {
            sum += weight;
            let at = if total > 0. { sum / total * length } else { 0. };
            stops.push((*edge, at));
        }
        Self {
            content,
            viewport,
            length,
            stops: stops.into(),
        }
    }

    pub fn max_scroll(&self) -> f32 {
        (self.content - self.viewport).max(0.)
    }

    /// Track position (0 to `length`) for the gallery scrolled to `scroll`.
    pub fn position(&self, scroll: f32) -> f32 {
        if self.max_scroll() <= 0. {
            return 0.;
        }
        interpolate(&self.stops, scroll, |s| s.0, |s| s.1)
    }

    /// Where to scroll the gallery for a track position.
    pub fn scroll_for(&self, position: f32) -> f32 {
        if self.length <= 0. {
            return 0.;
        }
        interpolate(&self.stops, position, |s| s.1, |s| s.0)
    }
}

/// Piecewise-linear lookup: `value` at `x` along increasing `key`s.
fn interpolate(
    stops: &[(f32, f32)],
    x: f32,
    key: impl Fn(&(f32, f32)) -> f32,
    value: impl Fn(&(f32, f32)) -> f32,
) -> f32 {
    let i = stops.partition_point(|s| key(s) <= x);
    if i == 0 {
        return stops.first().map_or(0., &value);
    }
    if i >= stops.len() {
        return stops.last().map_or(0., &value);
    }
    let (a, b) = (stops[i - 1], stops[i]);
    let span = key(&b) - key(&a);
    if span <= 0. {
        value(&b)
    } else {
        value(&a) + (x - key(&a)) / span * (value(&b) - value(&a))
    }
}

/// Years to label, as (track position, year): each year where it starts,
/// skipping any that would sit closer than `min_gap` to the one above.
pub fn year_labels(months: &[MonthMark], track: &Track, min_gap: f32) -> Vec<(f32, i16)> {
    let mut labels: Vec<(f32, i16)> = Vec::new();
    let mut last_year = None;
    for mark in months {
        if last_year == Some(mark.year) {
            continue;
        }
        last_year = Some(mark.year);
        let position = track.position(mark.y);
        if labels
            .last()
            .is_none_or(|(above, _)| position - above >= min_gap)
        {
            labels.push((position, mark.year));
        }
    }
    labels
}

/// Track positions of month starts, at least `min_gap` apart. When there
/// are too many months for one tick each, ticks go on every second, third,
/// sixth or twelfth month of the calendar, so the spacing stays regular
/// instead of losing months here and there.
pub fn month_ticks(months: &[MonthMark], track: &Track, min_gap: f32) -> Vec<f32> {
    let average = track.length / months.len().max(1) as f32;
    // The even share guarantees each month this much room.
    let least = average * EVEN_SHARE;
    let step = [1, 2, 3, 6, 12]
        .into_iter()
        .find(|&step| least * step as f32 >= min_gap)
        .unwrap_or(12);
    let mut ticks: Vec<f32> = Vec::new();
    for mark in months {
        if (mark.month as i32 - 1).rem_euclid(step) != 0 {
            continue;
        }
        let position = track.position(mark.y);
        if ticks.last().is_none_or(|above| position - above >= min_gap) {
            ticks.push(position);
        }
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear() -> Track {
        Track::new(10_500., 500., 400., &[])
    }

    fn mark(y: f32, year: i16, month: i8) -> MonthMark {
        MonthMark { y, year, month }
    }

    #[test]
    fn positions_and_scroll_round_trip() {
        let track = linear();
        assert_eq!(track.max_scroll(), 10_000.);
        assert_eq!(track.position(0.), 0.);
        assert_eq!(track.position(5_000.), 200.);
        assert_eq!(track.position(99_999.), 400.);
        assert_eq!(track.scroll_for(200.), 5_000.);
        assert_eq!(track.scroll_for(-50.), 0.);
        assert_eq!(track.scroll_for(900.), 10_000.);
    }

    #[test]
    fn short_galleries_dont_divide_by_zero() {
        let track = Track::new(300., 500., 400., &[100.]);
        assert_eq!(track.position(100.), 0.);
        assert_eq!(track.scroll_for(200.), 0.);
    }

    #[test]
    fn one_label_per_year_without_crowding() {
        let months = [
            mark(0., 2026, 11),
            mark(1_000., 2026, 10),
            mark(2_500., 2025, 12),
            // 2024 starts 4 px below 2025 on the track: skipped.
            mark(2_600., 2024, 6),
            mark(7_500., 2023, 1),
        ];
        assert_eq!(
            year_labels(&months, &linear(), 16.),
            [(0., 2026), (100., 2025), (300., 2023)]
        );
        assert_eq!(month_ticks(&months, &linear(), 6.), [0., 40., 100., 300.]);
    }

    #[test]
    fn busy_years_keep_every_month() {
        // A year whose months are 9,000 px and 11 x 150 px, then a year of
        // 150 px months, on an 800 px track.
        let mut marks = Vec::new();
        let mut y = 0.;
        let mut months = Vec::new();
        for (i, height) in std::iter::once(9_000.)
            .chain(std::iter::repeat_n(150., 23))
            .enumerate()
        {
            months.push(mark(y, 2025 - (i / 12) as i16, 12 - (i % 12) as i8));
            marks.push(y);
            y += height;
        }
        let track = Track::new(y + 500., 500., 800., &marks[1..]);
        let ticks = month_ticks(&months, &track, 6.);
        assert_eq!(ticks.len(), 24, "{ticks:?}");
        assert!(ticks.windows(2).all(|w| w[1] - w[0] >= 6.));
    }

    #[test]
    fn crowded_tracks_tick_by_the_calendar() {
        // 15 years of months on a 400 px track: about 2 px each.
        let months: Vec<MonthMark> = (0..180)
            .map(|i| mark(i as f32 * 100., 2025 - (i / 12) as i16, 12 - (i % 12) as i8))
            .collect();
        let marks: Vec<f32> = months[1..].iter().map(|m| m.y).collect();
        let track = Track::new(18_000. + 500., 500., 400., &marks);
        let ticks = month_ticks(&months, &track, 6.);
        // Every sixth month: January and July of each year.
        assert_eq!(ticks.len(), 30, "{}", ticks.len());
    }

    #[test]
    fn a_huge_month_doesnt_take_the_track() {
        // Four months: 100, 100, 9,600 and 100 px tall.
        let track = Track::new(10_400., 400., 400., &[100., 200., 9_800.]);
        let ticks: Vec<f32> = [100., 200., 9_800.]
            .iter()
            .map(|&y| track.position(y))
            .collect();
        // sqrt weights 10, 10, ~98, ~1.4 (the last month is mostly past the
        // end of scrolling): the big month gets most, but not 96%.
        assert!(ticks[0] > 25. && ticks[1] > 55., "{ticks:?}");
        assert!(ticks[2] < 400. && ticks[2] > 300.);
        // Still a round trip.
        for y in [0., 150., 5_000., 9_900., 10_000.] {
            assert!((track.scroll_for(track.position(y)) - y).abs() < 0.5, "{y}");
        }
    }
}
