//! Letterboxing, zoom and pan math for the viewer, in logical pixels.
//!
//! Zoom 1.0 means "fit": the whole image, letterboxed, centred. Pan is the
//! offset of the image's centre from the viewport's centre. Kept free of UI
//! types so it can be tested on its own.

/// What part of the image is shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl View {
    pub const FIT: View = View {
        zoom: 1.,
        pan_x: 0.,
        pan_y: 0.,
    };

    pub fn is_fit(&self) -> bool {
        (self.zoom - 1.).abs() < 0.001
    }

    /// Move `fraction` of the way towards `target`.
    pub fn approach(&self, target: View, fraction: f32) -> View {
        let lerp = |a: f32, b: f32| a + (b - a) * fraction;
        View {
            zoom: lerp(self.zoom, target.zoom),
            pan_x: lerp(self.pan_x, target.pan_x),
            pan_y: lerp(self.pan_y, target.pan_y),
        }
    }

    pub fn close_to(&self, other: View) -> bool {
        (self.zoom - other.zoom).abs() < 0.0005
            && (self.pan_x - other.pan_x).abs() < 0.25
            && (self.pan_y - other.pan_y).abs() < 0.25
    }
}

/// The viewport and the image being shown in it.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub viewport_w: f32,
    pub viewport_h: f32,
    /// Natural image size, in image pixels (only the ratio and, for 1:1
    /// zoom, the size matter).
    pub image_w: f32,
    pub image_h: f32,
    /// Space kept around the image when it fits.
    pub margin: f32,
    /// Device pixels per logical pixel.
    pub scale_factor: f32,
}

