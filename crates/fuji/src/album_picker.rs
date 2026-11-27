//! Adding photos to an album: a popover listing the user's albums (cover,
//! name, count), from the viewer's top bar and the gallery's menus.
//!
//! The gallery draws it, over the viewer too, and owns what it needs: the
//! album list (from the app), covers and counts (loaded the first time it
//! opens) and the album used last this session, which leads the list.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fuji_icloud::UserAlbum;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use gpui::{
    AnimationExt as _, App, Context, Corner, ElementId, Entity, FocusHandle, Focusable as _,
    FontWeight, KeyBinding, ObjectFit, Pixels, Point, ScrollHandle, Subscription, Window, actions,
    anchored, deferred, div, img, prelude::*, px,
};

use crate::assets::icon;
use crate::grid::{GridEvent, PhotoGrid};
use crate::input::{Submitted, TextInput};
use crate::ui::Status;
use crate::{anim, net, theme, thumbs, ui};

actions!(album_picker, [Up, Down, Pick, Dismiss, AddToLastAlbum]);

/// Adds what's selected (or the photo in the viewer) to the album used last.
pub const LAST_ALBUM_KEY: &str = "a";

const CONTEXT: &str = "AlbumPicker";
/// With more albums than this, a field to find one by name.
const FILTER_FROM: usize = 6;
const ROW_HEIGHT: f32 = 48.;
const VISIBLE_ROWS: f32 = 7.5;
const COVER: f32 = 36.;
/// Side of the square cover files made from thumbnails.
const COVER_PIXELS: u32 = 96;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("enter", Pick, Some(CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
        KeyBinding::new(LAST_ALBUM_KEY, AddToLastAlbum, Some("Gallery")),
        KeyBinding::new(LAST_ALBUM_KEY, AddToLastAlbum, Some("Viewer")),
    ]);
}

/// The gallery's album state for adding to albums.
#[derive(Default)]
pub struct Albums {
    list: Vec<UserAlbum>,
    /// Square cover files, by album id (`None`: looked, there's none).
    covers: HashMap<Arc<str>, Option<Arc<Path>>>,
    counts: HashMap<Arc<str>, usize>,
    /// Covers and counts were asked for.
    loading: bool,
    /// The album added to last (this session).
    last: Option<Arc<str>>,
    /// What's known to be in which album: put there or found there this
    /// session. The key takes out again what it just put in.
    known: HashSet<(Arc<str>, String)>,
    open: Option<Picker>,
    serial: u64,
    /// When an outside click last closed it: the click that follows on the
    /// button that opened it shouldn't open it again.
    closed_at: Option<Instant>,
}

struct Picker {
    serial: u64,
    /// Asset ids to add.
    items: Vec<String>,
    position: Point<Pixels>,
    corner: Corner,
    highlighted: usize,
    filter: Option<Entity<TextInput>>,
    focus: FocusHandle,
    /// Focus to give back on closing.
    return_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Albums {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }
}

impl PhotoGrid {
    /// The user's albums, from the app.
    pub fn set_user_albums(&mut self, albums: Vec<UserAlbum>, cx: &mut Context<Self>) {
        if self.albums.list != albums {
            self.albums.list = albums;
            cx.notify();
        }
    }

    /// Albums the items at hand can go to: the user's, but not the one open.
    fn album_choices(&self) -> Vec<UserAlbum> {
        let mut list: Vec<UserAlbum> = self
            .albums
            .list
            .iter()
            .filter(|a| self.album != a.album())
            .cloned()
            .collect();
        // The last one used first.
        if let Some(last) = &self.albums.last
            && let Some(i) = list.iter().position(|a| &a.id == last)
        {
            let album = list.remove(i);
            list.insert(0, album);
        }
        list
    }

    pub(crate) fn can_add_to_album(&self) -> bool {
        self.can_favorite() && !self.albums.list.is_empty()
    }

