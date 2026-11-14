//! A bounded image cache for the photo grid.
//!
//! GPUI's default cache keeps every decoded image forever; with a large
//! library that would grow without limit as you scroll. This keeps the most
//! recently drawn `capacity` images on the GPU and drops the rest.
//! Based on GPUI's `examples/image_gallery.rs` (Apache-2.0, Zed Industries).

use std::collections::HashMap;
use std::sync::Arc;

use futures::FutureExt as _;
use gpui::{
    App, Asset as _, Context, ImageCacheError, ImageCacheItem, RenderImage, Resource, Window, hash,
};

pub struct LruImageCache {
    capacity: usize,
    tick: u64,
    items: HashMap<u64, (ImageCacheItem, u64)>,
}

impl LruImageCache {
    pub fn new(capacity: usize, cx: &mut Context<Self>) -> Self {
        cx.on_release(|cache, cx| {
            for (_, (mut item, _)) in std::mem::take(&mut cache.items) {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        Self {
            capacity,
            tick: 0,
            items: HashMap::with_capacity(capacity),
        }
    }

    fn evict_oldest(&mut self, window: &mut Window, cx: &mut App) {
        let Some(oldest) = self
            .items
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(key, _)| *key)
        else {
            return;
        };
        if let Some((mut item, _)) = self.items.remove(&oldest)
            && let Some(Ok(image)) = item.get()
        {
            cx.drop_image(image, Some(window));
        }
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
        if let Some((item, used)) = self.items.get_mut(&key) {
            *used = self.tick;
            return item.get();
        }

        while self.items.len() >= self.capacity {
            self.evict_oldest(window, cx);
        }

        let load = gpui::ImgResourceLoader::load(resource.clone(), cx);
        let task = cx.background_executor().spawn(load).shared();
        self.items
            .insert(key, (ImageCacheItem::Loading(task.clone()), self.tick));

        // Redraw the view once the image is decoded.
        let view = window.current_view();
        window
            .spawn(cx, async move |cx| {
                _ = task.await;
                cx.on_next_frame(move |_, cx| cx.notify(view));
            })
            .detach();
        None
    }
}
