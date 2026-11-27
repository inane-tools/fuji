//! Moving photos in and out of the gallery: copy to the clipboard, download
//! originals, drag out as files; upload by picking, dropping or pasting
//! files; and delete, with a moment to undo.
//!
//! Copies and drags hand over the full-size JPEG rendition (the original
//! itself when that is a JPEG or PNG), which every app can open. Videos, and
//! "Download", use the original file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fuji_icloud::{Album, Asset, AssetKind, PhotosError};
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use gpui::{
    AppContext as _, ClipboardEntry, ClipboardItem, Context, Image, ImageFormat, IntoElement,
    PathPromptOptions, SharedString, Task, Window,
};

use crate::grid::PhotoGrid;
use crate::index::IndexItem;
use crate::platform::{self, ClipboardContent};
use crate::ui::{self, Status};
use crate::{export, net, thumbs};

/// How long to wait for a photo's record (its download URLs) to load.
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);
/// Larger files aren't offered for upload.
const MAX_UPLOAD_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// iCloud takes a moment to list an upload: look again after each of
/// these (the regular check, every 30 s, goes on after).
const CHECKS_AFTER_UPLOAD: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];
/// How long to watch for uploads to be listed, to point them out.
const AWAIT_UPLOADS_FOR: Duration = Duration::from_secs(5 * 60);
/// Uploads sent at once.
const PARALLEL_UPLOADS: usize = 3;
/// The most files one drop or pick takes (a whole drive dropped by mistake).
const MAX_UPLOAD_FILES: usize = 10_000;
/// How deep into dropped folders to look.
const MAX_FOLDER_DEPTH: usize = 12;
/// How long a deletion can be taken back before it's sent to iCloud.
pub const UNDO_WINDOW: Duration = Duration::from_secs(6);

/// Uploads iCloud hasn't listed yet, by file name (several files may
/// share a name). When they're listed, a message offers to show them.
pub struct Awaiting {
    names: Vec<String>,
    listed: Vec<String>,
    since: std::time::Instant,
}

/// Whether `paths` hold something to upload: a photo or video file, or a
/// folder. Fuji's own exports (photos dragged out of it) don't count.
pub fn can_upload(paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .take(100)
        .any(|path| !export::is_export(path) && (platform::is_uploadable(path) || path.is_dir()))
}

/// Items taken out of the gallery, to be deleted once the moment to undo
/// has passed.
pub struct Trash {
    serial: u64,
    pub items: Vec<IndexItem>,
}

impl Trash {
    pub fn ids(&self) -> HashSet<String> {
        self.items.iter().map(|item| item.id.clone()).collect()
    }
}

static TRASH_SERIAL: AtomicU64 = AtomicU64::new(0);

/// The most photos one copy or drag takes: each is fetched in full first.
const MAX_HANDED_OUT: usize = 100;

/// Files waiting for the user to confirm an upload.
pub struct PendingUpload {
    pub items: Vec<UploadItem>,
}

pub struct UploadItem {
    pub name: String,
    source: UploadSource,
}

enum UploadSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        one.to_string()
    } else {
        format!("{count} {many}")
    }
}

fn message(err: PhotosError) -> SharedString {
    match err {
        PhotosError::SessionExpired => "Your session expired. Sign in again.".into(),
        err => err.to_string().into(),
    }
}

impl PhotoGrid {
    /// The full records of `indices`, loading their pages if needed.
    fn records(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) -> Task<Option<Vec<Asset>>> {
        cx.spawn(async move |this, cx| {
            let mut assets = Vec::with_capacity(indices.len());
            for index in indices {
                let mut waited = Duration::ZERO;
                let asset = loop {
                    let found = this
                        .update(cx, |this, cx| {
                            let asset = this.asset(index).cloned();
                            if asset.is_none() {
                                this.ensure_loaded(index, cx);
                            }
                            asset
                        })
                        .ok()?;
                    if let Some(asset) = found {
                        break asset;
                    }
                    if waited >= RECORD_TIMEOUT {
                        return None;
                    }
                    let step = Duration::from_millis(80);
                    cx.background_executor().timer(step).await;
                    waited += step;
                };
                assets.push(asset);
            }
            Some(assets)
        })
    }

