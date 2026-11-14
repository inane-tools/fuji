//! Geometry of the timeline scrubber: the strip beside the gallery that maps
//! the whole library onto the height of the window.
//!
//! A position on the track stands for a scroll position: the top of the
//! track is the newest photo, the bottom the oldest. Year labels sit where
//! the gallery would be scrolled to show the start of that year.

use crate::layout::MonthMark;

#[derive(Clone, Copy, Debug)]
pub struct Track {
    /// Height of the whole gallery.
    pub content: f32,
    /// Height of the visible part.
    pub viewport: f32,
    /// Length of the track on screen.
    pub length: f32,
}

impl Track {
    pub fn max_scroll(&self) -> f32 {
        (self.content - self.viewport).max(0.)
    }

    /// Track position (0 to `length`) for the gallery scrolled to `scroll`.
    pub fn position(&self, scroll: f32) -> f32 {
        let max = self.max_scroll();
        if max <= 0. {
            return 0.;
        }
        (scroll / max).clamp(0., 1.) * self.length
    }

    /// Where to scroll the gallery for a track position.
    pub fn scroll_for(&self, position: f32) -> f32 {
        if self.length <= 0. {
            return 0.;
        }
        (position / self.length).clamp(0., 1.) * self.max_scroll()
    }
}

/// Years to label, as (track position, year): each year where it starts,
/// skipping any that would sit closer than `min_gap` to the one above.
pub fn year_labels(months: &[MonthMark], track: Track, min_gap: f32) -> Vec<(f32, i16)> {
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

/// Track positions of month starts, at least `min_gap` apart.
pub fn month_ticks(months: &[MonthMark], track: Track, min_gap: f32) -> Vec<f32> {
    let mut ticks: Vec<f32> = Vec::new();
    for mark in months {
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

    const TRACK: Track = Track {
        content: 10_500.,
        viewport: 500.,
        length: 400.,
    };

    fn mark(y: f32, year: i16, month: i8) -> MonthMark {
        MonthMark { y, year, month }
    }

    #[test]
    fn positions_and_scroll_round_trip() {
        assert_eq!(TRACK.max_scroll(), 10_000.);
        assert_eq!(TRACK.position(0.), 0.);
        assert_eq!(TRACK.position(5_000.), 200.);
        assert_eq!(TRACK.position(99_999.), 400.);
        assert_eq!(TRACK.scroll_for(200.), 5_000.);
        assert_eq!(TRACK.scroll_for(-50.), 0.);
        assert_eq!(TRACK.scroll_for(900.), 10_000.);
    }

    #[test]
    fn short_galleries_dont_divide_by_zero() {
        let track = Track {
            content: 300.,
            viewport: 500.,
            length: 400.,
        };
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
            year_labels(&months, TRACK, 16.),
            [(0., 2026), (100., 2025), (300., 2023)]
        );
        assert_eq!(month_ticks(&months, TRACK, 6.), [0., 40., 100., 300.]);
    }
}
