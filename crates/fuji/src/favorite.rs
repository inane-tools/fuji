//! Favoriting from Fuji: the heart in the viewer, the menus and the `.` key.
//!
//! The gallery changes at once; iCloud is told in the background, and if it
//! refuses, the change is put back.

use std::collections::HashSet;

use gpui::Context;

use crate::grid::PhotoGrid;
use crate::net;
use crate::ui::Status;

impl PhotoGrid {
    /// Whether the items at `indices` can be favorited from here. Shared
    /// albums belong to someone else; Recently Deleted holds what's going.
    pub(crate) fn can_favorite(&self) -> bool {
        self.can_delete()
    }

    /// Whether every item at `indices` is a favorite already.
    pub(crate) fn all_favorite(&self, indices: &[usize]) -> bool {
        !indices.is_empty()
            && indices
                .iter()
                .all(|&ix| self.index.get(ix).is_some_and(|item| item.favorite))
    }

    /// Favorite the items at `indices`, or, if they all are already,
    /// unfavorite them.
    pub(crate) fn toggle_favorite(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        if !self.can_favorite() {
            return;
        }
        let favorite = !self.all_favorite(&indices);
        let ids: Vec<String> = indices
            .iter()
            .filter_map(|&ix| self.index.get(ix))
            .filter(|item| item.favorite != favorite)
            .map(|item| item.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.close_menu();
        self.mark_favorite(&ids, favorite);
        cx.notify();

        let client = self.client.clone();
        let generation = self.generation;
        let names = ids.clone();
        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.set_favorite(&names, favorite)).await;
            this.update(cx, |this, cx| {
                let error = match result {
                    Some(Ok(())) => {
                        this.save_index(cx);
                        return;
                    }
                    Some(Err(fuji_icloud::PhotosError::SessionExpired)) => {
                        cx.emit(crate::grid::GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => err.to_string(),
                    None => "Something went wrong.".into(),
                };
                // Put it back, if still on the same album.
                if this.generation == generation {
                    this.mark_favorite(&ids, !favorite);
                }
                let what = if favorite { "favorite" } else { "unfavorite" };
                this.set_status(
                    Status::Failed(format!("Couldn't {what}: {error}").into()),
                    cx,
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Set the heart on these items wherever the gallery keeps them. In
    /// Favorites, unfavorited items leave the gallery (not while the viewer
    /// is open on them: the next sync takes them out).
    fn mark_favorite(&mut self, ids: &[String], favorite: bool) {
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut changed = Vec::new();
        for item in self.album_items() {
            if wanted.contains(item.id.as_str()) && item.favorite != favorite {
                let mut item = item.clone();
                item.favorite = favorite;
                changed.push(item);
            }
        }
        for asset in self.assets_mut() {
            if wanted.contains(asset.id.0.as_str()) {
                asset.favorite = favorite;
            }
        }
        let leaving =
            !favorite && matches!(self.album, fuji_icloud::Album::Favorites) && !self.viewer_open();
        if leaving {
            let removed: HashSet<String> = changed.into_iter().map(|item| item.id).collect();
            self.apply_album_changes(Vec::new(), &removed);
        } else {
            self.replace_items(changed);
        }
    }
}
