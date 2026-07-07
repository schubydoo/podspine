# Downstream packaging repos

Homebrew and Scoop are consumed from **separate repos** (tap / bucket). The canonical
manifests live here in the main repo and are regenerated on every release by
[`packaging-bump.yml`](../../.github/workflows/packaging-bump.yml); the two workflow
files in this directory mirror them into those repos.

## How the release automation flows

1. A release publishes → `packaging-bump.yml` runs `scripts/bump_packaging.py`, which
   rewrites the version + checksums in `Formula/podspine.rb`, `packaging/scoop/podspine.json`,
   and the `flake.nix` version from the release `checksums.txt`, and opens a
   `packaging-bump/vX.Y.Z` PR (auto-merge enabled).
2. That PR merges → the main repo's `tap-sync-trigger.yml` dispatches the sync workflows
   in the tap and bucket.
3. The tap / bucket sync workflows (below) pull the updated manifest from `main` and
   commit it into their repo. A daily cron is the fallback if a dispatch is missed.

## One-time setup

**`schubydoo/homebrew-podspine`** (tap → `brew tap schubydoo/podspine`):
- `Formula/podspine.rb` — seed with a copy of the main repo's `Formula/podspine.rb`.
- `.github/workflows/sync-formula.yml` — copy from `sync-formula.yml` here.

**`schubydoo/scoop-podspine`** (bucket):
- `bucket/podspine.json` — seed with the main repo's `packaging/scoop/podspine.json`.
- `.github/workflows/sync-manifest.yml` — copy from `sync-manifest.yml` here.

**podspine-ci App** must be installed on `homebrew-podspine` and `scoop-podspine` with
**Actions: write** (so `tap-sync-trigger.yml` can dispatch their syncs).

> **AUR** (`podspine-bin`) is intentionally not included here — it lives on the
> `feat/distribution-aur` branch until AUR account registration reopens.
