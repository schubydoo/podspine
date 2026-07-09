---
default: minor
---

Add an opt-in `saver` storage mode (`PODSPINE_STORAGE_MODE=saver`) that keeps chapters in a bounded on-demand cache (`PODSPINE_CACHE_SIZE`/`PODSPINE_CACHE_TTL`) instead of keeping every chapter split on disk — cutting the data-dir footprint for chaptered books (folder-of-MP3 books are still copied in full) for a small first-play delay per chapter. Ingest still splits each chapter once to record its real byte length, so `saver` saves disk, not ingest time.