    /// Make a file for each of `indices` that other apps can take.
    fn export_files(
        &mut self,
        indices: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<PathBuf>, SharedString>> {
        let records = self.records(indices, cx);
        let client = self.client.clone();
        let preview_dir = self.preview_dir.clone();
        cx.background_spawn(async move {
            let assets = records
                .await
                .ok_or_else(|| SharedString::from("Couldn't load the photo's details."))?;
            let mut files = Vec::with_capacity(assets.len());
            for asset in assets {
                let client = client.clone();
                let preview_dir = preview_dir.clone();
                let file = net::run(move || export_one(&client, &preview_dir, &asset))
                    .await
                    .ok_or_else(|| SharedString::from("Couldn't prepare the file."))??;
                files.push(file);
            }
            Ok(files)
        })
    }

    /// Copy the photos to the clipboard, as files and (for one photo) as an
    /// image too.
    pub fn copy(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        if indices.is_empty() || self.is_busy() {
            return;
        }
        let count = indices.len();
        if count > MAX_HANDED_OUT {
            let text = format!("Copy up to {MAX_HANDED_OUT} at a time. Download takes any number.");
            self.set_status(Status::Failed(text.into()), cx);
            return;
        }
        self.set_status(Status::Working("Copying\u{2026}".into()), cx);
        let files = self.export_files(indices, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let files = files.await?;
                    // One picture also goes on as image data, for editors.
                    let jpeg = match files.as_slice() {
                        [file] => std::fs::read(file).ok().filter(|b| export::is_jpeg(b)),
                        _ => None,
                    };
                    let png = jpeg.as_deref().and_then(export::png_from_jpeg);
                    let content = ClipboardContent { files, jpeg, png };
                    let copied = platform::copy(&content);
                    Ok::<_, SharedString>((content, copied))
                })
                .await;
            this.update(cx, |this, cx| {
                let done = Status::Done {
                    message: format!("Copied {}", plural(count, "photo", "items")).into(),
                    reveal: None,
                };
                let status = match result {
                    Ok((_, true)) => done,
                    // No file clipboard here: GPUI can still take one image.
                    Ok((content, false)) => match content.jpeg {
                        Some(jpeg) => {
                            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                                ImageFormat::Jpeg,
                                jpeg,
                            )));
                            done
                        }
                        None => Status::Failed("Couldn't copy to the clipboard.".into()),
                    },
                    Err(error) => Status::Failed(error),
                };
                this.set_status(status, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Save the original files: one asks for a file name, several for a folder.
    pub fn download(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        if indices.is_empty() || self.is_busy() {
            return;
        }
        let records = self.records(indices, cx);
        let client = self.client.clone();
        let folder = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);

        cx.spawn(async move |this, cx| {
            let fail = |text: &'static str, cx: &mut gpui::AsyncApp| {
                this.update(cx, |this, cx| {
                    this.set_status(Status::Failed(text.into()), cx)
                })
                .ok();
            };
            let Some(assets) = records.await else {
                return fail("Couldn't load the photo's details.", cx);
            };
            let originals: Vec<_> = assets
                .iter()
                .filter_map(|asset| {
                    let original = asset.original.clone()?;
                    Some((export::original_name(asset, &original), original))
                })
                .collect();
            if originals.is_empty() {
                return fail("The original isn't available for this item.", cx);
            }

            // Where to: a file for one, a folder for several.
            let single = originals.len() == 1;
            let (target_dir, reveal) = if single {
                let name = originals[0].0.clone();
                let Ok(chosen) = cx.update(|cx| cx.prompt_for_new_path(&folder, Some(&name)))
                else {
                    return;
                };
                let Ok(Ok(Some(path))) = chosen.await else {
                    return;
                };
                (None, path)
            } else {
                let chosen = cx.update(|cx| {
                    cx.prompt_for_paths(PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some("Download here".into()),
                    })
                });
                let Ok(chosen) = chosen else { return };
                let Ok(Ok(Some(mut paths))) = chosen.await else {
                    return;
                };
                let Some(dir) = paths.pop() else { return };
                (Some(dir.clone()), dir)
            };

            let total = originals.len();
            let mut failed = None;
            for (done, (name, original)) in originals.into_iter().enumerate() {
                let text = if single {
                    "Downloading original\u{2026}".to_string()
                } else {
                    format!("Downloading {} of {total}\u{2026}", done + 1)
                };
                this.update(cx, |this, cx| {
                    this.set_status(Status::Working(text.into()), cx)
                })
                .ok();
                let target = match &target_dir {
                    Some(dir) => free_name(dir, &name),
                    None => reveal.clone(),
                };
                let client = client.clone();
                match net::run(move || client.download_to(&original, &target)).await {
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        failed = Some(message(err));
                        break;
                    }
                    None => {
                        failed = Some("Download failed.".into());
                        break;
                    }
                }
            }
            let status = match failed {
                Some(error) => Status::Failed(error),
                None => Status::Done {
                    message: if single {
                        format!(
                            "Saved {}",
                            reveal.file_name().unwrap_or_default().to_string_lossy()
                        )
                    } else {
                        format!("Saved {total} items")
                    }
                    .into(),
                    reveal: Some(reveal),
                },
            };
            this.update(cx, |this, cx| this.set_status(status, cx)).ok();
        })
        .detach();
    }

    /// Drag the photos out of the window as files. The pointer's button is
    /// down when this is called; the drag starts once the files are ready,
    /// if it's still down.
    pub fn start_drag(&mut self, indices: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if !platform::CAN_DRAG_OUT || self.dragging_out || indices.is_empty() {
            return;
        }
        if indices.len() > MAX_HANDED_OUT {
            let text = format!("Drag up to {MAX_HANDED_OUT} at a time. Download takes any number.");
            self.set_status(Status::Failed(text.into()), cx);
            return;
        }
        let Some(handle) = platform::native_handle(window) else {
            return;
        };
        self.dragging_out = true;
        // Releasing the button while the files are fetched isn't a click.
        self.suppress_click = true;
        let files = self.export_files(indices, cx);
        cx.spawn(async move |this, cx| {
            let files = files.await;
            if let Ok(files) = &files
                && platform::left_button_down()
            {
                // Blocks here until the drop, running the window's messages;
                // no app state is borrowed at this point.
                platform::drag_files(handle, files);
            }
            this.update(cx, |this, cx| {
                this.dragging_out = false;
                this.suppress_click = false;
                if let Err(error) = files {
                    this.set_status(Status::Failed(error), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Offer to upload what's on the clipboard: image files, or an image.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if self.pending_upload.is_some() || self.is_busy() {
            return;
        }
        let image = cx.read_from_clipboard().and_then(|item| {
            item.into_entries().find_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image),
                ClipboardEntry::String(_) => None,
            })
        });
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_spawn(async { platform::clipboard_files() })
                .await;
            let own = !files.is_empty() && files.iter().all(|f| export::is_export(f));
            let mut items: Vec<UploadItem> = files
                .into_iter()
                .filter(|file| platform::is_uploadable(file) && !export::is_export(file))
                .filter_map(|file| {
                    Some(UploadItem {
                        name: file.file_name()?.to_str()?.to_string(),
                        source: UploadSource::File(file),
                    })
                })
                .collect();
            if items.is_empty()
                && !own
                && let Some(image) = image
                && let Some(extension) = image_extension(image.format)
            {
                let now = jiff::Zoned::now();
                items.push(UploadItem {
                    name: format!("Pasted {}.{extension}", now.strftime("%Y-%m-%d %H.%M.%S")),
                    source: UploadSource::Bytes(image.bytes().to_vec()),
                });
            }
            this.update(cx, |this, cx| {
                if own {
                    let text = "That's already in your library.";
                    this.set_status(Status::Failed(text.into()), cx);
                } else if items.is_empty() {
                    let text = "There's no photo or video on the clipboard.";
                    this.set_status(Status::Failed(text.into()), cx);
                } else {
                    this.pending_upload = Some(PendingUpload { items });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Upload what [`Self::paste`] or a drop offered.
    pub fn confirm_upload(&mut self, cx: &mut Context<Self>) {
        if let Some(upload) = self.pending_upload.take() {
            self.upload(upload.items, cx);
        }
    }

    /// Choose files to upload (the top bar's upload button). Picking them
    /// is the confirmation: they go straight up.
    pub fn pick_uploads(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Upload".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let items = cx
                .background_spawn(async move { upload_items(paths) })
                .await;
            this.update(cx, |this, cx| match items {
                Ok(items) if !items.is_empty() => this.upload(items, cx),
                Ok(_) => this.set_status(
                    Status::Failed("Those aren't photos or videos iCloud takes.".into()),
                    cx,
                ),
                Err(error) => this.set_status(Status::Failed(error), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Files or folders dropped on the gallery: offer to upload what's in them.
    pub(crate) fn dropped(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.pending_upload.is_some() || self.is_busy() {
            return;
        }
        // Photos dragged out of fuji and back in are already in the library.
        if !paths.is_empty() && paths.iter().all(|p| export::is_export(p)) {
            let text = "That's already in your library.";
            self.set_status(Status::Failed(text.into()), cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let items = cx
                .background_spawn(async move { upload_items(paths) })
                .await;
            this.update(cx, |this, cx| match items {
                Ok(items) if !items.is_empty() => {
                    this.pending_upload = Some(PendingUpload { items });
                    cx.notify();
                }
                Ok(_) => this.set_status(
                    Status::Failed("There are no photos or videos in that.".into()),
                    cx,
                ),
                Err(error) => this.set_status(Status::Failed(error), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Send `items` to iCloud, a few at a time, then look for them.
    fn upload(&mut self, items: Vec<UploadItem>, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let total = items.len();
        cx.spawn(async move |this, cx| {
            let progress = |done: usize| -> SharedString {
                if total == 1 {
                    "Adding to iCloud Photos\u{2026}".into()
                } else {
                    format!(
                        "Adding {} of {total} to iCloud Photos\u{2026}",
                        (done + 1).min(total)
                    )
                    .into()
                }
            };
            this.update(cx, |this, cx| {
                this.set_status(Status::Working(progress(0)), cx)
            })
            .ok();
            let mut queue = items.into_iter();
            let mut running = FuturesUnordered::new();
            let mut done = 0;
            let mut failures: Vec<SharedString> = Vec::new();
            let mut sent: Vec<String> = Vec::new();
            loop {
                while running.len() < PARALLEL_UPLOADS {
                    let Some(item) = queue.next() else {
                        break;
                    };
                    let client = client.clone();
                    running.push(net::run(move || {
                        match item.source {
                            UploadSource::Bytes(bytes) => client.upload(
                                &item.name,
                                &bytes,
                                jiff::Timestamp::now().as_millisecond(),
                            ),
                            UploadSource::File(path) => client.upload_file(&item.name, &path),
                        }
                        .map(|()| item.name)
                        .map_err(message)
                    }));
                }
                let Some(result) = running.next().await else {
                    break;
                };
                done += 1;
                match result {
                    Some(Ok(name)) => sent.push(name),
                    Some(Err(error)) => failures.push(error),
                    None => failures.push("Upload failed.".into()),
                }
                if done < total {
                    this.update(cx, |this, cx| {
                        this.set_status(Status::Working(progress(done)), cx)
                    })
                    .ok();
                }
            }
            let added = total - failures.len();
            let status = match failures.first() {
                None => Status::Done {
                    message: format!(
                        "Added {} to iCloud Photos",
                        plural(total, "1 item", "items")
                    )
                    .into(),
                    reveal: None,
                },
                Some(error) if total == 1 => Status::Failed(error.clone()),
                Some(error) => Status::Failed(format!("Added {added} of {total}. {error}").into()),
            };
            this.update(cx, |this, cx| {
                this.set_status(status, cx);
                if !sent.is_empty() {
                    this.awaiting = Some(Awaiting {
                        names: sent,
                        listed: Vec::new(),
                        since: std::time::Instant::now(),
                    });
                }
            })
            .ok();
            if added == 0 {
                return;
            }
            // Show the new arrivals once iCloud has listed them.
            for wait in CHECKS_AFTER_UPLOAD {
                cx.background_executor().timer(wait).await;
                if this.update(cx, |this, cx| this.sync_quietly(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// `items` were just listed by iCloud: if they're uploads being waited
    /// for, and all of them are now in, offer to show them.
    pub(crate) fn uploads_listed(&mut self, items: &[IndexItem], cx: &mut Context<Self>) {
        let Some(awaiting) = self.awaiting.as_mut() else {
            return;
        };
        if awaiting.since.elapsed() > AWAIT_UPLOADS_FOR {
            self.awaiting = None;
            return;
        }
        for item in items {
            let Some(name) = item.name.as_deref() else {
                continue;
            };
            if let Some(at) = awaiting.names.iter().position(|n| n == name)
                && !awaiting.listed.contains(&item.id)
            {
                awaiting.names.swap_remove(at);
                awaiting.listed.push(item.id.clone());
            }
        }
        if !awaiting.names.is_empty() {
            return;
        }
        let Some(awaiting) = self.awaiting.take() else {
            return;
        };
        let ids = awaiting.listed;
        let count = ids.len();
        let this = cx.weak_entity();
        let show = Rc::new(move |_: &mut Window, cx: &mut gpui::App| {
            this.update(cx, |this, cx| this.show_items(&ids, cx)).ok();
        });
        self.set_status(
            Status::Listed {
                message: format!(
                    "{} in your library",
                    plural(count, "The new item is", "new items are")
                )
                .into(),
                show,
            },
            cx,
        );
    }

    // ---- Deleting ---------------------------------------------------------

    /// Whether this album's items can be deleted from here. Shared albums
    /// belong to someone else; Recently Deleted is where deletions go.
    pub(crate) fn can_delete(&self) -> bool {
        !matches!(self.album, Album::Shared(_) | Album::RecentlyDeleted)
    }

    /// Ask before deleting `indices`.
    pub(crate) fn ask_delete(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        if !self.can_delete() {
            return;
        }
        let ids: Vec<String> = indices
            .iter()
            .filter_map(|&ix| self.index.get(ix).map(|item| item.id.clone()))
            .collect();
        if ids.is_empty() {
            return;
        }
        self.close_menu();
        self.pending_delete = Some(ids);
        cx.notify();
    }

    pub(crate) fn render_delete_dialog(
        &self,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let title = if count == 1 {
            "Delete this item?".to_string()
        } else {
            format!("Delete {} items?", crate::grid::group_digits(count))
        };
        let from = match self.album {
            Album::User(_) | Album::Memory(_) => {
                " They're deleted from your library, not just from this album."
            }
            _ => "",
        };
        let body = format!(
            "{} moved to Recently Deleted on all your devices, and removed for good after 30 days.{from}",
            if count == 1 { "It's" } else { "They're" }
        );
        let confirm = cx.weak_entity();
        let cancel = cx.weak_entity();
        ui::destructive_dialog(
            title,
            body,
            "Delete",
            move |window, cx| {
                confirm
                    .update(cx, |this, cx| this.confirm_delete(window, cx))
                    .ok();
            },
            move |_, cx| {
                cancel
                    .update(cx, |this, cx| {
                        this.pending_delete = None;
                        cx.notify();
                    })
                    .ok();
            },
        )
    }

    /// Take the items out of the gallery now; delete them in iCloud once
    /// the moment to undo has passed.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ids) = self.pending_delete.take() else {
            return;
        };
        self.leave_viewer(window);
        // One deletion waits at a time: an earlier one goes now.
        self.commit_trash(cx);
        let ids: HashSet<String> = ids.into_iter().collect();
        let items: Vec<IndexItem> = self
            .album_items()
            .iter()
            .filter(|item| ids.contains(&item.id))
            .cloned()
            .collect();
        if items.is_empty() {
            cx.notify();
            return;
        }
        self.selected.clear();
        self.select_anchor = None;
        self.apply_album_changes(Vec::new(), &ids);
        let count = items.len();
        let serial = TRASH_SERIAL.fetch_add(1, Ordering::Relaxed) + 1;
        self.trash = Some(Trash { serial, items });

        let this = cx.weak_entity();
        let undo = Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
            this.update(cx, |this, cx| this.undo_trash(serial, window, cx))
                .ok();
        });
        self.set_status(
            Status::Undoable {
                message: format!(
                    "Moved {} to Recently Deleted",
                    plural(count, "1 item", "items")
                )
                .into(),
                undo,
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UNDO_WINDOW).await;
            this.update(cx, |this, cx| {
                if this.trash.as_ref().is_some_and(|t| t.serial == serial) {
                    this.commit_trash(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Put back what [`Self::confirm_delete`] took out, if it's still waiting.
    fn undo_trash(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.trash.as_ref().is_none_or(|t| t.serial != serial) {
            return;
        }
        self.leave_viewer(window);
        let Some(trash) = self.trash.take() else {
            return;
        };
        let count = trash.items.len();
        self.apply_album_changes(trash.items, &HashSet::new());
        self.set_status(
            Status::Done {
                message: format!("Restored {}", plural(count, "1 item", "items")).into(),
                reveal: None,
            },
            cx,
        );
        cx.notify();
    }

    /// Delete in iCloud what's waiting in the trash, if anything.
    pub(crate) fn commit_trash(&mut self, cx: &mut Context<Self>) {
        let Some(trash) = self.trash.take() else {
            return;
        };
        // The toast's Undo can't take it back now.
        if matches!(self.toast, Some((_, Status::Undoable { .. }))) {
            self.toast = None;
        }
        self.deleting += 1;
        let client = self.client.clone();
        let generation = self.generation;
        let names: Vec<String> = trash.items.iter().map(|item| item.id.clone()).collect();
        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.set_deleted(&names, true)).await;
            this.update(cx, |this, cx| {
                this.deleting -= 1;
                let error = match result {
                    Some(Ok(())) => return,
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(crate::grid::GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => message(err),
                    None => "Something went wrong.".into(),
                };
                // Still on this album: show them again.
                if this.generation == generation {
                    this.apply_album_changes(trash.items, &HashSet::new());
                }
                this.set_status(
                    Status::Failed(format!("Couldn't delete: {error}").into()),
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }
}

/// What's uploadable in `paths`: files as they are, folders searched
/// through. Sorted by path, so a folder goes up in order.
fn upload_items(paths: Vec<PathBuf>) -> Result<Vec<UploadItem>, SharedString> {
    let mut files = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = paths.into_iter().map(|p| (p, 0)).collect();
    while let Some((path, depth)) = stack.pop() {
        if files.len() > MAX_UPLOAD_FILES {
            return Err(format!(
                "That's more than {} files. Add fewer at a time.",
                crate::grid::group_digits(MAX_UPLOAD_FILES)
            )
            .into());
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if depth >= MAX_FOLDER_DEPTH {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                // Skip hidden and system folders and files (".thumbnails").
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            }
        } else if meta.is_file() && platform::is_uploadable(&path) && !export::is_export(&path) {
            if meta.len() > MAX_UPLOAD_BYTES {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                return Err(format!("{name} is too large to upload.").into());
            }
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files
        .into_iter()
        .filter_map(|file| {
            Some(UploadItem {
                name: file.file_name()?.to_str()?.to_string(),
                source: UploadSource::File(file),
            })
        })
        .collect())
}

/// Fetch (or find in the cache) the file to hand out for `asset`.
fn export_one(
    client: &fuji_icloud::PhotosClient,
    preview_dir: &Path,
    asset: &Asset,
) -> Result<PathBuf, SharedString> {
    let io = |err: std::io::Error| SharedString::from(format!("Couldn't prepare the file: {err}"));
    // Videos go out whole; a still of one would be no use.
    if matches!(asset.kind, AssetKind::Video { .. }) {
        let original = asset
            .original
            .as_ref()
            .ok_or_else(|| SharedString::from("The video isn't available."))?;
        let target = export::original_path(asset, original).map_err(io)?;
        if !target.exists() {
            client.download_to(original, &target).map_err(message)?;
        }
        return Ok(target);
    }
    let rendition = asset
        .preview
        .as_ref()
        .or(asset.thumbnail.as_ref())
        .ok_or_else(|| SharedString::from("The photo isn't available."))?;
    let cached = thumbs::ensure(client, preview_dir, &asset.id, rendition).map_err(message)?;
    export::place(
        &cached,
        &asset.id,
        &export::rendition_name(asset, rendition),
    )
    .map_err(io)
}

fn image_extension(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Gif => "gif",
        ImageFormat::Webp => "webp",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        _ => return None,
    })
}

/// `dir/name`, or `dir/name (2)` and so on if that's taken.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path.extension().map(|e| e.to_string_lossy());
    (2..)
        .map(|n| match &extension {
            Some(extension) => dir.join(format!("{stem} ({n}).{extension}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_photos_videos_and_folders_are_taken() {
        let photo = PathBuf::from("C:/Pictures/beach.HEIC");
        let text = PathBuf::from("C:/Pictures/notes.txt");
        let own = export::dir().join("abc").join("IMG_0001.JPG");
        assert!(can_upload(std::slice::from_ref(&photo)));
        assert!(can_upload(&[text.clone(), photo]));
        assert!(!can_upload(&[text]));
        // Photos dragged out of Fuji, passing back over its window.
        assert!(!can_upload(&[own]));
        assert!(can_upload(&[std::env::temp_dir()]));
        assert!(!can_upload(&[]));
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("fuji-names-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "a.jpg"), dir.join("a.jpg"));
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a.jpg"), dir.join("a (2).jpg"));
        std::fs::write(dir.join("a (2).jpg"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a.jpg"), dir.join("a (3).jpg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counts_read_naturally() {
        assert_eq!(plural(1, "photo", "items"), "photo");
        assert_eq!(plural(3, "photo", "items"), "3 items");
    }
}
