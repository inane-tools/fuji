//! Full records (download URLs), fetched by page of rank, and thumbnails.

use super::*;

impl PhotoGrid {
    // ---- Full records (download URLs) ------------------------------------

    pub(super) fn request_page(&mut self, page: usize) {
        if self.pages_requested.insert(page) {
            self.page_queue.push(page);
            // Forget the stalest requests; they're asked for again if needed.
            while self.page_queue.len() > MAX_QUEUED_PAGES {
                let stale = self.page_queue.remove(0);
                self.pages_requested.remove(&stale);
            }
        }
    }

    pub(super) fn start_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let count = self.album_len();
        let start = page * PAGE;
        if start >= count {
            self.pages_requested.remove(&page);
            return;
        }
        let limit = PAGE.min(count - start);
        let client = self.client.clone();
        let album = self.album.clone();
        let generation = self.generation;
        let epoch = self.page_epoch;
        self.pages_in_flight += 1;

        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.fetch(&album, start, limit)).await;
            this.update(cx, |this, cx| {
                if this.generation != generation || this.page_epoch != epoch {
                    this.pump(cx);
                    return;
                }
                this.pages_in_flight -= 1;
                match result {
                    Some(Ok(assets)) => this.store_page(page, assets),
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => {
                        this.pages_requested.remove(&page);
                        for queued in this.page_queue.drain(..) {
                            this.pages_requested.remove(&queued);
                        }
                        this.fail(err.to_string(), cx);
                    }
                    None => {
                        this.pages_requested.remove(&page);
                        this.fail("Loading photos failed.".into(), cx);
                    }
                }
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn store_page(&mut self, page: usize, assets: Vec<Asset>) {
        let ids = assets.iter().map(|a| a.id.clone()).collect();
        for asset in assets {
            // Fresh URLs: let failed thumbnails try again.
            self.thumbs_failed.remove(&asset.id);
            self.assets.insert(asset.id.clone(), asset);
        }
        self.page_ids.insert(page, ids);
        self.page_lru.retain(|p| *p != page);
        self.page_lru.push_back(page);
        // Keep memory bounded: forget the least recently loaded pages.
        while self.page_lru.len() > MAX_PAGES {
            let Some(old) = self.page_lru.pop_front() else {
                break;
            };
            for id in self.page_ids.remove(&old).unwrap_or_default() {
                self.assets.remove(&id);
            }
            self.pages_requested.remove(&old);
        }
    }

    // ---- Thumbnails ---------------------------------------------------------

    /// List the thumbnail cache once, so cached thumbnails show without
    /// touching the disk (or the network) per tile.
    pub(super) fn list_disk_thumbs(&mut self, cx: &mut Context<Self>) {
        let dir = self.thumb_dir.clone();
        cx.spawn(async move |this, cx| {
            let names = cx
                .background_spawn(async move {
                    std::fs::read_dir(&*dir)
                        .map(|entries| {
                            entries
                                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                                .collect::<HashSet<_>>()
                        })
                        .unwrap_or_default()
                })
                .await;
            this.update(cx, |this, cx| {
                this.disk_thumbs = Some(Arc::new(names));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn on_disk(&self, path: &Path) -> bool {
        let (Some(names), Some(name)) = (&self.disk_thumbs, path.file_name()) else {
            return false;
        };
        name.to_str().is_some_and(|n| names.contains(n))
    }

    /// Fetch these items' thumbnails too (the viewer's filmstrip).
    pub(crate) fn want_thumbnails(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        self.wanted_thumbs = indices;
        self.request_visible(&[], cx);
    }

    /// Called while rendering with the items on screen, top to bottom.
    pub(super) fn request_visible(&mut self, visible: &[usize], cx: &mut Context<Self>) {
        if self.error.is_some() {
            return;
        }
        let mut queue = Vec::new();
        let wanted = std::mem::take(&mut self.wanted_thumbs);
        for &ix in visible.iter().chain(&wanted) {
            let Some(item) = self.index.get(ix) else {
                continue;
            };
            let id = item.asset_id();
            if self.thumbs_ready.contains_key(&id)
                || self.thumbs_failed.contains(&id)
                || self.thumbs_in_flight.contains(&id)
            {
                continue;
            }
            let path = thumbs::path_for(&self.thumb_dir, &id);
            if self.on_disk(&path) {
                self.thumbs_ready.insert(id, path.into());
                continue;
            }
            if self.disk_thumbs.is_none() {
                // Wait for the cache listing before fetching anything.
                continue;
            }
            match self.assets.get(&id).and_then(|a| a.thumbnail.clone()) {
                Some(resource) => queue.push((id, resource)),
                None => self.request_page(self.rank(ix) / PAGE),
            }
        }
        self.wanted_thumbs = wanted;
        // Visible order, first popped first.
        queue.reverse();
        self.thumb_queue = queue;
        self.pump(cx);
    }

    /// Start queued requests, up to the concurrency limits.
    pub(super) fn pump(&mut self, cx: &mut Context<Self>) {
        while self.pages_in_flight < MAX_PAGE_REQUESTS {
            let Some(page) = self.page_queue.pop() else {
                break;
            };
            self.start_page(page, cx);
        }
        while self.thumbs_in_flight.len() < MAX_THUMB_REQUESTS {
            let Some((id, resource)) = self.thumb_queue.pop() else {
                break;
            };
            self.start_thumb(id, resource, cx);
        }
    }

    pub(super) fn start_thumb(&mut self, id: AssetId, resource: Resource, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let dir = self.thumb_dir.clone();
        let generation = self.generation;
        self.thumbs_in_flight.insert(id.clone());

        cx.spawn(async move |this, cx| {
            let task_id = id.clone();
            let result = net::run(move || thumbs::ensure(&client, &dir, &task_id, &resource)).await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.thumbs_in_flight.remove(&id);
                }
                match result {
                    Some(Ok(path)) => {
                        this.thumbs_ready.insert(id, path.into());
                    }
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    _ => {
                        this.thumbs_failed.insert(id);
                    }
                }
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
