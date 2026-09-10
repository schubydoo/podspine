#!/usr/bin/env python3
"""Verify or refresh the pinned Zig download in the setup-zig composite action.

Renovate's custom manager bumps ``ZIG_VERSION`` in
``.github/actions/setup-zig/action.yml`` from the ``ziglang/zig`` releases, but it
cannot recompute the download URL or the tarball checksum. This helper reads the
pinned ``ZIG_VERSION`` from that file, looks the version up in Zig's authoritative
download index (``https://ziglang.org/download/index.json``), and reads the
``x86_64-linux`` tarball URL and its ``shasum``.

Without ``--fix`` it verifies that ``ZIG_URL`` and ``ZIG_SHA256`` in the file already
match the index, prints the correct values on a mismatch, and exits non-zero. With
``--fix`` it rewrites those two lines in place so a Renovate version bump lands with a
matching URL and digest in one commit (the postUpgradeTask in the renovate-config
``podspine`` preset runs ``--fix``). Standard library only. Usage::

    python scripts/update_zig_pin.py [--fix]
"""

from __future__ import annotations

import json
import re
import sys
import urllib.request
from pathlib import Path
from typing import NoReturn

INDEX_URL = "https://ziglang.org/download/index.json"
ACTION = Path(__file__).resolve().parents[1] / ".github/actions/setup-zig/action.yml"
PLATFORM = "x86_64-linux"

_VERSION_RE = re.compile(r'ZIG_VERSION:\s*"([^"]+)"')
_URL_RE = re.compile(r'(ZIG_URL:\s*")([^"]+)(")')
_SHA_RE = re.compile(r'(ZIG_SHA256:\s*")[0-9a-f]{64}(")')


def fail(message: str) -> NoReturn:
    """Print an error and exit non-zero, so a bad pin never passes silently."""
    print(f"update_zig_pin: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    fix = "--fix" in sys.argv[1:]

    text = ACTION.read_text(encoding="utf-8")
    version_match = _VERSION_RE.search(text)
    if version_match is None:
        fail(f"no ZIG_VERSION found in {ACTION}")
    version = version_match.group(1)

    with urllib.request.urlopen(INDEX_URL, timeout=30) as response:
        index = json.load(response)
    entry = index.get(version, {}).get(PLATFORM)
    if not entry or "tarball" not in entry or "shasum" not in entry:
        fail(f"Zig {version} has no {PLATFORM} entry in the download index")
    want_url = entry["tarball"]
    want_sha = entry["shasum"]

    have_url = _URL_RE.search(text)
    have_sha = _SHA_RE.search(text)
    if have_url is None or have_sha is None:
        fail(f"no ZIG_URL / ZIG_SHA256 lines found in {ACTION}")

    up_to_date = have_url.group(2) == want_url and text.count(want_sha) == 1

    if not fix:
        if up_to_date:
            print(f"Zig pin is current: {version} ({PLATFORM})")
            return
        fail(
            "Zig pin is stale. Run with --fix, or set:\n"
            f"  ZIG_URL: \"{want_url}\"\n"
            f"  ZIG_SHA256: \"{want_sha}\""
        )

    updated = _URL_RE.sub(rf"\g<1>{want_url}\g<3>", text)
    updated = _SHA_RE.sub(rf"\g<1>{want_sha}\g<2>", updated)
    if updated == text:
        print(f"Zig pin already matches {version}; nothing to fix")
        return
    ACTION.write_text(updated, encoding="utf-8")
    print(f"Updated Zig pin to {version}: {want_url}")


if __name__ == "__main__":
    main()
