# Benchmarks

How Podspine measures itself against the v2 performance targets, and how to
reproduce the numbers on your own hardware.

This is the measurement half of the Sprint 5 performance-validation work (against
NFR-P1..P4 in the PRD).
The point is not to publish a leaderboard — it is to answer one question before
any v2 efficiency work (on-the-fly splitting, transcoding) is built: **are the
NFR targets already met, or is there a real bottleneck to fix?** Run the harness
on the box you actually deploy to and let the numbers decide.

## Targets (PRD §5.1, NFR-P1..P4)

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

Illustrative only — captured in a Linux x86_64 CI-class sandbox, loopback, with a
synthetic 300s / 8-chapter book. **Your hardware will differ**; re-run locally.

| NFR | Metric                   | Measured           | Target      | Result |
|-----|--------------------------|--------------------|-------------|--------|
| P1  | Ingest (this book, 300s) | 0.91 s             | —           | —      |
| P1  | Ingest → 10h (extrap.)   | ~109 s             | ≤ 120 s     | PASS   |
| P2  | Feed p50/p95/p99         | 2.0 / 2.2 / 2.4 ms | p95 < 200ms | PASS   |
| P3  | Audio TTFB p50/p95/p99   | 2.4 / 2.7 / 2.8 ms | p95 < 300ms | PASS   |
| P4  | Idle RSS                 | 8.3 MB             | < 50 MB     | PASS   |

### Reading the reference run

All four targets clear with wide margins — feed render and audio TTFB sit ~100×
under budget, and idle memory is ~6× under. The only figure worth watching is
**P1**: pre-split ingest is I/O-bound (`ffmpeg -c copy` per chapter), so on slow
storage or a many-chapter book it will rise. Because the extrapolation folds in
fixed startup cost it is pessimistic, but if a real 10h book on your disk lands
near the 2-minute ceiling, that is the signal that on-the-fly byte-range
splitting (serving chapters from computed offsets, no duplicate split files) is
worth building — otherwise it is premature.

The optional `/metrics` endpoint (Prometheus counters/histograms) is intentionally
not part of this harness; it adds a runtime dependency and an opt-in config flag,
which is a separate change.