    /// Open the album list for the items at `indices`, with `corner` of it
    /// at `position` (window coordinates).
    pub(crate) fn open_album_picker(
        &mut self,
        indices: Vec<usize>,
        position: Point<Pixels>,
        corner: Corner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .albums
            .closed_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        let items: Vec<String> = indices
            .iter()
            .filter_map(|&ix| self.index.get(ix))
            .map(|item| item.id.clone())
            .collect();
        if items.is_empty() || !self.can_add_to_album() {
            return;
        }
        self.close_menu();
        self.load_album_covers(cx);
        // Before the picker takes focus.
        let return_focus = window.focused(cx);
        let focus = cx.focus_handle();
        let mut subscriptions = Vec::new();
        let filter = (self.album_choices().len() > FILTER_FROM).then(|| {
            let input = cx.new(|cx| TextInput::new("Find an album", cx).compact("search"));
            subscriptions.push(cx.observe(&input, |this, _, cx| {
                if let Some(picker) = &mut this.albums.open {
                    picker.highlighted = 0;
                    picker.scroll.scroll_to_item(0);
                }
                cx.notify();
            }));
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, _: &Submitted, window, cx| this.pick_highlighted(window, cx),
            ));
            input
        });
        match &filter {
            Some(input) => input.focus_handle(cx).focus(window),
            None => focus.focus(window),
        }
        self.albums.serial += 1;
        self.albums.open = Some(Picker {
            serial: self.albums.serial,
            items,
            position,
            corner,
            highlighted: 0,
            filter,
            focus,
            return_focus,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// The album used last, if it's there and isn't the one open.
    pub(crate) fn last_album_choice(&self) -> Option<UserAlbum> {
        let last = self.albums.last.clone()?;
        self.album_choices().into_iter().find(|a| a.id == last)
    }

    /// Whether all these items are known to be in `album`.
    fn all_in(&self, album: &Arc<str>, items: &[String]) -> bool {
        !items.is_empty()
            && items
                .iter()
                .all(|item| self.albums.known.contains(&(album.clone(), item.clone())))
    }

    fn ids_at(&self, indices: &[usize]) -> Vec<String> {
        indices
            .iter()
            .filter_map(|&ix| self.index.get(ix))
            .map(|item| item.id.clone())
            .collect()
    }

    /// Add to the album used last, or take out again what's in it (as `.`
    /// does with favorites).
    fn toggle_in_last_album(
        &mut self,
        album: UserAlbum,
        items: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if items.is_empty() {
            return;
        }
        if self.all_in(&album.id, &items) {
            self.remove_from_album(album, items, cx);
        } else {
            self.add_to_album(album, items, cx);
        }
    }

    /// The key: add the selection to the album used last (or take it out
    /// again), or, before there is one, pick it.
    pub(crate) fn add_selection_to_last_album(
        &mut self,
        _: &AddToLastAlbum,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer_open() || self.selected.is_empty() {
            return;
        }
        let indices: Vec<usize> = self.selected.iter().copied().collect();
        self.add_to_last_album(indices, None, window, cx);
    }

    /// Add the items at `indices` to the album used last; without one, open
    /// the list at `position` (or near the top of the window).
    pub(crate) fn add_to_last_album(
        &mut self,
        indices: Vec<usize>,
        position: Option<(Point<Pixels>, Corner)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_add_to_album() {
            return;
        }
        match self.last_album_choice() {
            Some(album) => {
                let items = self.ids_at(&indices);
                self.toggle_in_last_album(album, items, cx);
            }
            None => {
                let (position, corner) = position.unwrap_or_else(|| {
                    let width = window.viewport_size().width;
                    (gpui::point(width / 2. - px(150.), px(96.)), Corner::TopLeft)
                });
                self.open_album_picker(indices, position, corner, window, cx);
            }
        }
    }

    /// A menu entry opening the album list where the menu was.
    pub(crate) fn add_to_album_item(
        &self,
        indices: Vec<usize>,
        position: Point<Pixels>,
        label: String,
        cx: &mut Context<Self>,
    ) -> crate::ui::MenuItem {
        let this = cx.weak_entity();
        crate::ui::MenuItem::new("album-add", label, move |window, cx| {
            this.update(cx, |this, cx| {
                this.open_album_picker(indices.clone(), position, Corner::TopLeft, window, cx)
            })
            .ok();
        })
    }

    /// A menu entry adding to the album used last (or taking out again),
    /// if there is one.
    pub(crate) fn add_to_last_album_item(
        &self,
        indices: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> Option<crate::ui::MenuItem> {
        let album = self.last_album_choice()?;
        let verb = if self.all_in(&album.id, &self.ids_at(&indices)) {
            "Remove from"
        } else {
            "Add to"
        };
        let this = cx.weak_entity();
        Some(
            crate::ui::MenuItem::new(
                "album-add",
                format!("{verb} {}", album.name),
                move |_, cx| {
                    this.update(cx, |this, cx| {
                        let items = this.ids_at(&indices);
                        this.toggle_in_last_album(album.clone(), items, cx)
                    })
                    .ok();
                },
            )
            .hint("A"),
        )
    }

    pub(crate) fn close_album_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.albums.open.take() {
            match picker.return_focus {
                Some(focus) => focus.focus(window),
                None => self.focus_handle.focus(window),
            }
            cx.notify();
        }
    }

    /// The albums shown, matching what's typed.
    fn shown_albums(&self, cx: &App) -> Vec<UserAlbum> {
        let needle = self
            .albums
            .open
            .as_ref()
            .and_then(|p| p.filter.as_ref())
            .map(|input| input.read(cx).text().trim().to_lowercase())
            .unwrap_or_default();
        self.album_choices()
            .into_iter()
            .filter(|a| needle.is_empty() || a.name.to_lowercase().contains(&needle))
            .collect()
    }

    fn move_highlight(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.shown_albums(cx).len();
        if let Some(picker) = &mut self.albums.open
            && count > 0
        {
            let next = (picker.highlighted as isize + by).clamp(0, count as isize - 1) as usize;
            picker.highlighted = next;
            picker.scroll.scroll_to_item(next);
            cx.notify();
        }
    }

    fn pick_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(highlighted) = self.albums.open.as_ref().map(|p| p.highlighted) else {
            return;
        };
        if let Some(album) = self.shown_albums(cx).get(highlighted).cloned() {
            self.pick_album(album, window, cx);
        }
    }

    fn pick_album(&mut self, album: UserAlbum, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.albums.open.as_ref() else {
            return;
        };
        let items = picker.items.clone();
        self.close_album_picker(window, cx);
        self.add_to_album(album, items, cx);
    }

    /// Put these items (asset ids) in `album`, saying how it went.
    pub(crate) fn add_to_album(
        &mut self,
        album: UserAlbum,
        items: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.albums.last = Some(album.id.clone());
        let name = album.name.clone();
        self.set_status(
            Status::Working(format!("Adding to {name}\u{2026}").into()),
            cx,
        );
        let client = self.client.clone();
        let id = album.id.clone();
        let total = items.len();
        cx.spawn(async move |this, cx| {
            let sent = items.clone();
            // Its own thread: the shared pool may be busy with covers.
            let result = run_now(move || client.add_to_album(&id, &sent)).await;
            this.update(cx, |this, cx| {
                let status = match result {
                    Some(Ok(added)) => {
                        for item in items {
                            this.albums.known.insert((album.id.clone(), item));
                        }
                        if let Some(count) = this.albums.counts.get_mut(&album.id) {
                            *count += added;
                        }
                        // Showing that album: list what came in.
                        if this.album == album.album() {
                            this.sync_quietly(cx);
                        }
                        Status::Done {
                            message: added_message(&name, added, total).into(),
                            reveal: None,
                        }
                    }
                    Some(Err(fuji_icloud::PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => {
                        Status::Failed(format!("Couldn't add to {name}: {err}").into())
                    }
                    None => Status::Failed(format!("Couldn't add to {name}.").into()),
                };
                this.set_status(status, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Take these items (asset ids) out of `album` again.
    fn remove_from_album(&mut self, album: UserAlbum, items: Vec<String>, cx: &mut Context<Self>) {
        let name = album.name.clone();
        self.set_status(
            Status::Working(format!("Removing from {name}\u{2026}").into()),
            cx,
        );
        let client = self.client.clone();
        let id = album.id.clone();
        cx.spawn(async move |this, cx| {
            let sent = items.clone();
            let result = run_now(move || client.remove_from_album(&id, &sent)).await;
            this.update(cx, |this, cx| {
                let status = match result {
                    Some(Ok(removed)) => {
                        for item in items {
                            this.albums.known.remove(&(album.id.clone(), item));
                        }
                        if let Some(count) = this.albums.counts.get_mut(&album.id) {
                            *count = count.saturating_sub(removed);
                        }
                        if this.album == album.album() {
                            this.sync_quietly(cx);
                        }
                        Status::Done {
                            message: removed_message(&name, removed).into(),
                            reveal: None,
                        }
                    }
                    Some(Err(fuji_icloud::PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => {
                        Status::Failed(format!("Couldn't remove from {name}: {err}").into())
                    }
                    None => Status::Failed(format!("Couldn't remove from {name}.").into()),
                };
                this.set_status(status, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Counts in one request, then each album's first photo as its cover.
    fn load_album_covers(&mut self, cx: &mut Context<Self>) {
        if self.albums.loading {
            return;
        }
        self.albums.loading = true;
        let albums = self.album_choices();
        let client = self.client.clone();
        let dir = self.thumb_dir.clone();
        cx.spawn(async move |this, cx| {
            let ids: Vec<String> = albums.iter().filter_map(|a| a.album().count_id()).collect();
            let counts_client = client.clone();
            if let Some(Ok(found)) = net::run(move || counts_client.index_counts(&ids)).await {
                this.update(cx, |this, cx| {
                    for album in &albums {
                        let id = album.album().count_id();
                        if let Some((_, n)) = found.iter().find(|(i, _)| Some(i) == id.as_ref()) {
                            this.albums.counts.insert(album.id.clone(), *n);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
            let mut pending = FuturesUnordered::new();
            for album in albums {
                let (client, dir) = (client.clone(), dir.clone());
                pending.push(async move {
                    let id = album.id.clone();
                    let cover = net::run(move || {
                        let first = client
                            .fetch(&album.album(), 0, 1)
                            .ok()?
                            .into_iter()
                            .next()?;
                        let thumb = first.thumbnail.as_ref()?;
                        let path = thumbs::ensure(&client, &dir, &first.id, thumb).ok()?;
                        square_cover(&path)
                    })
                    .await
                    .flatten();
                    (id, cover)
                });
            }
            while let Some((id, cover)) = pending.next().await {
                let alive = this
                    .update(cx, |this, cx| {
                        this.albums.covers.insert(id, cover.map(Into::into));
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn render_album_picker(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let picker = self.albums.open.as_ref()?;
        let albums = self.shown_albums(cx);
        let count = picker.items.len();
        let subtitle = if count == 1 {
            "1 item".to_string()
        } else {
            format!("{} items", crate::collections::group_digits(count))
        };
        let last = self.albums.last.clone();
        let highlighted = picker.highlighted;
        let this = cx.weak_entity();

        let overflowing = albums.len() as f32 > VISIBLE_ROWS;
        let rows = albums.iter().enumerate().map(|(i, album)| {
            let cover = self.albums.covers.get(&album.id);
            let count = self.albums.counts.get(&album.id).copied();
            let is_last = last.as_ref() == Some(&album.id);
            let this = this.clone();
            let pick = album.clone();
            let cover = div()
                .size(px(COVER))
                .flex_none()
                .rounded(px(8.))
                .overflow_hidden()
                .bg(theme::placeholder())
                .flex()
                .items_center()
                .justify_center()
                .map(|el| match cover {
                    Some(Some(path)) => el.child(
                        img(path.clone())
                            .size_full()
                            .rounded(px(8.))
                            .object_fit(ObjectFit::Cover),
                    ),
                    Some(None) => el.child(icon("albums", px(16.), theme::text_meta())),
                    None => el,
                });
            div()
                .id(ElementId::NamedInteger("album-choice".into(), i as u64))
                .h(px(ROW_HEIGHT))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(px(10.))
                .cursor_pointer()
                .when(i == highlighted, |el| el.bg(theme::hover()))
                .hover(|el| el.bg(theme::hover()))
                .on_mouse_move({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(picker) = &mut this.albums.open
                                && picker.highlighted != i
                            {
                                picker.highlighted = i;
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                })
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.pick_album(pick.clone(), window, cx))
                        .ok();
                })
                .child(cover)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text())
                                .child(album.name.clone()),
                        )
                        .child(
                            div()
                                .h(px(16.))
                                .text_size(px(12.))
                                .text_color(theme::text_meta())
                                .children(count.map(crate::collections::group_digits)),
                        ),
                )
                .when(is_last, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(theme::text_meta())
                            .child("Last used"),
                    )
                })
        });
        let list = div()
            .relative()
            .child(
                div()
                    .id("album-choices")
                    .max_h(px(ROW_HEIGHT * VISIBLE_ROWS))
                    .overflow_y_scroll()
                    .track_scroll(&picker.scroll)
                    .flex()
                    .flex_col()
                    .children(rows)
                    // Clear of the scrollbar.
                    .when(overflowing, |el| el.pr(px(10.))),
            )
            .when(overflowing, |el| {
                el.child(ui::scrollbar(
                    "album-choices-scrollbar",
                    picker.scroll.clone(),
                ))
            })
            .when(albums.is_empty(), |el| {
                el.child(
                    div()
                        .px(px(8.))
                        .py(px(14.))
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("No album by that name."),
                )
            });

        let header = div()
            .px(px(8.))
            .pt(px(6.))
            .pb(px(4.))
            .flex()
            .items_baseline()
            .justify_between()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text())
                    .child("Add to Album"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(subtitle),
            );

        let up = this.clone();
        let down = this.clone();
        let pick = this.clone();
        let dismiss = this.clone();
        let outside = this.clone();
        let panel = div()
            .id("album-picker")
            .key_context(CONTEXT)
            .track_focus(&picker.focus)
            .occlude()
            .w(px(300.))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .rounded(px(16.))
            .bg(theme::popover())
            .shadow_lg()
            .on_action(move |_: &Up, _, cx| {
                up.update(cx, |this, cx| this.move_highlight(-1, cx)).ok();
            })
            .on_action(move |_: &Down, _, cx| {
                down.update(cx, |this, cx| this.move_highlight(1, cx)).ok();
            })
            .on_action(move |_: &Pick, window, cx| {
                pick.update(cx, |this, cx| this.pick_highlighted(window, cx))
                    .ok();
            })
            .on_action(move |_: &Dismiss, window, cx| {
                dismiss
                    .update(cx, |this, cx| this.close_album_picker(window, cx))
                    .ok();
            })
            .on_mouse_down_out(move |_, window, cx| {
                outside
                    .update(cx, |this, cx| {
                        this.albums.closed_at = Some(Instant::now());
                        this.close_album_picker(window, cx);
                    })
                    .ok();
            })
            .child(header)
            .children(
                picker
                    .filter
                    .clone()
                    .map(|input| div().px(px(2.)).child(input)),
            )
            .child(list)
            .with_animation(
                ElementId::NamedInteger("album-picker-open".into(), picker.serial),
                anim::ease(anim::FAST),
                |el, t| el.opacity(t),
            );
        Some(
            deferred(
                anchored()
                    .position(picker.position)
                    .anchor(picker.corner)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(2),
        )
    }
}

/// Run a blocking call on a thread of its own, now, rather than queued
/// behind the network pool's work.
async fn run_now<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("fuji-album-add".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.await.ok()
}

/// "Removed from Trip", "Removed 3 items from Trip".
fn removed_message(album: &str, removed: usize) -> String {
    match removed {
        0 => format!("Not in {album}"),
        1 => format!("Removed from {album}"),
        n => format!("Removed {n} items from {album}"),
    }
}

/// "Added to Trip", "Added 3 items to Trip", "Already in Trip".
fn added_message(album: &str, added: usize, total: usize) -> String {
    match (added, total) {
        (0, 1) => format!("Already in {album}"),
        (0, _) => format!("All already in {album}"),
        (_, 1) => format!("Added to {album}"),
        (a, t) if a == t => format!("Added {a} items to {album}"),
        (a, t) if t - a == 1 => format!("Added {a} items to {album} (1 was in it already)"),
        (a, t) => format!("Added {a} items to {album} ({} were in it already)", t - a),
    }
}

/// A small square crop of a thumbnail, beside it, for a cover whose
/// corners round (GPUI rounds an image's fitted bounds, not what shows).
fn square_cover(thumb: &Path) -> Option<PathBuf> {
    let out = thumb.with_extension(format!("square{COVER_PIXELS}.jpg"));
    if out.exists() {
        return Some(out);
    }
    let image = image::open(thumb).ok()?;
    let side = image.width().min(image.height());
    let square = image
        .crop_imm(
            (image.width() - side) / 2,
            (image.height() - side) / 2,
            side,
            side,
        )
        .thumbnail(COVER_PIXELS, COVER_PIXELS)
        .to_rgb8();
    let tmp = out.with_extension(format!("tmp{}", std::process::id()));
    square
        .save_with_format(&tmp, image::ImageFormat::Jpeg)
        .ok()?;
    std::fs::rename(&tmp, &out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_was_added() {
        assert_eq!(added_message("Trip", 1, 1), "Added to Trip");
        assert_eq!(added_message("Trip", 0, 1), "Already in Trip");
        assert_eq!(added_message("Trip", 3, 3), "Added 3 items to Trip");
        assert_eq!(
            added_message("Trip", 2, 3),
            "Added 2 items to Trip (1 was in it already)"
        );
    }
}
