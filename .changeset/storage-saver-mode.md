---
default: minor
---

Add an opt-in `saver` storage mode (`PODSPINE_STORAGE_MODE=saver`) that splits chaptered books on demand into a bounded cache (`PODSPINE_CACHE_SIZE`/`PODSPINE_CACHE_TTL`) instead of pre-splitting every chapter to disk — cutting the data-dir footprint for chaptered books (folder-of-MP3 books are still copied in full) for a small first-play delay per chapter.
