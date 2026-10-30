#!/usr/bin/env python3
"""Tests of tools/release-merge.py with fake assets (release.yml's draft job runs them first). No network.

    python3 -I tools/release_merge_test.py
"""
from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

MERGE = Path(__file__).resolve().parent / "release-merge.py"
V = "0.1.11"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Release:
    """A Mac and a Windows build's output, as release-zip.sh and release-zip.ps1 write them."""

    def __init__(self, root: Path, *, dmg_copy: bytes | None = None, drop: str | None = None, win_version: str = V):
        self.mac, self.win = root / "mac", root / "windows"
        self.mac.mkdir()
        self.win.mkdir()
        dmg = b"disk image " + V.encode()
        mac_files = {
            f"Seedwell-{V}.zip": b"mac zip",
            "install.sh": b"#!/bin/bash\n",
            f"Seedwell-{V}.dmg": dmg,
            "Seedwell.dmg": dmg if dmg_copy is None else dmg_copy,
        }
        if drop:
            mac_files.pop(drop)
        for name, data in mac_files.items():
            (self.mac / name).write_bytes(data)
        (self.mac / "SHA256SUMS").write_text("".join(f"{sha(d)}  {n}\n" for n, d in mac_files.items()))
        (self.mac / "latest.json").write_text(json.dumps({
            "version": V, "tag": f"v{V}", "notes_url": f"https://github.com/kkollsga/seedwell/releases/tag/v{V}",
            "minimum_macos": "15.0", "zip": f"Seedwell-{V}.zip", "sha256": sha(b"mac zip"),
        }))
        (self.mac / "NOTES.md").write_text(
            "## Install\n\n**Mac** (macOS 15 or later, Apple silicon): download the disk image.\n\n"
            "## Changes\n\n- One change.\n\n## Checksums\n\n```\nold\n```\n"
        )
        win_zip = f"Seedwell-{win_version}-windows-x64.zip"
        win_files = {win_zip: b"windows zip", "install.ps1": b"# ps1\n"}
        for name, data in win_files.items():
            (self.win / name).write_bytes(data)
        (self.win / "SHA256SUMS-windows").write_text("".join(f"{sha(d)}  {n}\n" for n, d in win_files.items()))
        (self.win / "latest-windows.json").write_text(json.dumps({
            "zip": win_zip, "sha256": sha(b"windows zip"), "minimum_windows": "10.0.17763",
        }))

    def merge(self) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, "-I", str(MERGE), str(self.mac), str(self.win)], capture_output=True, text=True)


class MergeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_one_release_with_every_asset_its_checksums_and_both_install_sections(self):
        r = Release(self.root)
        out = r.merge()
        self.assertEqual(out.returncode, 0, out.stderr)
        sums = (r.mac / "SHA256SUMS").read_text()
        for name in (f"Seedwell-{V}.zip", f"Seedwell-{V}.dmg", "Seedwell.dmg", "install.sh",
                     f"Seedwell-{V}-windows-x64.zip", "install.ps1"):
            self.assertIn(f"  {name}\n", sums)
        latest = json.loads((r.mac / "latest.json").read_text())
        self.assertEqual(latest["zip"], f"Seedwell-{V}.zip")
        self.assertEqual(latest["windows"]["zip"], f"Seedwell-{V}-windows-x64.zip")
        notes = (r.mac / "NOTES.md").read_text()
        self.assertIn("**Mac**", notes)
        self.assertIn("**Windows**", notes)
        self.assertLess(notes.index("**Mac**"), notes.index("**Windows**"))
        self.assertIn(f"  Seedwell-{V}.dmg\n", notes.split("## Checksums")[1])

    def test_a_release_without_its_disk_image_is_refused(self):
        r = Release(self.root, drop=f"Seedwell-{V}.dmg")
        out = r.merge()
        self.assertEqual(out.returncode, 1)
        self.assertIn(f"no checksum for Seedwell-{V}.dmg", out.stderr)

    def test_the_stable_disk_image_must_be_the_versioned_one(self):
        r = Release(self.root, dmg_copy=b"another disk image")
        out = r.merge()
        self.assertEqual(out.returncode, 1)
        self.assertIn("Seedwell.dmg is not the same file", out.stderr)

    def test_builds_of_two_versions_are_refused(self):
        r = Release(self.root, win_version="0.1.12")
        out = r.merge()
        self.assertEqual(out.returncode, 1)
        self.assertIn("the Windows build is", out.stderr)

    def test_a_file_that_does_not_match_its_checksum_is_refused(self):
        r = Release(self.root)
        (r.mac / f"Seedwell-{V}.dmg").write_bytes(b"changed after the checksums were made")
        out = r.merge()
        self.assertEqual(out.returncode, 1)
        self.assertIn("with a checksum its file does not have", out.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
