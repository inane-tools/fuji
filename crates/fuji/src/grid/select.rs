//! Selecting items, and what the pointer does on the mosaic.

use super::*;

impl PhotoGrid {
    // ---- Selection ------------------------------------------------------------

    /// What an action on tile `index` applies to: the whole selection if the
    /// tile is part of it, otherwise just the tile.
    pub(super) fn targets_for(&self, index: usize) -> Vec<usize> {
        if self.selected.contains(&index) {
            self.selected.iter().copied().collect()
        } else {
            vec![index]
        }
    }

    /// Whether every photo of the day, month or year around `index` is selected.
    pub(super) fn span_selected(&self, index: usize, span: Span) -> bool {
        let range = layout::span(&self.days, index, span);
        !range.is_empty() && self.selected.range(range.clone()).count() == range.len()
    }

    /// Select the whole day, month or year around `index`, or deselect it
    /// if it's all selected already.
    pub(super) fn toggle_span(&mut self, index: usize, span: Span, cx: &mut Context<Self>) {
        let range = layout::span(&self.days, index, span);
        if self.span_selected(index, span) {
            self.selected.retain(|ix| !range.contains(ix));
        } else {
            self.selected.extend(range);
            self.select_anchor = Some(index);
        }
        cx.notify();
    }

    pub(super) fn toggle_selected(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.select_anchor = Some(index);
        cx.notify();
    }

    pub(super) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.select_anchor = None;
        cx.notify();
    }

    pub(super) fn tile_clicked(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if std::mem::take(&mut self.suppress_click) {
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.shift
            && let Some(anchor) = self.select_anchor
        {
            self.selected.extend(anchor.min(index)..=anchor.max(index));
            cx.notify();
        } else if modifiers.control || modifiers.platform || !self.selected.is_empty() {
            self.toggle_selected(index, cx);
        } else {
            self.open_viewer(index, window, cx);
        }
    }

    pub(super) fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_some() {
            return;
        }
        if self.menu.take().is_none()
            && self.pending_upload.take().is_none()
            && self.pending_delete.take().is_none()
        {
            self.selected.clear();
            self.select_anchor = None;
        }
        cx.notify();
    }

    pub(crate) fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Close the viewer without keeping its photo in view: the items are
    /// about to move (a deletion, or one undone).
    pub(crate) fn leave_viewer(&mut self, window: &mut Window) {
        if self.viewer.take().is_some() {
            self.focus_handle.focus(window);
        }
    }

    pub(super) fn favorite_selection(
        &mut self,
        _: &FavoriteSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.toggle_favorite(self.selected.iter().copied().collect(), cx);
        }
    }

    pub(super) fn delete_selection(
        &mut self,
        _: &DeleteSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.ask_delete(self.selected.iter().copied().collect(), cx);
        }
    }

    pub(super) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() {
            self.selected = (0..self.index.len()).collect();
            cx.notify();
        }
    }

    pub(super) fn copy_selection(
        &mut self,
        _: &CopySelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.copy(self.selected.iter().copied().collect(), cx);
        }
    }

    pub(super) fn download_selection(
        &mut self,
        _: &DownloadSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.download(self.selected.iter().copied().collect(), cx);
        }
    }

    pub(super) fn paste_action(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() {
            self.paste(cx);
        }
    }

    // ---- Pointer --------------------------------------------------------------

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let held = event.pressed_button == Some(MouseButton::Left);
        if self.scrub.dragging {
            if held {
                self.scrub_to(event.position, cx);
            } else {
                self.scrub.dragging = false;
                cx.notify();
            }
            return;
        }
        let Some((index, start)) = self.press else {
            return;
        };
        if !held {
            self.press = None;
            return;
        }
        let moved = event.position - start;
        if f32::from(moved.x).hypot(f32::from(moved.y)) > DRAG_THRESHOLD {
            self.press = None;
            self.start_drag(self.targets_for(index), window, cx);
        }
    }

    pub(super) fn end_press(&mut self, cx: &mut Context<Self>) {
        self.press = None;
        if std::mem::take(&mut self.scrub.dragging) {
            cx.notify();
        }
    }

    pub(crate) fn open_viewer_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_menu(MenuKind::Viewer(index), position, cx);
    }

    pub(super) fn open_menu(
        &mut self,
        kind: MenuKind,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.menu_serial += 1;
        self.menu = Some(OpenMenu {
            serial: self.menu_serial,
            position,
            kind,
        });
        cx.notify();
    }
}
