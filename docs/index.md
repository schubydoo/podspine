<p align="center">
  <img src="assets/logo-mark.svg" alt="" width="96">
</p>

# Podspine

**Turn your audiobook files into per-chapter podcast feeds any podcast app can play.**

Podspine is a small self-hosted server. Point it at a folder of audiobooks and it
gives each book its own podcast RSS feed — one episode per chapter, in the right
order — that you subscribe to in Apple Podcasts, Pocket Casts, Overcast, AntennaPod,
or anything else that reads RSS. No accounts, no separate app, no built-in player.
Just a feed URL.

- **Chapters as episodes, in order.** The #1 bug in naive attempts is episodes
  playing out of order; Podspine emits sequential `pubDate`s (oldest = chapter 1)
  and `itunes:episode` numbers, and refuses to serve a feed that fails its own
  self-check.
- **Zero-config.** `docker run` with your library mounted just works.
- **Copy-first, no quality loss.** Chapters are split by stream copy (no re-encode)
  at ingest.
- **Private by default.** Each feed lives at an unguessable capability URL; the
  library is watched and feeds auto-refresh as you add books.
- **Your files stay yours.** DRM-free input only — Podspine ships no DRM
  circumvention.

## Quick start

Run the container with your library mounted:

```bash
docker run \
  -v /path/to/audiobooks:/library:ro \
  -v podspine-data:/data \
  -p 8080:8080 \
  -e PODSPINE_BASE_URL=http://<your-lan-ip>:8080 \
  ghcr.io/schubydoo/podspine:latest
```

Then open <http://localhost:8080> to browse your books, copy feed URLs, or scan a
book's QR code to open its one-tap [subscribe page](importing.md).

!!! tip "Set `PODSPINE_BASE_URL`"
    Point it at the address podcast apps will actually reach (your LAN IP or public
    hostname). It defaults to `http://localhost:8080`, which only works from the same
    machine — feed and audio URLs are built from it.

`ffmpeg`/`ffprobe` are bundled in the image. `/data` holds the SQLite index and the
split episode files, so keep it on a persistent volume. Prebuilt static binaries and
`cargo run` are covered in [Deploying](DEPLOYMENT.md).

## Where to next

<div class="grid cards" markdown>

- :material-rocket-launch: **[Deploying](DEPLOYMENT.md)** — Docker/compose, reverse
  proxy, systemd, backups, and the full config reference.
- :material-cellphone: **[Adding to your podcast app](importing.md)** — the subscribe
  page, per-app steps, and troubleshooting.
- :material-sitemap: **[Architecture](ARCHITECTURE.md)** — how the pipeline and feeds
  work, and the invariants behind them.
- :material-wrench: **[Development](DEVELOPMENT.md)** — local setup, crate layout,
  testing, and release builds.

</div>

## License

[AGPL-3.0-only](https://github.com/schubydoo/podspine/blob/main/LICENSE). Podspine
shells out to `ffmpeg`/`ffprobe` as separate processes (no linking) and ships no DRM
circumvention.
