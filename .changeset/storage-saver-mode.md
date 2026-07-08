---
default: minor
---

Add an opt-in `saver` storage mode (`PODSPINE_STORAGE_MODE=saver`) that splits chapters on demand and caches them (`PODSPINE_CACHE_SIZE`/`PODSPINE_CACHE_TTL`) instead of pre-splitting every book to disk — roughly halving storage for a small first-play delay per chapter.
