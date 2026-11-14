//! Moving photos in and out of the gallery: copy to the clipboard, download
//! originals, drag out as files, and paste in to upload.
//!
//! Copies and drags hand over the full-size JPEG rendition (the original
//! itself when that is a JPEG or PNG), which every app can open. Videos, and
//! "Download", use the original file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fuji_icloud::{Asset, AssetKind, PhotosError};
use gpui::{
    AppContext as _, ClipboardEntry, ClipboardItem, Context, Image, ImageFormat, PathPromptOptions,
    SharedString, Task, Window,
};

use crate::grid::PhotoGrid;
use crate::platform::{self, ClipboardContent};
use crate::ui::Status;
use crate::{export, net, thumbs};

/// How long to wait for a photo's record (its download URLs) to load.
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);
/// Larger originals aren't read into memory for an upload.
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// iCloud takes a moment to list an upload.
const REFRESH_AFTER_UPLOAD: Duration = Duration::from_secs(4);

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

    /// Upload what [`Self::paste`] offered.
    pub fn confirm_upload(&mut self, cx: &mut Context<Self>) {
        let Some(upload) = self.pending_upload.take() else {
            return;
        };
        let client = self.client.clone();
        let total = upload.items.len();
        cx.spawn(async move |this, cx| {
            let mut failed = None;
            for (done, item) in upload.items.into_iter().enumerate() {
                let text = if total == 1 {
                    "Adding to iCloud Photos\u{2026}".to_string()
                } else {
                    format!("Adding {} of {total} to iCloud Photos\u{2026}", done + 1)
                };
                this.update(cx, |this, cx| {
                    this.set_status(Status::Working(text.into()), cx)
                })
                .ok();
                let client = client.clone();
                let result = net::run(move || {
                    let bytes = match item.source {
                        UploadSource::Bytes(bytes) => bytes,
                        UploadSource::File(path) => read_for_upload(&path)?,
                    };
                    client.upload(&item.name, &bytes).map_err(message)
                })
                .await;
                match result {
                    Some(Ok(())) => {}
                    Some(Err(error)) => {
                        failed = Some(error);
                        break;
                    }
                    None => {
                        failed = Some("Upload failed.".into());
                        break;
                    }
                }
            }
            let status = match failed {
                Some(error) => Status::Failed(error),
                None => Status::Done {
                    message: format!(
                        "Added {} to iCloud Photos",
                        plural(total, "1 item", "items")
                    )
                    .into(),
                    reveal: None,
                },
            };
            this.update(cx, |this, cx| this.set_status(status, cx)).ok();
            // Show the new arrivals once iCloud has listed them.
            cx.background_executor().timer(REFRESH_AFTER_UPLOAD).await;
            this.update(cx, |this, cx| this.refresh(false, cx)).ok();
        })
        .detach();
    }
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

fn read_for_upload(path: &Path) -> Result<Vec<u8>, SharedString> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let size = std::fs::metadata(path)
        .map_err(|err| SharedString::from(format!("Couldn't read {name}: {err}")))?
        .len();
    if size > MAX_UPLOAD_BYTES {
        return Err(format!("{name} is too large to upload from here.").into());
    }
    std::fs::read(path).map_err(|err| format!("Couldn't read {name}: {err}").into())
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
