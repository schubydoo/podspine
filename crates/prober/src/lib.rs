//! `prober` — thin `ffprobe` wrapper. Runs
//! `ffprobe -v error -print_format json -show_chapters
//! -show_entries format=duration:stream=duration`; parses `start_time`/`end_time`
//! (decimal strings, not raw `start`+`time_base`), format duration, and cover-art
//! stream presence into `ProbedBook { chapters, duration, has_cover }`.
//! See TAD §4. Implemented in Sprint 1 (Task 1.2).
