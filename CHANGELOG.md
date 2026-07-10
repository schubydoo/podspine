# Changelog

## 1.3.0 (2026-07-10)

### Features

- Detect non-faststart whole-file mp4 (`moov` after `mdat`) at ingest and log a one-line callout; add opt-in `PODSPINE_REMUX_NON_FASTSTART` to remux such books to faststart on demand — a cache-managed stream-copy (no re-encode) served from the `saver` cache and regenerated/evicted like a cached chapter, never a pinned duplicate — so podcast clients seek quickly. MP3/OGG/FLAC, already-faststart mp4, and chaptered books are unaffected. ([#61](https://github.com/schubydoo/podspine/pull/61))
- Add per-book `.podspine.toml` overrides: a sidecar beside a single-file book (`Author - Title.podspine.toml`) or inside a folder book overrides settings for just that book — `storage_mode`, `force_embedded_chapters`, `remux_non_faststart`, `default_cover_url`, plus troubleshooting knobs `disabled`, `title`, `author`, and `force_reingest` — with precedence sidecar → CLI/env → global config → default. Server-wide keys placed in a per-book file are ignored with a warning. ([#62](https://github.com/schubydoo/podspine/pull/62))
- Serve whole-file episodes — folder-of-MP3 tracks and chapterless single files — in place, streaming them directly from the read-only library instead of copying them under the data dir; this removes the silent duplication those books used to cost (an existing library reclaims the copies on its next re-scan). Chaptered books are unchanged (`full`/`saver`). ([#59](https://github.com/schubydoo/podspine/pull/59))
- Add an opt-in `saver` storage mode (`PODSPINE_STORAGE_MODE=saver`) that keeps chapters in a bounded on-demand cache (`PODSPINE_CACHE_SIZE`/`PODSPINE_CACHE_TTL`) instead of keeping every chapter split on disk — cutting the data-dir footprint for chaptered books (whole-file books such as folder-of-MP3 tracks stream in place regardless of storage mode) for a small first-play delay per chapter. Ingest still splits each chapter once to record its real byte length, so `saver` saves disk, not ingest time. ([#53](https://github.com/schubydoo/podspine/pull/53))

## 1.2.0 (2026-07-07)

### Features

- Add a `--version`/`-V` flag so `podspine --version` reports the version (used by the install scripts and package-manager smoke tests). ([#36](https://github.com/schubydoo/podspine/pull/36))

## 1.1.0 (2026-07-05)

### Features

- Add an "Add to a podcast app" subscribe page: the book-page QR now opens `/subscribe/{feed_id}` with one-tap "Open in…" deep links for Apple Podcasts, Overcast, Pocket Casts, Castro, AntennaPod, and Podcast Addict (per-app QRs behind an expander) instead of raw feed XML the iOS Camera couldn't open. ([#22](https://github.com/schubydoo/podspine/pull/22))

## 1.0.1 (2026-07-05)

### Fixes

- Set the audio `Content-Type` on `/audio` responses so Apple Podcasts and other iOS clients can play episodes (axum-range sets none, which made playback fail with "this episode can't be played on this device"). ([#20](https://github.com/schubydoo/podspine/pull/20))

## 1.0.0 (2026-07-05)

First tagged release: a zero-config, self-hosted server that turns a folder of
audiobooks into per-chapter podcast RSS feeds any podcast app can play.

### Added
- **Per-chapter podcast feeds** with correct ordering: sequential `pubDate`s
  (oldest = chapter 1), `itunes:episode`, `itunes:duration`, and `enclosure
  length` read from the real output file. A built-in self-check refuses to serve
  a malformed feed.
- **Copy-first chapter splitting** via `ffmpeg` stream copy (no re-encode).
- **Multi-book library scanning**: each top-level audio file or per-book subfolder
  becomes an independent feed, with collision-free slugs.
- **Web UI**: a browsable book grid with cover art, plus a per-book page with a
  copy-feed-URL control, a scannable QR code, and per-app "how to add this" help.
- **Cover art**: embedded covers extracted to `itunes:image`, with an optional
  feed-level fallback (`--default-cover-url`).
- **MP3-folder audiobooks**: a folder of per-chapter MP3s ingested as episodes,
  ordered by track number (falling back to filename order), no re-encode.
- **Tier-2 input formats**: Ogg Vorbis, Opus, and FLAC, stream-copied into a
  matching container.
- **Chapter sidecars**: a `.cue` (75 fps `INDEX 01`) or `.ffmeta` sidecar is
  preferred over embedded chapters; `--force-embedded-chapters` overrides.
- **HTTP Range** streaming for audio (seek/scrub), with correct MIME types.
- **Zero-config Docker image** (multi-arch amd64/arm64, non-root, ffmpeg bundled)
  and static musl binaries.
- **Configuration** via CLI flags, environment variables, or a TOML file, with an
  ffmpeg/ffprobe startup preflight.

### Security
- Opaque book/episode ids resolved server-side; slugs validated against an
  allow-list charset and rejected with 404 (path-traversal guard), with the
  resolved path canonicalized under the data dir as defense-in-depth.
- Bounded `ffmpeg` concurrency (semaphore) with a per-child timeout and kill;
  request concurrency, timeout, and body-size limits on the HTTP layer.
- Error responses never leak filesystem paths or `ffmpeg` stderr.
- **DRM-free input only.** DRM-protected files (`.aax`/`.aaxc`/`.aa`/`.odm`) are
  skipped with a logged notice; Podspine ships no DRM circumvention.
