# fuji

A native desktop client for **iCloud Photos** (Apple Photos) on Windows and Linux.

Apple ships Photos only for Apple platforms. On Windows the official option is
"iCloud for Windows", which only syncs to a folder, and Linux has nothing. fuji
aims to be a real photo-library app: browse, search, view, download, and upload
your iCloud library without a Mac.

## Status

Early planning. Nothing works yet.

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
