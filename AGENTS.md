# Podspine

Self-hosted server that turns audiobook files into per-chapter podcast RSS feeds any podcatcher can play. Rust/Axum, shells out to ffmpeg/ffprobe, AGPL-3.0. **Pre-code stage** — planning is done, no source scaffolded yet.

## Critical commands
Rust workspace:
- Build `cargo build` · Test `cargo test` · Lint `cargo clippy -- -D warnings` · Format `cargo fmt`
- Run locally: `cargo run -- --library ./sample-books` → http://localhost:8080
- Release binary: `cargo build --release --target x86_64-unknown-linux-musl`
- Container: `docker run -v /books:/library -p 8080:8080 podspine`
- `ffmpeg`/`ffprobe` must be on PATH (bundled in the Docker image).

## Architecture map
Cargo workspace, one crate per pipeline stage: `scanner` (watch/classify by format tier) → `prober` (ffprobe) / `chapters` (sidecar-vs-embedded resolution) → `splitter` (ffmpeg) → `index` (rusqlite, bundled) → `feed` (rss 2.1 + itunes/podcast ns) → `http` (axum; Range via axum-range) + `ui` (maud) + `config`. Pre-split at ingest (v1); flat filesystem for audio + SQLite index.

## Hard rules
- **YOU MUST** make feed pubDates sequential, oldest = chapter 1 (else episodes play out of order — the #1 bug). Always emit `itunes:episode`, `itunes:duration`, and `enclosure length`.
- **enclosure `length` = real output file size** (`fs::metadata().len()`), NEVER prorated from bitrate.
- ffmpeg split: `-ss <start>` BEFORE `-i`, `-t <duration>` (NEVER `-to` after `-i` — makes a 2× file), plus `-map 0:a:0 -map_chapters -1 -c copy -movflags +faststart`.
- **Pass ffmpeg args as an argv vector, NEVER a shell string** — chapter titles are untrusted (command injection).
- Treat book/chapter ids as opaque index keys; canonicalize and assert the path stays under the library root; 404 on reject. NEVER `format!("{root}/{user_input}")`.
- `guid = blake3(book.id : idx : source_mtime)` — keeps episodes stable across rescans.
- **NEVER add DRM circumvention** (Audible AAX/AAXC/.aa, OverDrive, WMA-DRM); DRM-free input only. Keep ffmpeg out-of-process (GPL boundary).
- FLAC: prefer a `.cue` sidecar (embedded FLAC chapters lack titles).
- License is AGPL-3.0 — add a `LICENSE` file when scaffolding.

## Workflow preferences
- Minimal diffs for small fixes; don't rewrite a file for a one-line change.
- Milestone-based, no deadlines. Prove the hardest piece first — correct M4B split + sequential pubDates (Sprint 1) — before server work.
- Default and PR target branch is `main`.

## What NOT to include here
Crate versions and design rationale live in the local design docs — don't duplicate. Don't restate standard Rust conventions (clippy/fmt enforce them).
