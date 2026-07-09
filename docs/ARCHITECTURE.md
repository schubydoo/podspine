# Architecture

How Podspine turns a folder of audiobooks into per-chapter podcast feeds, and the
invariants that keep those feeds correct and the server safe.

## Overview

Podspine is a single Rust binary built as a Cargo workspace — one crate per
pipeline stage. It shells out to `ffmpeg`/`ffprobe` as separate processes (a GPL
boundary; always invoked with an argument vector, never a shell string) and keeps
its own state in a SQLite index plus a flat directory of extracted chapter files
and covers (whole-file episodes are served in place from the library, not copied).

At startup it resolves configuration, scans the library (splitting chaptered books
into per-chapter files, serving whole-file books in place, and recording both in
the index), then serves feeds, audio, and a
small browse UI over HTTP. A background watcher keeps the index reconciled with the
library while it runs, so added, replaced, or removed books are picked up without a
restart.

```mermaid
flowchart LR
  config --> scanner --> prober --> chapters --> splitter --> index[(index)]
  index --> feed --> http --> ui
  index --> http
```

The crates are described below; the pipeline runs left to right, with the SQLite
`index` feeding both the `feed` builder and the `http` server.

## Crates

| Crate | Responsibility |
|---|---|
| `config` | Resolve settings from CLI flags → env → TOML (in that precedence); preflight `ffmpeg`/`ffprobe` so a missing toolchain fails at startup, not mid-request. |
| `scanner` | Walk the library, classify each book (single audio file, per-book subfolder, or multi-track MP3 folder), and orchestrate probe → chapters → split → cover → index. Assigns collision-free slugs; one bad book never aborts the scan. Also hosts the background watcher that debounces filesystem changes and re-reconciles the index while the server runs. |
| `prober` | Thin `ffprobe` wrapper → `ProbedBook` (duration, audio codec, cover presence/codec, track/title tags, embedded chapters). Parsing is separated from the subprocess call so it's unit-testable. |
| `chapters` | Resolve the chapter source: a sibling `.cue` (75 fps `INDEX 01`) or `.ffmeta` sidecar wins over embedded markers (priority `.cue` > `.ffmeta` > embedded). `.opf`/`.nfo`/`.odm` are never chapter sources. |
| `splitter` | `ffmpeg` wrapper: stream-copy each chapter into a codec-matching container (no re-encode). Bounds concurrency with a semaphore and enforces a per-child timeout/kill. Also extracts cover art. |
| `index` | `rusqlite` (bundled SQLite) store for `book`, `episode`, and `feed_token` rows, with idempotent upserts keyed on stable ids. |
| `feed` | Build one RSS 2.0 channel (itunes + podcast namespaces) per book, and a self-check that refuses to serve a malformed feed. |
| `http` | Axum router: UI, feed, cover, and Range audio routes, plus the security/DoS middleware. |
| `ui` | `maud` server-rendered pages: book grid, per-book copy-URL + QR + regenerate, and the per-book **subscribe page** (one-tap "Open in…" deep links + per-app QRs). Pure presentation — no DB or HTTP dependency. |

Plus the `podspine` server binary (`src/main.rs`, wiring config → scan → watch →
serve) and a `podspine-cli` proof-of-concept for the single-file split pipeline.

## Ingest data flow

Per book, the scanner runs:

```mermaid
flowchart TD
  A[audio file or folder] --> B{DRM?<br/>.aax/.aaxc/.aa/.odm}
  B -- yes --> Z[skip + log notice]
  B -- no --> C[prober: ffprobe]
  C --> D{sidecar?<br/>.cue / .ffmeta}
  D -- yes --> E[chapters from sidecar]
  D -- no --> F[embedded chapters, or single episode]
  E --> G[splitter: stream-copy each chapter]
  F --> G
  G --> H[extract cover, if any]
  H --> I[(index: book + episodes)]
```

- **Chaptered single-file books** (`.m4b`/`.m4a`, `.mp3`, `.ogg`/`.opus`/`.flac`
  with chapters) are split by chapter via stream copy into a container matching
  the source codec (`m4a`/`mp3`/`flac`/`ogg`/`opus`) — see the storage model below
  for `full` vs `saver`.
- **MP3 folders** (per-chapter tracks) are treated as one episode per file, ordered
  by track number (falling back to filename order) and **served in place** from the
  library — no copy, no re-split, no re-encode.
- A book with no chapters and no sidecar degrades to a single-episode feed with a
  warning; that whole file is also **served in place** (Sprint 6.2).

## Storage model

SQLite index + flat filesystem. The unit is the **episode**, and how it is stored
depends on whether it is a whole source file or a sub-range of a container:

