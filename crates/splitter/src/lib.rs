//! `splitter` — `ffmpeg` wrapper. Per chapter:
//! `-ss <start> -i <in> -t <end-start> -map 0:a:0 -map_chapters -1 -c copy
//! -movflags +faststart <out>.m4a`. Args passed as an argv vector, never a shell
//! string (untrusted chapter titles). Computes `byte_length` from the actual
//! output file (`fs::metadata().len()`), never prorated. Guarded by a semaphore +
//! per-child timeout/kill. See TAD §4/§5.4. Implemented in Sprint 1 (Task 1.3).
