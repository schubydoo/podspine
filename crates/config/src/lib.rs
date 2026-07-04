//! `config` — `clap`/env/TOML resolution; validates the library root. Includes a
//! startup preflight that execs `ffmpeg -version` / `ffprobe -version` and fails
//! fast with a clear message if absent (so failure is at startup, not
//! mid-request). Library path is the only required input. See TAD §4.
//! Implemented in Sprint 2 (Task 2.2).
