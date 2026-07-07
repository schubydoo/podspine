# Changelog

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
