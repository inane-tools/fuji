# Fuji

A native desktop client for **iCloud Photos** (Apple Photos) on Windows and Linux.

Apple ships Photos only for Apple platforms. On Windows the official option is
"iCloud for Windows", which only syncs to a folder, and Linux has nothing. Fuji
aims to be a real photo-library app: browse, search, view, download, and upload
your iCloud library without a Mac.

## Status

Early. Working so far:

- Apple ID sign-in (SRP, two-factor by trusted device or SMS, trusted
  sessions), with the session kept in the OS keychain
- The gallery shows your iCloud library as a mosaic (justified rows that
  keep each photo's shape) under year, month and day headers: Library,
  Favorites, Videos and Recently Deleted. The album index is cached on disk,
  so it opens instantly and refreshes from iCloud in the background.
  Thumbnails already on disk show without any network request; memory stays
  bounded however far you scroll.
- Restarts continue the existing iCloud session instead of signing in again
  (which would make Apple email you). Cookies are kept in an AES-256-GCM
  encrypted file; the key and tokens live in the OS keychain.

- Click a photo for the full-size viewer, letterboxed on black. Zoom with the
  mouse wheel (toward the cursor), a touchpad pinch, double-click (fit / actual
  size), or `+` `-` `0`; drag to pan. Arrow keys to move, Esc to close,
  Copy (Ctrl+C) puts the image on the clipboard, Download (Ctrl+S) saves the
  original file (HEIC, RAW, MOV, ...).

Not yet: video playback, albums, shared albums, editing, HEIC decoding for
originals that have no JPEG rendition.

`cargo test -p fuji-icloud -- --ignored` runs a live check that Apple accepts
Fuji's sign-in request (it uses a made-up address, not your account).

## Stack

Pure Rust, with [GPUI](https://gpui.rs) (Zed's GPU-accelerated UI framework)
for a fast, smooth, low-memory app. GPUI is pre-1.0, so it is pinned to an
exact version in the workspace `Cargo.toml`; upgrade it on purpose.

| Crate | What it is |
| --- | --- |
| `crates/fuji` | The desktop app (GPUI) |
| `crates/fuji-icloud` | Library model and the iCloud Photos client (currently a `MockLibrary`) |

## Building

Requires stable Rust (see `rust-toolchain.toml`).

```sh
cargo run -p fuji
```

**Linux** needs Vulkan drivers plus the Wayland/X11 development headers, for
example on Debian/Ubuntu:

```sh
sudo apt install libvulkan1 libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libfontconfig-dev
```

**Windows** needs the MSVC build tools. Keep the checkout at a short path: build
artifacts nest deeply and fail to link past Windows' 260-character path limit.
If that happens, set `CARGO_TARGET_DIR` to a short folder such as `C:\t\fuji`.

## Goals

- Sign in with an Apple ID, including two-factor authentication
- Browse the library: timeline, albums, shared albums, favorites, hidden, recently deleted
- Fast thumbnail grid with local caching
- View full-resolution photos and videos, including Live Photos and HEIC
- Download originals, or sync the library to a local folder
- Upload new photos and videos
- Basic edits that iCloud supports: favorite, hide, delete, add to album

## Non-goals (for now)

- Editing pixels (crop, filters, adjustments)
- Reading a local macOS `Photos.photoslibrary` package

## How it talks to iCloud

Apple has no public API for iCloud Photos. Fuji will use the same private web
endpoints that icloud.com uses (CloudKit-backed, reached after Apple ID SRP
sign-in and 2FA). Open-source projects such as `pyicloud` and `icloudpd` have
documented much of this. Because the API is private, Apple can change it at any
time and things may break.

Notes:

- Accounts with **Advanced Data Protection** turned on cannot use web access
  unless "Access iCloud Data on the Web" is enabled on a trusted device.
- Session tokens must be stored in the OS keychain (Windows Credential Manager,
  Secret Service / libsecret on Linux), never in plain files.

## Platforms

- Windows 10/11 (x64, arm64)
- Linux (x64, arm64): Flatpak / AppImage

## Credits

- Icons: [Phosphor](https://phosphoricons.com) (MIT), in `crates/fuji/assets/icons`
- Font: Gabarito (SIL Open Font License), in `crates/fuji/assets`

## Disclaimer

Fuji is not affiliated with or endorsed by Apple. "Apple", "iCloud", and
"Apple Photos" are trademarks of Apple Inc.
