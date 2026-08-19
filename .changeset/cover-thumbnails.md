---
default: minor
---

Serve a small cover thumbnail to the browse-UI grid instead of full-resolution artwork: the scanner generates a `cover_thumb.jpg` (long edge ≤400px) alongside each cover, and `/cover/{id}/thumb` serves it (falling back to the full cover when it's missing). Existing libraries are backfilled by the reconcile on the next scan — no re-ingest needed. The RSS feed and `/cover` keep the full-size image for podcatchers
