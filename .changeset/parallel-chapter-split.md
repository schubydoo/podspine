---
default: perf
---

Split a book's chapters in parallel instead of one at a time, bounded by the existing CPU-sized ffmpeg gate, so a first scan of a chaptered library is much faster (measured ~9× on a 20-core host for a 40-chapter book) — episode order and per-chapter enclosure sizes are unchanged
