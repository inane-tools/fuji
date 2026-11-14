# fuji

A native desktop client for **iCloud Photos** (Apple Photos) on Windows and Linux.

Apple ships Photos only for Apple platforms. On Windows the official option is
"iCloud for Windows", which only syncs to a folder, and Linux has nothing. fuji
aims to be a real photo-library app: browse, search, view, download, and upload
your iCloud library without a Mac.

## Status

Early. Working so far:

- Apple ID sign-in (SRP, two-factor by trusted device or SMS, trusted
  sessions), with the session kept in the OS keychain
- The photo grid shows your real iCloud library, newest first: Library,
  Favorites, Videos and Recently Deleted. Items load in pages as you scroll;
  thumbnails download for what's on screen, are cached on disk, and are held
  in a bounded GPU cache so memory stays flat on large libraries.

Not yet: full-size viewer, video playback, albums, shared albums, editing.

`cargo test -p fuji-icloud -- --ignored` runs a live check that Apple accepts
fuji's sign-in request (it uses a made-up address, not your account).

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

Apple has no public API for iCloud Photos. fuji will use the same private web
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

## Disclaimer

fuji is not affiliated with or endorsed by Apple. "Apple", "iCloud", and
"Apple Photos" are trademarks of Apple Inc.
