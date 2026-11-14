//! Motion helpers: one easing curve and one set of durations for the app,
//! plus a value that eases toward a target frame by frame.
//!
//! "Reduce motion" in Settings turns every animation into a jump.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{Animation, ease_out_quint};

static REDUCED: AtomicBool = AtomicBool::new(false);

pub fn set_reduced(reduced: bool) {
    REDUCED.store(reduced, Ordering::Relaxed);
}

pub fn reduced() -> bool {
    REDUCED.load(Ordering::Relaxed)
}

/// Quick feedback: menus, toasts, selection.
pub const FAST: u64 = 140;
/// The default for things appearing.
pub const NORMAL: u64 = 220;

/// A one-shot ease-out animation of `ms` milliseconds.
pub fn ease(ms: u64) -> Animation {
    let ms = if reduced() { 1 } else { ms };
    Animation::new(Duration::from_millis(ms)).with_easing(ease_out_quint())
}

/// A number that eases toward a target, for things that can be redirected
/// mid-flight (scrolling, zooming, sliding panels). Call [`Eased::tick`] once
/// per frame and ask for another frame while it returns `true`.
#[derive(Clone, Copy, Debug)]
pub struct Eased {
    value: f32,
    target: f32,
    last: Option<Instant>,
}

impl Eased {
    pub const fn new(value: f32) -> Self {
        Self {
            value,
            target: value,
            last: None,
        }
    }

    pub fn get(&self) -> f32 {
        self.value
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    /// Go to `value` at once.
    pub fn jump(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.last = None;
    }

    pub fn is_moving(&self) -> bool {
        self.value != self.target
    }

    /// Move toward the target. `rate` is how quickly (per second; higher is
    /// snappier); within `epsilon` it lands. Returns whether it's still moving.
    pub fn tick(&mut self, rate: f32, epsilon: f32) -> bool {
        self.tick_at(Instant::now(), rate, epsilon)
    }

    fn tick_at(&mut self, now: Instant, rate: f32, epsilon: f32) -> bool {
        if (self.value - self.target).abs() <= epsilon || reduced() {
            self.value = self.target;
            self.last = None;
            return false;
        }
        let dt = self
            .last
            .map_or(1. / 60., |t| (now - t).as_secs_f32())
            .clamp(0.001, 0.05);
        self.last = Some(now);
        self.value += (self.target - self.value) * (1. - (-dt * rate).exp());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eases_to_the_target_and_stops() {
        let mut v = Eased::new(0.);
        v.set(100.);
        let start = Instant::now();
        let mut frames = 0;
        let mut last = 0.;
        while v.tick_at(start + Duration::from_millis(16 * (frames + 1)), 16., 0.5) {
            assert!(v.get() > last, "moves forward every frame");
            last = v.get();
            frames += 1;
            assert!(frames < 200, "never settles");
        }
        assert_eq!(v.get(), 100.);
        assert!(!v.is_moving());
        assert!(frames > 5, "takes several frames: {frames}");
    }

    #[test]
    fn jump_lands_at_once() {
        let mut v = Eased::new(0.);
        v.set(50.);
        v.jump(10.);
        assert_eq!((v.get(), v.target()), (10., 10.));
        assert!(!v.tick(16., 0.5));
    }
}
