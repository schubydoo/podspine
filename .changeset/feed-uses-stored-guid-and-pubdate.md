---
default: patch
---

Publish the guid and the pubDate that each episode recorded at ingest. The feed used to re-derive both from the book's source file mtime. A Refresh sets that mtime to a sentinel until the watcher re-ingests the book, so during that window the feed published a new guid for every episode and dates in 1969. A podcast app saw a whole new book, downloaded it, and downloaded it again when the real mtime returned. The feed now reads the stored per-episode guid and pubDate, and the channel date follows the newest episode. Chapter order and the guid formula are unchanged.
