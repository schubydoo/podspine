# Deployment

Running Podspine in a homelab. For a first run, the [README quick start](../README.md#quick-start)
is enough; this page covers the production details: persistence, exposing it safely,
and running it as a service.

> **Podspine has no authentication.** It's designed as trusted, host-local
> infrastructure. Bind it to loopback, or put a reverse proxy with auth in front of
> it before exposing it to a network — anyone who can reach the port can read your
> feeds and audio. See [SECURITY.md](../SECURITY.md).

## Configuration

Every option is a CLI flag, an environment variable, or a TOML key (`--config`), in
that precedence. The library path is the only required input.

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--library` | `PODSPINE_LIBRARY` | — (required) | Folder of audiobooks to scan. |
| `--data-dir` | `PODSPINE_DATA_DIR` | `./data` | SQLite index + split episode files. |
| `--bind` | `PODSPINE_BIND` | `0.0.0.0:8080` | Address to listen on. |
| `--base-url` | `PODSPINE_BASE_URL` | `http://localhost:<port>` | External URL used to build feed/audio links. |
| `--default-cover-url` | `PODSPINE_DEFAULT_COVER_URL` | none | Feed-level fallback cover for books with no embedded art. |
| `--force-embedded-chapters` | `PODSPINE_FORCE_EMBEDDED_CHAPTERS` | off | Ignore `.cue`/`.ffmeta` sidecars. |
| `--config` | `PODSPINE_CONFIG` | none | Path to a TOML config file. |

> **`PODSPINE_BASE_URL` is the one that bites people.** Feed and enclosure (audio)
> URLs are built from it. If it's left at `localhost`, a podcatcher on another device
> can't fetch anything. Set it to the LAN IP / hostname (and scheme + port, or the
> public URL if behind a proxy) that clients actually reach.

## Docker

The image bundles `ffmpeg`, runs as a non-root user (uid 10001), and defaults to
`PODSPINE_LIBRARY=/library` and `PODSPINE_DATA_DIR=/data`.

```bash
docker run -d --name podspine \
  -v /path/to/audiobooks:/library:ro \
  -v podspine-data:/data \
  -p 8080:8080 \
  -e PODSPINE_BASE_URL=http://<your-lan-ip>:8080 \
  ghcr.io/schubydoo/podspine:latest
```

- Mount the library **read-only** (`:ro`) — Podspine only reads it.
- Keep `/data` on a **named volume** (or a host path): it holds the SQLite index and
  the split episode files, and should persist across restarts and upgrades.

### docker-compose

```yaml
services:
  podspine:
    image: ghcr.io/schubydoo/podspine:latest
    restart: unless-stopped
    ports:
      - "8080:8080"
    environment:
      PODSPINE_BASE_URL: http://your-lan-ip:8080
    volumes:
      - /path/to/audiobooks:/library:ro
      - podspine-data:/data
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://127.0.0.1:8080/healthz"]
      interval: 30s
      timeout: 5s
      retries: 3

volumes:
  podspine-data:
```

## Prebuilt binary + systemd

Download the static musl binary for your architecture from the
[releases](https://github.com/schubydoo/podspine/releases) (verify it against its
`.sha256`), and make sure `ffmpeg`/`ffprobe` are installed. Example unit:

```ini
# /etc/systemd/system/podspine.service
[Unit]
Description=Podspine
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/podspine
Environment=PODSPINE_LIBRARY=/srv/audiobooks
Environment=PODSPINE_DATA_DIR=/var/lib/podspine
Environment=PODSPINE_BIND=127.0.0.1:8080
Environment=PODSPINE_BASE_URL=https://podspine.example.com
DynamicUser=yes
StateDirectory=podspine
ReadOnlyPaths=/srv/audiobooks
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now podspine
```

## Reverse proxy

Front Podspine with a proxy to add TLS and (recommended) auth. The one hard
requirement: **pass through `Range` and `Accept-Ranges`** — podcast apps use HTTP
Range to seek/scrub, and stripping it breaks playback. Also set `PODSPINE_BASE_URL`
to the public URL so generated links match.

**Caddy** (Range passes through automatically):

```caddy
podspine.example.com {
    reverse_proxy 127.0.0.1:8080
    # basic auth (optional): basicauth { user <bcrypt-hash> }
}
```

**nginx:**

```nginx
server {
    server_name podspine.example.com;
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header Range $http_range;               # forward seek requests
        proxy_set_header If-Range $http_if_range;
        proxy_buffering off;                              # stream large audio
        # auth_basic "Podspine"; auth_basic_user_file /etc/nginx/.htpasswd;
    }
}
```

## Health checks

`GET /healthz` returns `200 ok`. Use it for container/orchestrator liveness (see the
compose example above) or an uptime monitor.

## Data, backups, and updating

- **Back up `<data_dir>`** — `podspine.db` plus `books/`. The DB is the source of
  truth for slugs, episode order, and stable guids; losing it means feeds get new
  guids on the next scan (subscribers may re-download). The `books/` tree can be
  regenerated by re-scanning the library, but backing it up avoids the re-split.
- **Your source library** is never modified — Podspine only reads it.
- **Updating:** pull the new image (or drop in the new binary) and restart. The
  library is re-scanned on startup; unchanged books are idempotent (no re-split), so
  restarts are cheap.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — how it works internally.
- [importing.md](importing.md) — adding a feed to podcast apps + troubleshooting.
- [../SECURITY.md](../SECURITY.md) — threat model and vulnerability reporting.
