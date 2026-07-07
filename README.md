# Podspine

**Turn your audiobook files into per-chapter podcast feeds any podcast app can play.**

Podspine is a small self-hosted server. Point it at a folder of audiobooks and it
gives each book its own podcast RSS feed — one episode per chapter, in the right
order — that you subscribe to in Apple Podcasts, Pocket Casts, Overcast,
AntennaPod, or anything else that reads RSS. No accounts, no separate app, no
built-in player. Just a feed URL.

- **Chapters as episodes, in order.** The #1 bug in naive attempts is episodes
  playing out of order; Podspine emits sequential `pubDate`s (oldest = chapter 1)
  and `itunes:episode` numbers, and refuses to serve a feed that fails its own
  self-check.
- **Zero-config.** `docker run` with your library mounted just works.
- **Copy-first, no quality loss.** Chapters are split by stream copy (no
  re-encode) at ingest.
- **Your files stay yours.** DRM-free input only — Podspine ships no DRM
  circumvention.

> Status: released and feature-complete — library scan with live auto-refresh, web
> UI, cover art, MP3 folders, Tier-2 formats, chapter sidecars, private
> capability-URL feeds (with one-click regenerate), a one-tap **subscribe page**
> with per-app deep links + QR, and security hardening. See [CHANGELOG](CHANGELOG.md).

## Quick start

### Docker (recommended)

```bash
docker run \
  -v /path/to/audiobooks:/library:ro \
  -v podspine-data:/data \
  -p 8080:8080 \
  -e PODSPINE_BASE_URL=http://<your-lan-ip>:8080 \
  ghcr.io/schubydoo/podspine:latest
```

Then open <http://localhost:8080> to browse your books and copy feed URLs.

> **Set `PODSPINE_BASE_URL`** to the address podcast apps will actually reach
> (your LAN IP or public hostname). It defaults to `http://localhost:8080`, which
> only works from the same machine — feed and audio URLs are built from it.

`ffmpeg`/`ffprobe` are bundled in the image. The image runs as a non-root user;
`/data` holds the SQLite index and the split episode files, so keep it on a
persistent volume.

### Install the binary

Linux / macOS (downloads the signed binary for your OS/arch, verifies its
checksum, installs to `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/schubydoo/podspine/main/install.ps1 | iex
```

Or a package manager — `brew install schubydoo/podspine/podspine`, `scoop install
podspine`, `yay -S podspine-bin`, `nix profile install github:schubydoo/podspine`,
or `cargo binstall --git https://github.com/schubydoo/podspine podspine`.
`ffmpeg`/`ffprobe` must be on your `PATH`. Full matrix,
version pinning, uninstall, and signature verification: **[Installing](https://schubydoo.github.io/podspine/latest/installation/)**.

```bash
podspine --library /path/to/audiobooks --base-url http://<your-lan-ip>:8080
# → http://localhost:8080
```

### From source

```bash
cargo run -- --library ./sample-books
```

## Configuration

The library path is the only required input; everything else has a default and can
be set via CLI flag, environment variable, or a TOML file (`--config`), in that
precedence. See the **[full option reference](docs/DEPLOYMENT.md#configuration)**.

Each book's feed lives at an unguessable **capability URL** — `/feed/{feed_id}.xml`,
with `/audio/{feed_id}/{n}` (episode audio, HTTP Range) and `/cover/{feed_id}`. The
browse UI (`/`, `/book/{slug}`) enumerates your library, so keep it on the LAN or
behind proxy-auth while the capability routes are safe to expose — see
**[exposing Podspine safely](docs/DEPLOYMENT.md#exposing-podspine-safely)**.

## Supported formats

Point Podspine at a folder; each audiobook becomes its own feed. A book can be a
single file or a per-book subfolder.

| Tier | Formats | Chapter source |
|---|---|---|
| **1** | M4B / M4A (AAC/ALAC), single-file MP3, **folder of per-chapter MP3s** | embedded chapters / file (track) order |
| **2** | OGG Vorbis, Opus, FLAC | embedded chapters, or a `.cue` sidecar (FLAC needs one) |

**Chapter sidecars.** A companion file beside the audio is preferred over
embedded chapters, in priority order: **`.cue`** (`INDEX 01`, 75 frames/sec) →
**`.ffmeta`** → embedded. `.opf` / `.nfo` / `.odm` are never treated as chapter
sources. Use `--force-embedded-chapters` to ignore sidecars.

**DRM.** DRM-protected files — Audible `.aax`/`.aaxc`/`.aa`, OverDrive `.odm` —
are **skipped** with a logged notice. Podspine ships no DRM circumvention. If you
own such files, convert them to a DRM-free format (M4B/MP3/FLAC/…) with your own
tools first, then drop the result in your library.

## Adding a feed to your podcast app

Open the Podspine UI and click a book. Copy its feed URL, or scan the QR code with
your phone to open its **subscribe page** — a set of one-tap "Open in…" deep links
for Apple Podcasts, Overcast, Pocket Casts, Castro, AntennaPod, and Podcast Addict.
For per-app steps and troubleshooting, see **[docs/importing.md](docs/importing.md)**.

## Documentation

📖 **Full docs: <https://schubydoo.github.io/podspine/>** (searchable, versioned). The sources also render on GitHub:

- **[Deploying](docs/DEPLOYMENT.md)** — Docker/compose, reverse proxy, systemd, backups, and the full config reference.
- **[Adding to your podcast app](docs/importing.md)** — per-app import steps and troubleshooting.
- **[Architecture](docs/ARCHITECTURE.md)** — how the pipeline and feeds work, and the invariants behind them.
- **[Development](docs/DEVELOPMENT.md)** — local setup, crate layout, testing, and release builds.
- **[Contributing](CONTRIBUTING.md)** · **[Security](SECURITY.md)** · **[Changelog](CHANGELOG.md)**

## Development

Rust workspace, one crate per pipeline stage; requires `ffmpeg`/`ffprobe` on `PATH`.
See **[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)** for setup, the crate layout, and
testing, and **[CONTRIBUTING.md](CONTRIBUTING.md)** for the workflow.

## License

[AGPL-3.0-only](LICENSE). Podspine shells out to `ffmpeg`/`ffprobe` as separate
processes (no linking) and ships no DRM circumvention.
