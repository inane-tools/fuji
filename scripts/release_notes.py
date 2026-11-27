"""Release title and notes, as CI publishes them.

    python3 scripts/release_notes.py codename 1.2.3   -> Gaifū Kaisei
    python3 scripts/release_notes.py notes 1.2.3      -> the release's notes

The codename comes from crates/fuji/src/release.rs (the same list Settings
shows), and the notes from docs/releases/<version>.md, followed by the
changelog link and how to install.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
REPO = "https://github.com/inane-tools/fuji"
INSTALL = (
    "Windows: use the setup executable. It installs for your account only and "
    "isn't code-signed, so SmartScreen may ask you to choose More info → Run "
    "anyway. Linux: use the AppImage or install the Flatpak bundle "
    "(`flatpak install --user fuji-linux-x86_64.flatpak`). Flatpak updates are "
    "manual; Windows and the AppImage use Fuji's GitHub updater. Each file has a "
    "`.sig` signature that the updater checks, and `SHA256SUMS` lists their "
    "checksums."
)


def codename(version: str) -> str:
    source = (ROOT / "crates/fuji/src/release.rs").read_text(encoding="utf-8")
    views = source.split("const VIEWS", 1)[1].split("];", 1)[0]
    names = re.findall(r'\(\s*"([^"]+)",\s*"[^"]*",?\s*\)', views)
    major, minor = (int(n) for n in version.split(".")[:2])
    index = max(0, major - 1) * 10 + minor
    if index >= len(names):
        sys.exit(f"No codename for {version}")
    return names[index]


def notes(version: str) -> str:
    path = ROOT / "docs/releases" / f"{version}.md"
    if not path.exists():
        sys.exit(f"Write {path.relative_to(ROOT)} first")
    body = path.read_text(encoding="utf-8").strip()
    return f"{body}\n\n[Full changelog]({REPO}/blob/main/CHANGELOG.md)\n\n{INSTALL}\n"


if __name__ == "__main__":
    what, version = sys.argv[1], sys.argv[2]
    sys.stdout.reconfigure(encoding="utf-8")
    print(codename(version) if what == "codename" else notes(version), end="")