- **Whole-file episode** — a folder-of-MP3 track, or a chapterless single file —
  is **served in place** from the read-only library (Sprint 6.2). Its
  `episode.source_path` records the library file; nothing is copied under
  `<data_dir>`, and the audio handler streams it (with Range) after asserting the
  path stays under the canonical library root.
- **Chaptered episode** — a sub-range of a container — must be **extracted** under
  `<data_dir>` (a raw byte range of an `.m4b` isn't a standalone file). In `full`
  mode (default) every chapter is pre-split at ingest and kept; in `saver` mode
  each is split once at ingest to record its exact byte length, then deleted and
  regenerated on demand into a bounded cache — so `saver` cuts steady-state disk,
  not ingest time or I/O.

So `<data_dir>` grows on top of the originals only for chaptered books; whole-file
books cost only their index row and any extracted cover. See
[DEPLOYMENT.md](DEPLOYMENT.md#storage-mode-full-vs-saver) for the disk-budget
details.

```
<data_dir>/
├── podspine.db              # SQLite index (book, episode, feed_token)
└── books/
    └── <slug>/
        ├── 001.m4a          # per-chapter episode files (NNN.<ext>)
        ├── 002.m4a
        ├── ...
        └── cover.jpg        # extracted cover, if present
```

Everything the HTTP layer serves lives under `<data_dir>` — a single trusted root
that the path-safety check enforces. `book` and `episode` rows carry the on-disk
`file_path` written at scan time; the server never builds a path from request input.

## HTTP surface

Routes split into two surfaces. The **browse UI** is keyed by the human `slug` and
enumerates the library, so it's meant for the LAN / behind proxy-auth. The
**capability surface** is keyed by a random, unguessable per-book `feed_id` and is
safe to expose externally (a guessed id 404s); see
[DEPLOYMENT.md](DEPLOYMENT.md#exposing-podspine-safely).

| Route | Surface | Purpose |
|---|---|---|
| `GET /` | UI (slug) | Browsable book grid. |
| `GET /book/{slug}` | UI (slug) | Per-book page: copy capability-feed-URL, QR code (to the subscribe page), per-app how-to, **Regenerate link**. |
| `POST /book/{slug}/regenerate` | UI (slug) | Rotate the book's `feed_id` (leak recovery); same-origin/CSRF-guarded. |
| `GET /subscribe/{feed_id}` | capability | "Add to a podcast app" helper page: one-tap "Open in…" deep links + per-app QRs. |
| `GET /feed/{feed_id}.xml` | capability | The podcast feed (built from the index, passed through the self-check); always `itunes:block` + `X-Robots-Tag: noindex`. |
| `GET /audio/{feed_id}/{n}` | capability | Episode audio with HTTP Range (206 / `Content-Range` / 416) via `axum-range`. |
| `GET /cover/{feed_id}` | capability | Book cover image. |
| `GET /healthz` | — | Liveness. |

## Invariants

These are the rules the whole design exists to protect — the reasons the crates are
split the way they are.

**Feed correctness (the bug that killed predecessors):**
- `pubDate`s are strictly sequential with **oldest = chapter 1**, so episodes play
  in order.
- Every item carries `itunes:episode`, `itunes:duration` (`HH:MM:SS`), and an
  `enclosure length` read from the **real output file** (never prorated from a
  bitrate).
- `guid = blake3(book.id : idx : source_mtime)` — stable across re-scans of an
  unchanged source, and changes only when the source changes.
- A generated feed is rejected by the self-check before it can be served if any of
  the above is violated.

**Audio fidelity:**
- Never re-encoded: chapters are extracted by stream copy and whole files are served
  untouched, so there's no quality loss.

**Security (see [SECURITY.md](https://github.com/schubydoo/podspine/blob/main/SECURITY.md) for the threat model):**
- Book/episode ids are **opaque index keys**. Slugs are validated against an
  allow-list charset and rejected with 404; the resolved audio path is canonicalized
  and asserted to stay under `<data_dir>`. A path is never built from user input.
- `ffmpeg`/`ffprobe` are invoked with an **argv vector, never a shell string** —
  chapter titles and filenames are untrusted.
- Bounded `ffmpeg` concurrency (a semaphore sized to the CPU count) with a per-child
  timeout and kill; the HTTP layer adds concurrency, timeout, and body-size limits.
- **DRM-free input only.** DRM-protected files are skipped with a logged notice;
  Podspine ships no circumvention, and `ffmpeg` stays out-of-process (GPL boundary).

## See also

- [DEVELOPMENT.md](DEVELOPMENT.md) — building, testing, and the crate layout in practice.
- [DEPLOYMENT.md](DEPLOYMENT.md) — running it in production (Docker, reverse proxy, systemd).
- [CONTRIBUTING.md](https://github.com/schubydoo/podspine/blob/main/CONTRIBUTING.md) — contribution workflow.
