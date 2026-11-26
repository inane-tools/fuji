//! Scrolling, zooming, and where things are on the mosaic.

use super::*;

impl PhotoGrid {
    // ---- Scrolling and zooming ----------------------------------------------

    pub(super) fn top(&self) -> f32 {
        -f32::from(self.scroll.offset().y)
    }

    /// Scroll to `top` at once.
    pub(super) fn jump_to(&mut self, top: f32) {
        let top = top.max(0.);
        self.scroller.jump(top);
        self.scroll.set_offset(point(px(0.), px(-top)));
    }

    /// Ease the scroll position toward `top`.
    pub(super) fn scroll_toward(&mut self, top: f32, cx: &mut Context<Self>) {
        if !self.scroller.is_moving() {
            self.scroller.jump(self.top());
        }
        self.scroller.set(top.clamp(0., self.max_scroll));
        self.show_scrubber_date(cx);
        cx.notify();
    }

    /// Show the scrubber's date for a moment.
    pub(super) fn show_scrubber_date(&mut self, cx: &mut Context<Self>) {
        self.scrub.scrolled = Some(Instant::now());
        self.scrub.generation += 1;
        let generation = self.scrub.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCRUBBER_LINGER).await;
            this.update(cx, |this, cx| {
                if this.scrub.generation == generation {
                    this.scrub.scrolled = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn on_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        let zoom = event.modifiers.control || event.modifiers.platform;
        match event.delta {
            ScrollDelta::Lines(lines) => {
                let notches = lines.y / LINES_PER_NOTCH;
                if zoom {
                    self.zoom_by(ZOOM_STEP.powf(notches), Some(event.position), cx);
                } else {
                    let from = if self.scroller.is_moving() {
                        self.scroller.target()
                    } else {
                        self.top()
                    };
                    self.scroll_toward(from - notches * WHEEL_STEP, cx);
                }
            }
            // A touchpad already scrolls smoothly: follow the fingers.
            ScrollDelta::Pixels(delta) => {
                if zoom {
                    let factor = (f32::from(delta.y) * 0.005).exp();
                    self.zoom_by(factor, Some(event.position), cx);
                } else {
                    let top = (self.top() - f32::from(delta.y)).clamp(0., self.max_scroll);
                    self.jump_to(top);
                    self.show_scrubber_date(cx);
                    cx.notify();
                }
            }
        }
        cx.stop_propagation();
    }

    /// Change the size of the mosaic's rows, keeping the photo under `at`
    /// (window coordinates; the middle of the gallery if `None`) in place.
    pub(super) fn zoom_by(
        &mut self,
        factor: f32,
        at: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let target = (self.row_height.target() * factor)
            .clamp(settings::MIN_ROW_HEIGHT, settings::MAX_ROW_HEIGHT);
        self.zoom_to(target, at, cx);
    }

    /// The photo row at `screen_y` (from the gallery's top), to hold there
    /// while the layout changes.
    pub(super) fn anchor_at(&self, screen_y: f32) -> Option<ZoomAnchor> {
        let layout = self.layout.as_ref()?;
        let y = self.top() + screen_y;
        let row = layout.tile_row_at(y)?;
        let RowKind::Tiles(tiles) = &row.kind else {
            return None;
        };
        Some(ZoomAnchor {
            index: tiles.first()?.index,
            fraction: ((y - row.y) / row.height).clamp(0., 1.),
            screen_y,
        })
    }

    pub(super) fn zoom_to(
        &mut self,
        target: f32,
        at: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if (target - self.row_height.target()).abs() < 0.01 {
            return;
        }
        if self.zoom_anchor.is_none() {
            let bounds = self.scroll.bounds();
            let viewport = f32::from(bounds.size.height);
            let screen_y = at
                .map(|p| f32::from(p.y - bounds.top()))
                .filter(|y| (0. ..=viewport).contains(y))
                .unwrap_or(viewport / 2.);
            self.zoom_anchor = self.anchor_at(screen_y);
        }
        // The anchor decides the scroll position while zooming.
        self.scroller.jump(self.top());
        self.row_height.set(target);
        cx.notify();
    }

    pub(super) fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(ZOOM_STEP, None, cx);
    }

    pub(super) fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1. / ZOOM_STEP, None, cx);
    }

    pub(super) fn zoom_reset(&mut self, _: &ZoomReset, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(settings::DEFAULT_ROW_HEIGHT, None, cx);
    }

    /// Scroll to the scrubber position under the pointer.
    pub(super) fn scrub_to(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(track) = self.track.clone() else {
            return;
        };
        let y = f32::from(pointer.y - self.scroll.bounds().top()) - SCRUBBER_TOP;
        self.scrub.hover = Some(y.clamp(0., track.length));
        self.scroll_toward(track.scroll_for(y), cx);
    }
}
