//! A bounded image cache for the photo grid and the viewer.
//!
//! GPUI's default cache keeps every decoded image forever; with a large
//! library that would grow without limit as you scroll. This keeps the most
//! recently drawn `capacity` images on the GPU and drops the rest.
//!
//! It can also decode images from disk at a lower height than the file's
//! (see [`LruImageCache::set_max_height`]): zoomed out, the grid's tiles are
//! small, and a full-size decode of each thumbnail would hold several times
//! the memory the tile can show. When the height changes, the image already
//! decoded stays on screen until the new one is ready.
//!
//! Based on GPUI's `examples/image_gallery.rs` (Apache-2.0, Zed Industries).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use futures::FutureExt as _;
use gpui::{
    App, Asset as _, Context, ImageCacheError, ImageCacheItem, RenderImage, Resource, Window, hash,
};
use smallvec::smallvec;

struct Entry {
    item: ImageCacheItem,
    /// The height it was decoded at (0: as the file is).
    height: u32,
    used: u64,
    /// The same image at the height now wanted, while it decodes.
    next: Option<(u32, ImageCacheItem)>,
}

pub struct LruImageCache {
    capacity: usize,
    /// Decode files no taller than this (0: as they are).
    max_height: u32,
    /// Crop files to their middle square.
    square: bool,
    tick: u64,
    items: HashMap<u64, Entry>,
}

impl LruImageCache {
    pub fn new(capacity: usize, cx: &mut Context<Self>) -> Self {
        cx.on_release(|cache, cx| {
            for (_, entry) in std::mem::take(&mut cache.items) {
                for mut item in std::iter::once(entry.item).chain(entry.next.map(|(_, i)| i)) {
                    if let Some(Ok(image)) = item.get() {
                        cx.drop_image(image, None);
                    }
                }
            }
        })
        .detach();
        Self {
            capacity,
            max_height: 0,
            square: false,
            tick: 0,
            items: HashMap::with_capacity(capacity),
        }
    }

    /// Change how many images are kept; a smaller cache shrinks as it's used.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
    }

    /// Decode images from files at most `height` pixels tall (0: as they
    /// are). Images already decoded at another height are redone as drawn.
    pub fn set_max_height(&mut self, height: u32) {
        self.max_height = height;
    }

    /// Crop images from files to their middle square, for square tiles
    /// with rounded corners: GPUI rounds the corners of the image as
    /// fitted, which a cropping fit puts outside the tile.
    pub fn set_square(&mut self, square: bool) {
        self.square = square;
    }

    fn evict_oldest(&mut self, window: &mut Window, cx: &mut App) {
        let Some(oldest) = self
            .items
            .iter()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| *key)
        else {
            return;
        };
        if let Some(entry) = self.items.remove(&oldest) {
            for mut item in std::iter::once(entry.item).chain(entry.next.map(|(_, i)| i)) {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, Some(window));
                }
            }
        }
    }

    /// Start decoding `resource` (at `height`, for a file) and redraw the
    /// view drawing it once that's done.
    fn start(
        resource: &Resource,
        height: u32,
        square: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> ImageCacheItem {
        let task = match resource {
            Resource::Path(path) if height > 0 => {
                let path = path.clone();
                cx.background_executor()
                    .spawn(async move { decode_scaled(&path, height, square) })
                    .shared()
            }
            _ => {
                let load = gpui::ImgResourceLoader::load(resource.clone(), cx);
                cx.background_executor().spawn(load).shared()
            }
        };
        let view = window.current_view();
        let done = task.clone();
        window
            .spawn(cx, async move |cx| {
                _ = done.await;
                cx.on_next_frame(move |_, cx| cx.notify(view));
            })
            .detach();
        ImageCacheItem::Loading(task)
    }
}

impl gpui::ImageCache for LruImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.tick += 1;
        let key = hash(resource);
        // Only files are decoded smaller.
        let want = match resource {
            Resource::Path(_) => self.max_height,
            _ => 0,
        };
        if let Some(entry) = self.items.get_mut(&key) {
            entry.used = self.tick;
            if entry.height == want {
                entry.next = None;
                return entry.item.get();
            }
            match &mut entry.next {
                Some((height, next)) if *height == want => {
                    if let Some(Ok(image)) = next.get() {
                        // The new size is ready: show it, free the old one.
                        let (_, next) = entry.next.take()?;
                        let mut old = std::mem::replace(&mut entry.item, next);
                        entry.height = want;
                        if let Some(Ok(old)) = old.get() {
                            cx.drop_image(old, Some(window));
                        }
                        return Some(Ok(image));
                    }
                }
                _ => {
                    if let Some((_, mut stale)) = entry.next.take()
                        && let Some(Ok(image)) = stale.get()
                    {
                        cx.drop_image(image, Some(window));
                    }
                    entry.next = Some((want, Self::start(resource, want, self.square, window, cx)));
                }
            }
            // Meanwhile, the old size (if it ever finished).
            return match entry.item.get() {
                Some(Ok(image)) => Some(Ok(image)),
                _ => None,
            };
        }

        while self.items.len() >= self.capacity {
            self.evict_oldest(window, cx);
        }
        let item = Self::start(resource, want, self.square, window, cx);
        self.items.insert(
            key,
            Entry {
                item,
                height: want,
                used: self.tick,
                next: None,
            },
        );
        None
    }
}

/// Decode an image file no taller than `height`, as GPUI keeps images
/// (BGRA).
fn decode_scaled(
    path: &Path,
    height: u32,
    square: bool,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    let io = |err: std::io::Error| ImageCacheError::Io(Arc::new(err));
    let bytes = std::fs::read(path).map_err(io)?;
    let mut image =
        image::load_from_memory(&bytes).map_err(|err| io(std::io::Error::other(err)))?;
    if square {
        let side = image.width().min(image.height());
        image = image.crop_imm(
            (image.width() - side) / 2,
            (image.height() - side) / 2,
            side,
            side,
        );
    }
    let image = if image.height() > height {
        // Wide panoramas keep their full width at this height.
        image.thumbnail(u32::MAX, height)
    } else {
        image
    };
    let mut pixels = image.into_rgba8();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(smallvec![image::Frame::new(
        pixels
    )])))
}

/// The decode height for tiles `tile_height` logical pixels tall: a power
/// of two with room to spare, so small changes in zoom don't redo them.
pub fn height_for(tile_height: f32, scale: f32) -> u32 {
    let device = (tile_height * scale * 1.25).ceil().max(1.) as u32;
    device.next_power_of_two().clamp(64, 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_heights_step_in_powers_of_two() {
        assert_eq!(height_for(40., 1.), 64);
        assert_eq!(height_for(100., 1.), 128);
        assert_eq!(height_for(110., 1.5), 256);
        assert_eq!(height_for(240., 2.), 1024);
        assert_eq!(height_for(2_000., 2.), 1024);
    }

    #[test]
    fn decodes_smaller_and_as_bgra() {
        let dir = std::env::temp_dir().join(format!("fuji-decode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("red.png");
        image::RgbaImage::from_pixel(400, 200, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let image = decode_scaled(&path, 64, false).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (128, 64));
        let bytes = image.as_bytes(0).unwrap();
        assert_eq!(&bytes[..4], &[0, 0, 255, 255]);
    }
}
