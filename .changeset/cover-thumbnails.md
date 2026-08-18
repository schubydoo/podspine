---
default: minor
---

Serve a small cover thumbnail to the browse-UI grid instead of full-resolution artwork: `/cover/{id}/thumb` generates a `cover_thumb.jpg` (long edge ≤400px) on first request, caches it in the data dir, and regenerates it when the cover changes — so existing books get thumbnails on first view with no re-ingest. The RSS feed and `/cover` keep the full-size image for podcatchers
