//! The few things GPUI doesn't do: putting files on the clipboard, reading
//! files from it, and dragging files out to other apps.
//!
//! Windows only for now; elsewhere these report "not supported" and callers
//! fall back to what GPUI offers.

use std::path::{Path, PathBuf};

/// What to put on the clipboard for a copy.
pub struct ClipboardContent {
    /// Files, for pasting into a folder, a chat or a mail.
    pub files: Vec<PathBuf>,
    /// The same picture as image data, for pasting into an image editor.
    pub jpeg: Option<Vec<u8>>,
    pub png: Option<Vec<u8>>,
}

/// File types iCloud Photos accepts as uploads.
pub fn is_uploadable(path: &Path) -> bool {
    const EXTENSIONS: [&str; 14] = [
        "jpg", "jpeg", "png", "heic", "heif", "gif", "tif", "tiff", "dng", "webp", "mov", "mp4",
        "m4v", "bmp",
    ];
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, POINT};
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_DataObject, DROPFILES, DragQueryFileW, HDROP, ILCreateFromPathW, ILFree,
        SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
    };
    use windows::core::{HSTRING, w};

    use super::ClipboardContent;

    struct Clipboard;

    impl Clipboard {
        fn open() -> Option<Self> {
            // Another app may hold the clipboard for a moment; try a few times.
            for _ in 0..10 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Some(Self);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn set(format: u32, data: &[u8]) -> bool {
        unsafe {
            let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, data.len()) else {
                return false;
            };
            let target = GlobalLock(global);
            if target.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), target.cast::<u8>(), data.len());
            let _ = GlobalUnlock(global);
            // On success the clipboard owns the memory.
            SetClipboardData(format, Some(HANDLE(global.0))).is_ok()
        }
    }

    /// A `DROPFILES` header followed by the paths, each ending in a NUL, and
    /// one more NUL at the end.
    fn drop_files(files: &[PathBuf]) -> Vec<u8> {
        let header = DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32,
            pt: POINT::default(),
            fNC: false.into(),
            fWide: true.into(),
        };
        let mut bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw const header).cast::<u8>(),
                std::mem::size_of::<DROPFILES>(),
            )
        }
        .to_vec();
        for file in files {
            for unit in file.as_os_str().encode_wide().chain(Some(0)) {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    pub fn copy(content: &ClipboardContent) -> bool {
        let Some(_clipboard) = Clipboard::open() else {
            return false;
        };
        if unsafe { EmptyClipboard() }.is_err() {
            return false;
        }
        let mut ok = true;
        if !content.files.is_empty() {
            ok &= set(CF_HDROP.0 as u32, &drop_files(&content.files));
        }
        if let Some(png) = &content.png {
            ok &= set(unsafe { RegisterClipboardFormatW(w!("PNG")) }, png);
        }
        if let Some(jpeg) = &content.jpeg {
            ok &= set(unsafe { RegisterClipboardFormatW(w!("JFIF")) }, jpeg);
        }
        ok
    }

    pub fn clipboard_files() -> Vec<PathBuf> {
        let mut files = Vec::new();
        if unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) }.is_err() {
            return files;
        }
        let Some(_clipboard) = Clipboard::open() else {
            return files;
        };
        unsafe {
            let Ok(handle) = GetClipboardData(CF_HDROP.0 as u32) else {
                return files;
            };
            let global = HGLOBAL(handle.0);
            let data = GlobalLock(global);
            if data.is_null() {
                return files;
            }
            let hdrop = HDROP(data);
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buffer = vec![0u16; len + 1];
                if DragQueryFileW(hdrop, i, Some(&mut buffer)) > 0 {
                    files.push(std::ffi::OsString::from_wide(&buffer[..len]).into());
                }
            }
            let _ = GlobalUnlock(global);
        }
        files
    }

    pub fn left_button_down() -> bool {
        unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0 }
    }

    struct IdList(*mut ITEMIDLIST);

    impl Drop for IdList {
        fn drop(&mut self) {
            unsafe { ILFree(Some(self.0)) };
        }
    }

    /// Run a shell drag of `files` from the window. Blocks, pumping the
    /// window's messages, until the files are dropped or the drag is
    /// cancelled.
    pub fn drag_files(hwnd: isize, files: &[PathBuf]) -> bool {
        let ids: Vec<IdList> = files
            .iter()
            .filter_map(|file| {
                let path: &Path = file;
                let id = unsafe { ILCreateFromPathW(&HSTRING::from(path)) };
                (!id.is_null()).then_some(IdList(id))
            })
            .collect();
        if ids.is_empty() {
            return false;
        }
        let pointers: Vec<*const ITEMIDLIST> = ids.iter().map(|id| id.0.cast_const()).collect();
        unsafe {
            let Ok(items) = SHCreateShellItemArrayFromIDLists(&pointers) else {
                return false;
            };
            let Ok(data) = items.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) else {
                return false;
            };
            // With no drop source of our own, the shell supplies one (and
            // the drag picture).
            SHDoDragDrop(
                Some(HWND(hwnd as *mut core::ffi::c_void)),
                &data,
                None,
                DROPEFFECT_COPY,
            )
            .is_ok()
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    use super::ClipboardContent;

    pub fn copy(_: &ClipboardContent) -> bool {
        false
    }

    pub fn clipboard_files() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn left_button_down() -> bool {
        false
    }

    pub fn drag_files(_: isize, _: &[PathBuf]) -> bool {
        false
    }
}

/// Replace the clipboard with `content`. `false` if that isn't supported
/// here or failed.
pub fn copy(content: &ClipboardContent) -> bool {
    imp::copy(content)
}

/// Files on the clipboard (copied in a file manager, say).
pub fn clipboard_files() -> Vec<PathBuf> {
    imp::clipboard_files()
}

pub fn left_button_down() -> bool {
    imp::left_button_down()
}

/// Drag `files` out of the window with native handle `window`. Blocks until
/// the drop; call it from the main thread, outside any app update.
pub fn drag_files(window: isize, files: &[PathBuf]) -> bool {
    imp::drag_files(window, files)
}

/// The window's native handle (an `HWND` on Windows), for [`drag_files`].
pub fn native_handle(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Whether dragging files out works on this system.
pub const CAN_DRAG_OUT: bool = cfg!(windows);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_types() {
        assert!(is_uploadable(Path::new("a/IMG_1.HEIC")));
        assert!(is_uploadable(Path::new("clip.mov")));
        assert!(!is_uploadable(Path::new("notes.txt")));
        assert!(!is_uploadable(Path::new("no-extension")));
    }
}