/// Where to draw the image, relative to the viewport's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Geometry {
    /// Logical pixels per image pixel at zoom 1 (fit, letterboxed).
    pub fn fit_scale(&self) -> f32 {
        let room_w = (self.viewport_w - 2. * self.margin).max(1.);
        let room_h = (self.viewport_h - 2. * self.margin).max(1.);
        (room_w / self.image_w.max(1.)).min(room_h / self.image_h.max(1.))
    }

    /// Zoom at which one image pixel covers one screen pixel.
    pub fn actual_size_zoom(&self) -> f32 {
        1. / self.scale_factor.max(0.1) / self.fit_scale()
    }

    /// Up to 4x actual size, and never less than 4x fit (small images).
    pub fn max_zoom(&self) -> f32 {
        (self.actual_size_zoom() * 4.).max(4.)
    }

    pub fn image_size(&self, zoom: f32) -> (f32, f32) {
        let scale = self.fit_scale() * zoom;
        (self.image_w * scale, self.image_h * scale)
    }

    /// Keep zoom in range and the image covering the viewport where it can:
    /// no gaps on an axis where the image is larger than the viewport, and
    /// centred on an axis where it's smaller.
    pub fn clamp(&self, view: View) -> View {
        let zoom = view.zoom.clamp(1., self.max_zoom());
        let (w, h) = self.image_size(zoom);
        let limit = |size: f32, viewport: f32, pan: f32| {
            let room = (size - viewport) / 2.;
            if room <= 0. {
                0.
            } else {
                pan.clamp(-room, room)
            }
        };
        View {
            zoom,
            pan_x: limit(w, self.viewport_w, view.pan_x),
            pan_y: limit(h, self.viewport_h, view.pan_y),
        }
    }

    /// Zoom to `zoom`, keeping the image point under `(px, py)` (relative to
    /// the viewport centre) where it is.
    pub fn zoom_at(&self, view: View, zoom: f32, px: f32, py: f32) -> View {
        let zoom = zoom.clamp(1., self.max_zoom());
        let k = zoom / view.zoom;
        self.clamp(View {
            zoom,
            pan_x: px - (px - view.pan_x) * k,
            pan_y: py - (py - view.pan_y) * k,
        })
    }

    /// Double-click: from fit, zoom in on the point; otherwise back to fit.
    pub fn toggle_at(&self, view: View, px: f32, py: f32) -> View {
        if view.is_fit() {
            // Actual size if that's a real step up, else a comfortable 2.5x.
            let target = if self.actual_size_zoom() > 1.5 {
                self.actual_size_zoom()
            } else {
                2.5
            };
            self.zoom_at(view, target, px, py)
        } else {
            View::FIT
        }
    }

    pub fn rect(&self, view: View) -> Rect {
        let (w, h) = self.image_size(view.zoom);
        Rect {
            x: self.viewport_w / 2. + view.pan_x - w / 2.,
            y: self.viewport_h / 2. + view.pan_y - h / 2.,
            w,
            h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(image_w: f32, image_h: f32) -> Geometry {
        Geometry {
            viewport_w: 1000.,
            viewport_h: 800.,
            image_w,
            image_h,
            margin: 0.,
            scale_factor: 1.,
        }
    }

    #[test]
    fn landscape_is_letterboxed_top_and_bottom() {
        let g = geometry(4000., 2000.);
        let r = g.rect(View::FIT);
        assert_eq!(
            r,
            Rect {
                x: 0.,
                y: 150.,
                w: 1000.,
                h: 500.
            }
        );
    }

    #[test]
    fn portrait_is_pillarboxed_left_and_right() {
        let g = geometry(3000., 4000.);
        let r = g.rect(View::FIT);
        assert_eq!(
            r,
            Rect {
                x: 200.,
                y: 0.,
                w: 600.,
                h: 800.
            }
        );
    }

    #[test]
    fn margin_is_respected_when_fitting() {
        let g = Geometry {
            margin: 20.,
            ..geometry(1000., 1000.)
        };
        let r = g.rect(View::FIT);
        assert_eq!((r.w, r.h), (760., 760.));
        assert_eq!((r.x, r.y), (120., 20.));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let g = geometry(4000., 3000.);
        // Cursor 100px right and 50px down from the centre.
        let (px, py) = (100., 50.);
        let before = g.rect(View::FIT);
        let image_x = (500. + px - before.x) / before.w;
        let image_y = (400. + py - before.y) / before.h;

        let view = g.zoom_at(View::FIT, 3., px, py);
        let after = g.rect(view);
        assert!((after.x + image_x * after.w - (500. + px)).abs() < 0.01);
        assert!((after.y + image_y * after.h - (400. + py)).abs() < 0.01);
    }

    #[test]
    fn panning_never_shows_a_gap() {
        let g = geometry(4000., 3000.);
        let view = g.clamp(View {
            zoom: 2.,
            pan_x: 10_000.,
            pan_y: -10_000.,
        });
        let r = g.rect(view);
        assert!(r.x <= 0. && r.x + r.w >= 1000.);
        assert!(r.y <= 0. && r.y + r.h >= 800.);
    }

    #[test]
    fn small_axis_stays_centred() {
        // Wide panorama: at 1.5x it's still shorter than the viewport.
        let g = geometry(8000., 1000.);
        let view = g.clamp(View {
            zoom: 1.5,
            pan_x: 0.,
            pan_y: 300.,
        });
        assert_eq!(view.pan_y, 0.);
    }

    #[test]
    fn zoom_is_limited() {
        let g = geometry(4000., 3000.);
        assert_eq!(
            g.clamp(View {
                zoom: 0.2,
                ..View::FIT
            })
            .zoom,
            1.
        );
        let max = g.max_zoom();
        assert_eq!(
            g.clamp(View {
                zoom: 1000.,
                ..View::FIT
            })
            .zoom,
            max
        );
        // 4000px wide shown in 1000px: actual size is 4x fit, max is 16x.
        assert!((g.actual_size_zoom() - 4.).abs() < 0.001);
        assert!((max - 16.).abs() < 0.001);
    }

    #[test]
    fn high_dpi_actual_size_accounts_for_scale() {
        let g = Geometry {
            scale_factor: 2.,
            ..geometry(4000., 3000.)
        };
        assert!((g.actual_size_zoom() - 2.).abs() < 0.001);
    }

    #[test]
    fn double_click_toggles() {
        let g = geometry(4000., 3000.);
        let zoomed = g.toggle_at(View::FIT, 0., 0.);
        assert!((zoomed.zoom - g.actual_size_zoom()).abs() < 0.001);
        assert_eq!(g.toggle_at(zoomed, 0., 0.), View::FIT);
        // A small image goes to 2.5x instead of its (tiny) actual size.
        let small = geometry(500., 400.);
        assert!((small.toggle_at(View::FIT, 0., 0.).zoom - 2.5).abs() < 0.001);
    }

    #[test]
    fn approach_converges() {
        let target = View {
            zoom: 3.,
            pan_x: 50.,
            pan_y: -20.,
        };
        let mut view = View::FIT;
        for _ in 0..60 {
            view = view.approach(target, 0.3);
        }
        assert!(view.close_to(target));
    }
}
