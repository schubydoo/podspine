---
default: minor
---

Detect non-faststart whole-file mp4 (`moov` after `mdat`) at ingest and log a one-line callout; add opt-in `PODSPINE_REMUX_NON_FASTSTART` to remux such books to faststart on demand — a cache-managed stream-copy (no re-encode) served from the `saver` cache and regenerated/evicted like a cached chapter, never a pinned duplicate — so podcast clients seek quickly. MP3/OGG/FLAC, already-faststart mp4, and chaptered books are unaffected.
