#!/usr/bin/env python3
"""One release from the Mac's and the Windows builds' assets (release.yml's draft job). No network.

    python3 -I tools/release-merge.py <mac dir> <windows dir>

<mac dir> holds app/release-zip.sh's output (Seedwell-X.Y.Z.zip, Seedwell-X.Y.Z.dmg, Seedwell.dmg, install.sh,
SHA256SUMS, latest.json, NOTES.md),
<windows dir> app-windows/release-zip.ps1's (Seedwell-X.Y.Z-windows-x64.zip, install.ps1, SHA256SUMS-windows,
latest-windows.json). The files in <mac dir> are rewritten in place:

- SHA256SUMS: the Mac's lines, then the Windows ones (a file listed twice keeps its last line).
- latest.json: every field the Mac wrote, unchanged and in order (installed Mac copies read them), plus
  "windows": {"zip", "sha256", "minimum_windows"} (what the Windows app reads; crates/ffi/src/updates.rs).
- NOTES.md: the Install section for both systems, and the Checksums block from the merged SHA256SUMS.

Refuses (exit 1) when the two builds are not of the same version, a checksum does not match its file, an asset is
missing, or Seedwell.dmg (the stable "latest" link) is not the same file as Seedwell-X.Y.Z.dmg.
"""
import hashlib
import json
import re
import sys
from pathlib import Path

WINDOWS_INSTALL = """**Windows** (64-bit Windows 10 version 1809 or later, or Windows 11), in PowerShell:

```powershell
irm https://github.com/kkollsga/seedwell/releases/latest/download/install.ps1 | iex
```

Installs for your user only, no administrator rights. Run the same command again to update; uninstall from
Settings › Apps › Installed apps."""


def die(msg: str) -> None:
    print(f"release-merge: {msg}", file=sys.stderr)
    sys.exit(1)


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def sums(path: Path) -> list[tuple[str, str]]:
    out = []
    for line in path.read_text().splitlines():
        if line.strip():
            digest, name = line.split(None, 1)
            out.append((digest.lower(), name.lstrip("*")))
    return out


def main() -> None:
    if len(sys.argv) != 3:
        die("usage: release-merge.py <mac dir> <windows dir>")
    mac, win = Path(sys.argv[1]), Path(sys.argv[2])
    latest = json.loads((mac / "latest.json").read_text(), object_pairs_hook=dict)
    version = latest.get("version", "")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        die(f"the Mac's latest.json has no plain version: {version!r}")
    windows = json.loads((win / "latest-windows.json").read_text())
    zip_name = f"Seedwell-{version}-windows-x64.zip"
    if windows.get("zip") != zip_name:
        die(f"the Windows build is {windows.get('zip')!r}, the Mac's is version {version}")
    if not (win / zip_name).is_file() or sha256(win / zip_name) != str(windows.get("sha256", "")).lower():
        die(f"latest-windows.json's sha256 is not {zip_name}'s")
    if not re.fullmatch(r"\d+(\.\d+){0,2}", str(windows.get("minimum_windows", ""))):
        die(f"latest-windows.json's minimum_windows is not a version: {windows.get('minimum_windows')!r}")

    # SHA256SUMS: each listed file must be the one next to it.
    merged: dict[str, str] = {}
    for d, f in ((mac, "SHA256SUMS"), (win, "SHA256SUMS-windows")):
        for digest, name in sums(d / f):
            if not (d / name).is_file() or sha256(d / name) != digest:
                die(f"{f} lists {name} with a checksum its file does not have")
            merged[name] = digest
    for name in (f"Seedwell-{version}.zip", f"Seedwell-{version}.dmg", "Seedwell.dmg", "install.sh", zip_name, "install.ps1"):
        if name not in merged:
            die(f"no checksum for {name}")
    if merged["Seedwell.dmg"] != merged[f"Seedwell-{version}.dmg"]:
        die(f"Seedwell.dmg is not the same file as Seedwell-{version}.dmg")
    sums_text = "".join(f"{digest}  {name}\n" for name, digest in merged.items())
    (mac / "SHA256SUMS").write_text(sums_text)

    latest["windows"] = {"zip": zip_name, "sha256": windows["sha256"].lower(), "minimum_windows": windows["minimum_windows"]}
    (mac / "latest.json").write_text(json.dumps(latest, separators=(",", ":")) + "\n")

    # NOTES.md: the Mac's Install section gets the Windows one after it; the Checksums block lists every file.
    notes = (mac / "NOTES.md").read_text()
    install = re.search(r"(## Install\n)(.*?)(\n## )", notes, re.S)
    if not install:
        die("NOTES.md has no Install section")
    mac_part = install.group(2).strip()
    if not mac_part.startswith("**Mac**"):
        mac_part = "**Mac**, in Terminal:\n\n" + mac_part
    notes = notes[: install.start(2)] + "\n" + mac_part + "\n\n" + WINDOWS_INSTALL + "\n" + notes[install.end(2) :]
    checks = re.search(r"(## Checksums\n\n```\n)(.*?)(```)", notes, re.S)
    if not checks:
        die("NOTES.md has no Checksums block")
    notes = notes[: checks.start(2)] + sums_text + notes[checks.end(2) :]
    (mac / "NOTES.md").write_text(notes)
    print(f"release-merge: Seedwell {version} for the Mac and Windows: {len(merged)} checksums, latest.json, NOTES.md")


if __name__ == "__main__":
    main()
