# Benchmarks

How Podspine measures itself against the v2 performance targets, and how to
reproduce the numbers on your own hardware.

This is the measurement half of the Sprint 5 performance-validation work — it
checks Podspine against the four performance targets in the [table below](#targets)
(ingest time, feed render p95, audio time-to-first-byte, and idle memory).
The point is not to publish a leaderboard — it is to answer one question before
any further efficiency work is built: **are the NFR targets already met, or is
there a real bottleneck to fix?** (Opt-in transcoding — `PODSPINE_TRANSCODE` —
and `saver` mode's regenerate-on-demand chapter cache have since shipped; what
remains hypothetical is byte-range chapter serving with no split files at all.)
Run the harness on the box you actually deploy to and let the numbers decide.

## Targets

| NFR    | Metric                          | Target                  |
|--------|---------------------------------|-------------------------|
| P1     | Ingest / pre-split              | ≤ 2 min per 10h book    |
| P2     | Feed render latency (p95)       | < 200 ms                |
| P3     | Audio time-to-first-byte (LAN)  | < 300 ms                |
| P4     | Idle resident memory (RSS)      | < 50 MB                 |

## Running the harness

```sh
scripts/bench.sh
```

It needs only `bash`, `ffmpeg`, `ffprobe`, `curl`, `awk`, and `sort` — no extra
crates, and it touches nothing the server ships. It builds the release binary
(if absent), synthesizes a chapterised sine-tone `.m4a`, boots `podspine` against
a throwaway library and data dir on `127.0.0.1`, drives it with `curl`, prints a
report, and tears everything down.

Knobs (all optional env vars):

| Var            | Default | Meaning                                   |
|----------------|---------|-------------------------------------------|
| `DURATION_SEC` | `600`   | Synthetic book length in seconds          |
| `CHAPTERS`     | `12`    | Number of chapters to split into          |
| `N_FEED`       | `200`   | Feed requests sampled for the p95          |
| `N_AUDIO`      | `30`    | Audio requests sampled for TTFB           |
| `PORT`         | `18080` | Loopback port to bind                     |
| `KEEP`         | unset   | Keep the temp work dir for inspection     |

```sh
# A heavier run closer to a real book:
DURATION_SEC=3600 CHAPTERS=40 N_FEED=500 scripts/bench.sh
```

## How each number is measured

- **Ingest (P1)** — wall-clock from launching the process to the book appearing
  on the home grid (scanned, split, and indexed). This **includes fixed startup
  overhead** (process init + bind), so the extrapolation to a 10h book is
  *conservative*: startup does not scale with book length, but the linear
  extrapolation pretends it does. Treat the 10h figure as an upper bound.
- **Feed render p95 (P2)** — `curl` `time_total` over `N_FEED` requests to
  `/feed/{id}.xml`, after one warm-up (the feed passes the self-check and renders
  fresh each time). Percentiles via nearest-rank.
- **Audio TTFB (P3)** — `curl` `time_starttransfer` over `N_AUDIO` ranged
  (`Range: bytes=0-65535`) requests to `/audio/{id}/1`. This is **loopback**, so
  a real LAN client adds one network hop; budget accordingly against the 300 ms
  target.
- **Idle RSS (P4)** — `VmRSS` from `/proc/<pid>/status` after the run (Linux
  only; reported as `n/a` elsewhere).

## Reference run

Illustrative only — captured 2026-08-19 on a Linux x86_64 host, loopback, with a
synthetic 300s / 8-chapter book, after the parallel chapter split (v1.7.0).
**Your hardware will differ**; re-run locally.

| NFR | Metric                   | Measured           | Target      | Result |
|-----|--------------------------|--------------------|-------------|--------|
| P1  | Ingest (this book, 300s) | 0.24 s             | —           | —      |
| P1  | Ingest → 10h (extrap.)   | ~29 s              | ≤ 120 s     | PASS   |
| P2  | Feed p50/p95/p99         | 1.3 / 2.3 / 2.5 ms | p95 < 200ms | PASS   |
| P3  | Audio TTFB p50/p95/p99   | 0.7 / 0.8 / 0.9 ms | p95 < 300ms | PASS   |
| P4  | Idle RSS                 | 9.5 MB             | < 50 MB     | PASS   |

### Reading the reference run

All four targets clear with wide margins — feed render and audio TTFB sit ~100×
under budget, and idle memory is ~5× under. **P1** used to be the figure to
watch, and it improved substantially in v1.7.0: chapters are now split in
parallel, bounded by a CPU-sized ffmpeg gate (measured ~9× on a 20-core host for
a 40-chapter book), so many-chapter books scale far better than a linear
per-chapter model suggests. Pre-split ingest remains I/O-bound on slow storage.
This applies only to **chaptered** books — whole-file episodes (MP3-folder
tracks, chapterless singles) are served in place from the library and skip the
split entirely (Sprint 6.2), and `saver` mode trades the persistent split set
for a regenerate-on-demand cache. Because the extrapolation folds in fixed
startup cost it is pessimistic, but if a real 10h chaptered book on your disk
still lands near the 2-minute ceiling, that is the signal that on-the-fly
byte-range chapter serving (no split files at all) is worth the complexity —
otherwise premature.

The optional `/metrics` endpoint (Prometheus counters/histograms, enabled with
`--metrics-bind` on its own listener) is intentionally not part of this harness —
the harness measures the serving path, not the exporter.
