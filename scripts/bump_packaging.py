#!/usr/bin/env python3
"""Regenerate podspine's packaging manifests to a released version from checksums.txt.

After a release publishes, the Homebrew formula, the Scoop manifest, and the Nix
flake's version must point at the new release. This
helper rewrites whichever of those exist in the working tree, in place, keying every
checksum off the release's authoritative ``checksums.txt`` (the list of what was
actually built and signed). The Nix flake builds from source, so only its version
string is bumped (no per-arch hashes).

Driven by ``.github/workflows/packaging-bump.yml``. A manifest that is not present is
skipped. It fails closed rather than write a half-baked manifest: a manifest's
required asset must be in ``checksums.txt``, and after rewriting no manifest may still
reference a non-target version. Standard library only. Usage::

    python scripts/bump_packaging.py <version> <owner/repo> <path-to-checksums.txt>
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")
# Real binary asset tokens: <os>-<arch>[.exe]. Excludes the .sbom.cdx.json sidecars
# that also share the podspine-v<ver>- prefix in checksums.txt.
_ARCH_RE = re.compile(r"^(?:linux|darwin|windows)-(?:amd64|arm64)(?:\.exe)?$")
# Any podspine-v<x.y.z>- asset token left after a bump: a missed arch.
_ASSET_VERSION_RE = re.compile(r"podspine-v(\d+\.\d+\.\d+)-")


def parse_sums(text: str) -> dict[str, str]:
    """Parse ``checksums.txt`` text into an ``{asset_filename: sha256}`` mapping."""
    sums: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 2 or not _SHA256_RE.match(parts[0]):
            continue
        digest, name = parts
        sums[name.lstrip("*")] = digest  # coreutils marks binary mode with a leading '*'
    return sums


def asset_url(repo: str, version: str, asset: str) -> str:
    """Build the release download URL for one asset filename."""
    return f"https://github.com/{repo}/releases/download/v{version}/{asset}"


def binary_assets(version: str, sums: dict[str, str]) -> dict[str, str]:
    """Map each real ``podspine-v<version>-<os>-<arch>`` binary to its token + digest."""
    prefix = f"podspine-v{version}-"
    return {
        asset[len(prefix) :]: digest
        for asset, digest in sums.items()
        if asset.startswith(prefix) and _ARCH_RE.match(asset[len(prefix) :])
    }


def stale_versions(text: str, version: str) -> set[str]:
    """Return any asset-token versions in ``text`` other than the target version."""
    return set(_ASSET_VERSION_RE.findall(text)) - {version}


def bump_formula(text: str, version: str, repo: str, bins: dict[str, str]) -> str:
    """Rewrite the Homebrew formula's version and each present arch's URL + sha256."""
    text = re.sub(r'version "[^"]+"', f'version "{version}"', text, count=1)
    for arch, digest in bins.items():
        if arch.endswith(".exe"):  # Windows isn't in the formula (Scoop covers it)
            continue
        url = asset_url(repo, version, f"podspine-v{version}-{arch}")
        block = re.compile(
            r'(?P<head>url ")[^"]*-'
            + re.escape(arch)
            + r'(?P<mid>"\s*\n\s*sha256 ")[0-9a-f]{64}(?P<tail>")'
        )
        text = block.sub(
            lambda m, url=url, digest=digest: f"{m['head']}{url}{m['mid']}{digest}{m['tail']}",
            text,
        )
    return text


def bump_scoop(text: str, version: str, repo: str, bins: dict[str, str]) -> str:
    """Rewrite the Scoop manifest's version, URL, and hash (Windows amd64)."""
    token = "windows-amd64.exe"
    digest = bins.get(token)
    if digest is None:
        raise ValueError(f"Scoop: podspine-v{version}-{token} missing from checksums.txt")
    data = json.loads(text)
    data["version"] = version
    arch = data["architecture"]["64bit"]
    arch["url"] = f"{asset_url(repo, version, f'podspine-v{version}-{token}')}#/podspine.exe"
    arch["hash"] = digest
    return json.dumps(data, indent=4) + "\n"


def bump_flake(text: str, version: str) -> str:
    """Rewrite the Nix flake's version string (it builds from source — no hashes)."""
    return re.sub(r'(version = ")[^"]+(";)', rf"\g<1>{version}\g<2>", text, count=1)


def main(argv: list[str]) -> int:
    """Bump every present packaging manifest to ``version`` from ``checksums.txt``."""
    if len(argv) != 4:
        print("usage: bump_packaging.py <version> <owner/repo> <checksums.txt>", file=sys.stderr)
        return 2
    version, repo, sums_path = argv[1], argv[2], argv[3]
    if not _VERSION_RE.match(version):
        print(f"error: version {version!r} is not MAJOR.MINOR.PATCH", file=sys.stderr)
        return 2

    sums = parse_sums(Path(sums_path).read_text())
    bins = binary_assets(version, sums)
    if not bins:
        print(f"error: checksums.txt has no podspine-v{version}-* binaries", file=sys.stderr)
        return 1

    manifests = {
        Path("Formula/podspine.rb"): lambda t: bump_formula(t, version, repo, bins),
        Path("packaging/scoop/podspine.json"): lambda t: bump_scoop(t, version, repo, bins),
        Path("flake.nix"): lambda t: bump_flake(t, version),
    }

    # First pass: compute + validate every present manifest. Fail closed BEFORE any
    # write so a mid-sequence abort never leaves a half-rewritten tree.
    planned: list[tuple[Path, str]] = []
    try:
        for path, rewrite in manifests.items():
            if not path.exists():
                print(f"skip {path} (not present)")
                continue
            new_text = rewrite(path.read_text())
            leftover = stale_versions(new_text, version)
            if leftover:
                raise ValueError(f"{path}: still references version(s) {sorted(leftover)} after bump")
            planned.append((path, new_text))
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    # Second pass: write only the manifests that actually changed.
    changed: list[str] = []
    for path, new_text in planned:
        if new_text == path.read_text():
            print(f"unchanged {path}")
            continue
        path.write_text(new_text)
        changed.append(str(path))
        print(f"bumped {path}")

    print(f"changed={','.join(changed)}" if changed else "changed=")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
