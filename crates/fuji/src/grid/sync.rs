//! The album's index: loading it from disk, scanning iCloud, and keeping it
//! current from the change feed.

use super::*;

impl PhotoGrid {
    // ---- Album index ------------------------------------------------------

    pub(super) fn load_album(&mut self, cx: &mut Context<Self>) {
        // Leaving the album ends the chance to undo a deletion.
        self.commit_trash(cx);
        self.pending_delete = None;
        self.generation += 1;
        self.token = None;
        self.syncing = None;
        self.page_epoch += 1;
        self.count_gap = 0;
        self.gap_misses = 0;
        self.recent.clear();
        self.state = LoadState::Loading;
        self.index.clear();
        self.days.clear();
        if let Some(search) = self.search.as_mut() {
            search.all.clear();
            search.all_days.clear();
            search.ranks.clear();
        }
        self.scan = None;
        self.layout = None;
        self.layout_dirty = true;
        self.assets.clear();
        self.page_ids.clear();
        self.page_lru.clear();
        self.pages_requested.clear();
        self.page_queue.clear();
        self.pages_in_flight = 0;
        self.thumbs_failed.clear();
        self.thumbs_in_flight.clear();
        self.thumb_queue.clear();
        self.error = None;
        self.viewer = None;
        self.selected.clear();
        self.select_anchor = None;
        self.menu = None;
        self.zoom_anchor = None;
        self.scroller.jump(0.);
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();

        let generation = self.generation;
        let path = index::cache_path(&self.account_dir, &self.album);
        cx.spawn(async move |this, cx| {
            // Cached index first, so the gallery shows right away.
            let cached = cx
                .background_spawn(async move {
                    let cached = index::load(&path)?;
                    let days = layout::days(&cached.items, &jiff::tz::TimeZone::system());
                    Some((cached, days))
                })
                .await;
            let had_cache = cached.is_some();
            this.update(cx, |this, cx| {
                if this.generation == generation
                    && let Some((cached, days)) = cached
                {
                    this.token = cached.token.filter(|_| this.follows_changes());
                    this.count_gap = cached.gap;
                    this.show_album_items(cached.items, days);
                    this.state = LoadState::Ready;
                    cx.notify();
                }
                // Then refresh from iCloud.
                if this.generation == generation {
                    this.refresh(!had_cache, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Albums kept current from the change feed rather than rescanned:
    /// those whose membership shows in an item's own fields.
    pub(super) fn follows_changes(&self) -> bool {
        matches!(self.album, Album::All | Album::Favorites | Album::Videos)
    }

    /// Whether a library item belongs in this album.
    pub(super) fn album_keeps(&self, item: &IndexItem) -> bool {
        match self.album {
            Album::Favorites => item.favorite,
            Album::Videos => matches!(item.kind, Kind::Video(_)),
            _ => true,
        }
    }

    /// Full records loaded so far, to change in place.
    pub(crate) fn assets_mut(&mut self) -> impl Iterator<Item = &mut Asset> {
        self.assets.values_mut()
    }

    pub(crate) fn viewer_open(&self) -> bool {
        self.viewer.is_some()
    }

    /// Swap in new versions of items that stay where they are (a flag
    /// changed), in the album and in what a search shows.
    pub(crate) fn replace_items(&mut self, items: Vec<IndexItem>) {
        if items.is_empty() {
            return;
        }
        let by_id: HashMap<&str, &IndexItem> =
            items.iter().map(|item| (item.id.as_str(), item)).collect();
        let lists = std::iter::once(&mut self.index)
            .chain(self.search.as_mut().map(|search| &mut search.all));
        for list in lists {
            for item in list.iter_mut() {
                if let Some(new) = by_id.get(item.id.as_str()) {
                    *item = (*new).clone();
                }
            }
        }
    }

    /// The whole album (not just what a search shows).
    pub(crate) fn album_items(&self) -> &[IndexItem] {
        match &self.search {
            Some(search) => &search.all,
            None => &self.index,
        }
    }

    /// Bring the album up to date: drop `removed`, place `changed` by date.
    /// Returns whether anything moved.
    pub(crate) fn apply_album_changes(
        &mut self,
        changed: Vec<IndexItem>,
        removed: &HashSet<String>,
    ) -> bool {
        let mut items = self.album_items().to_vec();
        if !index::apply(&mut items, changed, removed) {
            return false;
        }
        let days = layout::days(&items, &jiff::tz::TimeZone::system());
        self.show_album_items(items, days);
        self.forget_pages();
        true
    }

    /// Ranks moved: full records loaded by rank no longer line up.
    pub(super) fn forget_pages(&mut self) {
        self.page_epoch += 1;
        self.assets.clear();
        self.page_ids.clear();
        self.page_lru.clear();
        self.pages_requested.clear();
        self.page_queue.clear();
        self.pages_in_flight = 0;
    }

    /// Check for changes in the background, if nothing else is going on.
    pub(crate) fn sync_quietly(&mut self, cx: &mut Context<Self>) {
        if self.viewer.is_some()
            || self.scan.is_some()
            || self.syncing.is_some()
            || self.trash.is_some()
            || self.deleting > 0
            || self.error.is_some()
            || !matches!(self.state, LoadState::Ready)
        {
            return;
        }
        if let Some(token) = self.token.clone() {
            self.sync_changes(token, false, cx);
        }
    }

    /// Bring the album up to date: from the change feed when it can,
    /// otherwise by asking how many items it has and rescanning it.
    pub(crate) fn refresh(&mut self, progressive: bool, cx: &mut Context<Self>) {
        if self.scan.is_some() || self.syncing.is_some() {
            return;
        }
        if self.album_len() > 0
            && self.trash.is_none()
            && self.deleting == 0
            && let Some(token) = self.token.clone()
        {
            self.sync_changes(token, true, cx);
            return;
        }
        let generation = self.generation;
        let client = self.client.clone();
        let album = self.album.clone();
        let want_token = self.follows_changes();
        cx.spawn(async move |this, cx| {
            let count = net::run(move || {
                client.check_ready()?;
                // Taken before the scan, so changes made during it are
                // picked up after.
                let token = want_token.then(|| client.sync_token().ok()).flatten();
                Ok::<_, PhotosError>((client.count(&album)?, token))
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match count {
                    Some(Ok((count, token))) => this.start_scan(count, token, progressive, cx),
                    Some(Err(PhotosError::SessionExpired)) => cx.emit(GridEvent::SessionExpired),
                    Some(Err(err)) => this.fail(err.to_string(), cx),
                    None => this.fail("Something went wrong loading the library.".into(), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask the change feed what changed since `token` and apply it.
    pub(super) fn sync_changes(&mut self, token: String, shown: bool, cx: &mut Context<Self>) {
        self.syncing = Some(shown);
        if shown {
            cx.notify();
        }
        let generation = self.generation;
        let client = self.client.clone();
        let album = self.album.clone();
        cx.spawn(async move |this, cx| {
            let result = net::run(move || {
                let changes = client.changes(&token)?;
                Ok::<_, PhotosError>((changes, client.count(&album)?))
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.syncing = None;
                match result {
                    Some(Ok((changes, count))) => this.changes_arrived(changes, count, shown, cx),
                    Some(Err(PhotosError::SessionExpired)) => cx.emit(GridEvent::SessionExpired),
                    // Offline for a moment: keep the token for the next try.
                    Some(Err(PhotosError::Network(err))) => {
                        if shown {
                            this.set_status(
                                Status::Failed(format!("Couldn't check for changes: {err}").into()),
                                cx,
                            );
                        }
                    }
                    None if !shown => {}
                    // The token expired, or something else: rescan.
                    _ => {
                        this.token = None;
                        this.refresh(false, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn changes_arrived(
        &mut self,
        changes: LibraryChanges,
        count: usize,
        shown: bool,
        cx: &mut Context<Self>,
    ) {
        let quiet = changes.assets.is_empty() && changes.removed.is_empty();
        let mut removed: HashSet<String> = changes.removed.into_iter().collect();
        let mut changed = Vec::new();
        for asset in &changes.assets {
            let item = IndexItem::from(asset);
            if self.album_keeps(&item) {
                changed.push(item);
            } else {
                removed.insert(item.id);
            }
        }
        let now = Instant::now();
        self.recent
            .retain(|(at, ..)| now.duration_since(*at) < RECENT_FOR);
        for id in &removed {
            self.recent.push((now, id.clone(), None));
        }
        for item in &changed {
            self.recent.push((now, item.id.clone(), Some(item.clone())));
        }
        self.uploads_listed(&changed, cx);
        let moved = self.apply_album_changes(changed, &removed);
        let token_moved = self.token.as_deref() != Some(changes.token.as_str());
        self.token = Some(changes.token);
        if count as isize - self.album_len() as isize == self.count_gap {
            self.gap_misses = 0;
        } else if quiet {
            // iCloud's count lags the feed for a while after a change, so
            // only a count that stays out of step means something was
            // missed (an edge the feed doesn't show): then rescan.
            self.gap_misses += 1;
            if shown || self.gap_misses >= 2 {
                self.gap_misses = 0;
                self.token = None;
                self.refresh(false, cx);
                return;
            }
        }
        if moved || token_moved {
            self.save_index(cx);
        }
    }

    /// Lay what the feed brought lately over a fresh scan, which may have
    /// been listed from before iCloud caught up with it.
    pub(super) fn reapply_recent(&mut self) {
        let mut latest: HashMap<String, Option<IndexItem>> = HashMap::new();
        for (_, id, item) in &self.recent {
            latest.insert(id.clone(), item.clone());
        }
        let mut removed = HashSet::new();
        let mut changed = Vec::new();
        for (id, item) in latest {
            match item {
                Some(item) if self.album_keeps(&item) => changed.push(item),
                _ => {
                    removed.insert(id);
                }
            }
        }
        if !changed.is_empty() || !removed.is_empty() {
            self.apply_album_changes(changed, &removed);
        }
    }

    pub(super) fn start_scan(
        &mut self,
        count: usize,
        token: Option<String>,
        progressive: bool,
        cx: &mut Context<Self>,
    ) {
        if self.scan.is_some() {
            return;
        }
        if count == 0 {
            self.token = token;
            self.set_index(Vec::new());
            self.state = LoadState::Ready;
            self.save_index(cx);
            cx.notify();
            return;
        }
        // Showing pages as they arrive only works from an empty gallery.
        let progressive = progressive && self.album_len() == 0;
        self.scan = Some(Scan {
            count,
            next_rank: 0,
            in_flight: 0,
            pages: HashMap::new(),
            joined: 0,
            fresh: Vec::new(),
            progressive,
            token,
        });
        self.pump_scan(cx);
        cx.notify();
    }

    pub(super) fn pump_scan(&mut self, cx: &mut Context<Self>) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        while scan.in_flight < MAX_SCAN_REQUESTS && scan.next_rank < scan.count {
            let start = scan.next_rank;
            let limit = SCAN_PAGE.min(scan.count - start);
            scan.next_rank += limit;
            scan.in_flight += 1;

            let client = self.client.clone();
            let album = self.album.clone();
            let generation = self.generation;
            cx.spawn(async move |this, cx| {
                let result = net::run(move || {
                    client
                        .scan(&album, start, limit)
                        .map(|assets| assets.iter().map(IndexItem::from).collect::<Vec<_>>())
                })
                .await;
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match result {
                        Some(Ok(items)) => this.scan_page_done(start, items, cx),
                        Some(Err(PhotosError::SessionExpired)) => {
                            cx.emit(GridEvent::SessionExpired)
                        }
                        Some(Err(err)) => {
                            this.scan = None;
                            this.fail(err.to_string(), cx);
                        }
                        None => {
                            this.scan = None;
                            this.fail("Updating the library failed.".into(), cx);
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    pub(super) fn scan_page_done(
        &mut self,
        start: usize,
        items: Vec<IndexItem>,
        cx: &mut Context<Self>,
    ) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        scan.in_flight -= 1;
        scan.pages.insert(start, items);

        // Join the pages that now continue the run from the start. Each item
        // is moved once, however the pages arrive.
        let mut arrived = Vec::new();
        while let Some(page) = scan.pages.remove(&scan.joined) {
            scan.joined += SCAN_PAGE.min(scan.count - scan.joined);
            arrived.extend(page);
        }
        let done = scan.in_flight == 0 && scan.next_rank >= scan.count;
        let progressive = scan.progressive;

        if progressive {
            if !arrived.is_empty() {
                let tz = jiff::tz::TimeZone::system();
                let days = layout::days(&arrived, &tz);
                self.append_album_items(arrived, days);
                self.state = LoadState::Ready;
            }
        } else {
            scan.fresh.extend(arrived);
        }
        if done {
            if let Some(scan) = self.scan.take() {
                self.token = scan.token;
                if !progressive {
                    self.set_index(scan.fresh);
                }
                self.count_gap = scan.count as isize - self.album_len() as isize;
                self.gap_misses = 0;
                self.forget_pages();
                self.reapply_recent();
                if !progressive {
                    let listed = self.album_items().to_vec();
                    self.uploads_listed(&listed, cx);
                }
            }
            // Items deleted while the scan ran stay out.
            if let Some(trash) = &self.trash {
                let ids = trash.ids();
                self.apply_album_changes(Vec::new(), &ids);
            }
            self.state = LoadState::Ready;
            self.save_index(cx);
        }
        self.pump_scan(cx);
        cx.notify();
    }

    /// Replace the album's items, keeping the selection on the same photos.
    pub(super) fn set_index(&mut self, items: Vec<IndexItem>) {
        let days = layout::days(&items, &jiff::tz::TimeZone::system());
        self.show_album_items(items, days);
    }

    pub(crate) fn save_index(&self, cx: &mut Context<Self>) {
        let path = index::cache_path(&self.account_dir, &self.album);
        let mut items = self.album_items().to_vec();
        // Not yet deleted in iCloud: if fuji closes now, they come back.
        if let Some(trash) = &self.trash {
            index::apply(&mut items, trash.items.clone(), &HashSet::new());
        }
        let token = self.token.clone();
        let gap = self.count_gap;
        cx.background_spawn(async move { index::save(&path, items, token, gap) })
            .detach();
    }
}
