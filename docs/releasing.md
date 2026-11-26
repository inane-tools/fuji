# Releasing Fuji

Releases are made by CI (`.github/workflows/release.yml`) when a version tag
is pushed. Fuji's updater then finds them on GitHub.

## Once: the signing key

Fuji only installs updates signed with the release key. Its public half is
`PUBLIC_KEY` in `crates/fuji/src/updater.rs`; the private half is a P-256 key
in PEM form, kept outside the repository and in the repository's Actions
secrets as `UPDATE_SIGNING_KEY`:

```sh
gh secret set UPDATE_SIGNING_KEY --repo inane-tools/fuji < path/to/update-signing-key.pem
```

Keep a copy of the key somewhere safe. If it's lost, published copies of
Fuji can't take updates signed with a new one: their users would have to
download the next version by hand. To change keys on purpose, ship a version
with the new `PUBLIC_KEY`, signed with the old key, before signing with the
new one.

A new key is made with:

```sh
openssl ecparam -name prime256v1 -genkey -noout -out update-signing-key.pem
openssl ec -in update-signing-key.pem -pubout -outform DER | tail -c 65 | od -An -tx1 | tr -d ' \n'
```

The second line prints the public half for `PUBLIC_KEY`.

## Each release

1. Set the version in the workspace `Cargo.toml` (`[workspace.package]`)
   and commit it. A new minor version gets the next codename (see
   `crates/fuji/src/release.rs`).
2. Tag that commit and push the tag:

   ```sh
   git tag v1.0.1
   git push origin v1.0.1
   ```

3. CI checks that the tag matches the version, runs the tests, builds
   and packages Windows and Linux, signs each file, and publishes the
   release with generated notes. Edit the notes on GitHub afterwards if
   you like.

To try the packaging without publishing, run the Release workflow by hand
(Actions → Release → Run workflow): it does everything but publish, and
keeps the signed files as the run's `release-preview` artifact.

Each release has:

| File | What it is |
| --- | --- |
| `fuji-windows-x86_64-setup.exe` | The Windows installer (Inno Setup, `packaging/windows/fuji.iss`), for the current user |
| `fuji-linux-x86_64.AppImage` | Linux, as an AppImage |
| `fuji-linux-x86_64.flatpak` | Linux, as a Flatpak bundle (`packaging/linux/tools.inane.Fuji.yml`) |
| `<file>.sig` | Each file's signature: ECDSA P-256 over SHA-256, DER |
| `SHA256SUMS` | Checksums of the files and signatures |

The updater looks for exactly these names, so keep them. It ignores drafts
and pre-releases, and only works while the repository is public (GitHub
doesn't answer for private ones without a token).

## How updating works

Shortly after launch, then every six hours (unless turned off in Settings),
Fuji asks GitHub for the latest release. If it's newer, Fuji says so: a ring
and badge on the account picture, "Update available" at the top of the
account menu, and the updates block in Settings' About card. Nothing is
downloaded until the user clicks Download; Fuji then fetches the file for
its system, shows the progress and checks the signature. Install and
restart then:

- on Windows, runs the new installer silently; it closes Fuji, installs
  over it and starts the new version;
- in an AppImage, moves the running AppImage aside (removed on the next
  launch), puts the new one in its place and starts it.

A Flatpak, or a Fuji built from source, isn't replaced: Settings offers
the releases page instead.

Development builds (`cargo run`) look for updates but never install them,
and `FUJI_NO_UPDATE=1` turns installing off for any build (for packagers).

To try the whole flow without a release, start Fuji with
`FUJI_FAKE_UPDATE=1.0.1` (or `1.0.1:fail` for a failed download): it
pretends that version is out, makes up the download, and installing only
restarts Fuji as it is.
