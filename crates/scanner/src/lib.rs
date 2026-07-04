//! `scanner` — watches/scans the library root; classifies each item by format
//! tier (M4B/M4A, single-file MP3, folder-of-files, Ogg/Opus/FLAC) and skips
//! DRM'd files (AAX/AAXC/`.aa`/`.odm`) with a logged notice; orchestrates
//! chapters -> splitter -> index. See TAD §4. Implemented in Sprint 2 (Task 2.3).
