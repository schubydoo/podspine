#!/usr/bin/env bash
# Podspine performance harness (Task 5.3 — measurement half).
#
# Measures the four v2 NFR targets (PRD §5.1, tad.md §5) against a synthetic
# book on THIS machine, so you can decide whether the v2 efficiency work
# (on-the-fly split 5.1, transcode 5.2) is actually warranted before building it:
#
#   NFR-P1  ingest/pre-split   <=2 min per 10h book
#   NFR-P2  feed render p95    <200 ms
#   NFR-P3  audio TTFB (LAN)   <300 ms
#   NFR-P4  idle RSS           <50 MB
#
# It is deliberately dependency-light — bash, ffmpeg, ffprobe, curl, awk, sort —
# and touches nothing the server ships: it builds the release binary, synthesizes
# a chapterised .m4a with ffmpeg, boots podspine against a throwaway library +
# data dir on the loopback, drives it with curl, then tears everything down.
#
# Numbers reflect the host it runs on (CPU, disk, filesystem). Run it on the box
# you actually deploy to; a CI runner or dev laptop is only a rough proxy. The
# ingest figure is extrapolated to a 10h book (stream-copy split time is ~linear
# in source duration) and clearly labelled as such.
#
# Usage:
#   scripts/bench.sh                 # defaults: 600s synthetic book, 12 chapters
#   DURATION_SEC=3600 CHAPTERS=40 scripts/bench.sh
#   N_FEED=500 N_AUDIO=50 scripts/bench.sh
#   KEEP=1 scripts/bench.sh          # keep the temp dir + printed paths
#
# Env knobs (all optional):
#   DURATION_SEC  synthetic book length in seconds        (default 600)
#   CHAPTERS      number of chapters to split into         (default 12)
#   N_FEED        feed requests sampled for p95            (default 200)
#   N_AUDIO       audio requests sampled for TTFB          (default 30)
#   PORT          loopback port to bind                    (default 18080)
#   KEEP          non-empty to keep the temp working dir   (default unset)
set -euo pipefail

DURATION_SEC="${DURATION_SEC:-600}"
CHAPTERS="${CHAPTERS:-12}"
N_FEED="${N_FEED:-200}"
N_AUDIO="${N_AUDIO:-30}"
PORT="${PORT:-18080}"
BASE="http://127.0.0.1:${PORT}"

# --- preflight -------------------------------------------------------------
for tool in ffmpeg ffprobe curl awk sort; do
  command -v "$tool" >/dev/null 2>&1 || { echo "bench: missing required tool: $tool" >&2; exit 1; }
done

# Fractional epoch seconds, portably. `date +%s.%N` is GNU-only: BSD/macOS `date`
# has no %N and prints it literally, which would poison the ingest timing. Prefer
# bash 5's EPOCHREALTIME builtin (handles a comma decimal separator under some
# locales); fall back to `date +%N` when it yields real digits; else degrade to
# whole-second resolution (old macOS bash + BSD date) rather than lie.
now() {
  if [ -n "${EPOCHREALTIME:-}" ]; then
    printf '%s' "${EPOCHREALTIME/,/.}"
    return
  fi
  local s ns
  s=$(date +%s)
  ns=$(date +%N 2>/dev/null || echo 0)
  case "$ns" in *[!0-9]*|'') ns=0 ;; esac
  printf '%s.%09d' "$s" "$((10#$ns))"
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/podspine"
if [ ! -x "$BIN" ]; then
  echo "bench: building release binary (one-time)..." >&2
  ( cd "$ROOT" && cargo build --release --quiet )
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/podspine-bench.XXXXXX")"
LIBRARY="$WORK/library"
DATA="$WORK/data"
mkdir -p "$LIBRARY" "$DATA"

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then
    echo "bench: kept work dir: $WORK" >&2
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

# --- synthesize a chapterised book ----------------------------------------
# One sine-tone .m4a of DURATION_SEC, carved into CHAPTERS equal chapters via an
# FFMETADATA sidecar (same shape the integration tests use, scaled up).
echo "bench: synthesizing ${DURATION_SEC}s / ${CHAPTERS}-chapter book..." >&2
META="$WORK/meta.txt"
{
  echo ";FFMETADATA1"
  chap_ms=$(( DURATION_SEC * 1000 / CHAPTERS ))
  i=0
  while [ "$i" -lt "$CHAPTERS" ]; do
    start=$(( i * chap_ms ))
    end=$(( start + chap_ms ))
    printf '[CHAPTER]\nTIMEBASE=1/1000\nSTART=%d\nEND=%d\ntitle=Chapter %d\n' \
      "$start" "$end" "$(( i + 1 ))"
    i=$(( i + 1 ))
  done
} >"$META"

INPUT="$LIBRARY/synthetic-audiobook.m4a"
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=220:duration=${DURATION_SEC}" \
  -i "$META" -map_metadata 1 -map 0:a -c:a aac -b:a 64k \
  "$INPUT"

# --- boot the server, timing ingest (scan + pre-split) --------------------
echo "bench: booting server and timing ingest..." >&2
t_start=$(now)
"$BIN" --library "$LIBRARY" --data-dir "$DATA" \
  --bind "127.0.0.1:${PORT}" --base-url "$BASE" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

# Poll the home grid until the book is indexed + split (a /book/ link appears).
# Elapsed from launch to that point is the ingest (pre-split) cost.
SLUG=""
for _ in $(seq 1 600); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "bench: server exited early; log:" >&2; cat "$WORK/server.log" >&2; exit 1
  fi
  home="$(curl -fsS "$BASE/" 2>/dev/null || true)"
  SLUG="$(printf '%s' "$home" | grep -oE '/book/[a-z0-9-]+' | head -1 | sed 's#/book/##' || true)"
  [ -n "$SLUG" ] && break
  sleep 0.2
done
t_ready=$(now)
[ -n "$SLUG" ] || { echo "bench: book never appeared; log:" >&2; cat "$WORK/server.log" >&2; exit 1; }

# Discover the capability feed_id from the book page.
book_page="$(curl -fsS "$BASE/book/$SLUG")"
FEED_ID="$(printf '%s' "$book_page" | grep -oE '/feed/[A-Za-z0-9_-]+\.xml' | head -1 | sed -E 's#/feed/(.+)\.xml#\1#' || true)"
[ -n "$FEED_ID" ] || { echo "bench: could not find feed_id on book page" >&2; exit 1; }
FEED_URL="$BASE/feed/${FEED_ID}.xml"
AUDIO_URL="$BASE/audio/${FEED_ID}/1"

ingest_s="$(awk -v a="$t_start" -v b="$t_ready" 'BEGIN{printf "%.2f", b-a}')"
# Extrapolate to a 10h (36000s) book — split time is ~linear in source duration.
ingest_10h="$(awk -v s="$ingest_s" -v d="$DURATION_SEC" 'BEGIN{printf "%.1f", s*36000/d}')"

# --- feed render latency (p95) --------------------------------------------
echo "bench: sampling feed latency (${N_FEED} requests)..." >&2
curl -fsS "$FEED_URL" -o /dev/null   # warm once (feed self-check + first render)
FEED_TIMES="$WORK/feed_times"
: >"$FEED_TIMES"
for _ in $(seq 1 "$N_FEED"); do
  curl -fsS -o /dev/null -w '%{time_total}\n' "$FEED_URL" >>"$FEED_TIMES"
done

# --- audio TTFB -----------------------------------------------------------
echo "bench: sampling audio TTFB (${N_AUDIO} requests)..." >&2
AUDIO_TIMES="$WORK/audio_times"
: >"$AUDIO_TIMES"
for _ in $(seq 1 "$N_AUDIO"); do
  # time_starttransfer = time to first byte; Range keeps the transfer tiny.
  curl -fsS -o /dev/null -r 0-65535 -w '%{time_starttransfer}\n' "$AUDIO_URL" >>"$AUDIO_TIMES"
done

# --- idle RSS (Linux) -----------------------------------------------------
rss_mb="n/a (non-Linux)"
if [ -r "/proc/$SERVER_PID/status" ]; then
  rss_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$SERVER_PID/status")"
  rss_mb="$(awk -v k="$rss_kb" 'BEGIN{printf "%.1f", k/1024}')"
fi

# --- percentile helper ----------------------------------------------------
# Reads seconds on stdin, prints "p50 p95 p99 max" in milliseconds.
pct() {
  sort -n | awk '
    { v[NR]=$1 }
    END {
      if (NR==0) { print "n/a n/a n/a n/a"; exit }
      p50=v[int((NR-1)*0.50)+1]; p95=v[int((NR-1)*0.95)+1];
      p99=v[int((NR-1)*0.99)+1]; mx=v[NR];
      printf "%.1f %.1f %.1f %.1f\n", p50*1000, p95*1000, p99*1000, mx*1000
    }'
}
read -r f50 f95 f99 fmax < <(pct <"$FEED_TIMES")
read -r a50 a95 a99 amax < <(pct <"$AUDIO_TIMES")

verdict() { awk -v v="$1" -v t="$2" 'BEGIN{print (v+0 <= t+0) ? "PASS" : "FAIL"}'; }

# --- report ---------------------------------------------------------------
cat <<EOF

================ Podspine performance report ================
host:      $(uname -sm)
book:      ${DURATION_SEC}s, ${CHAPTERS} chapters (synthetic sine .m4a)
samples:   feed=${N_FEED}, audio=${N_AUDIO}

NFR   metric                         measured            target      result
----- ------------------------------ ------------------- ----------- ------
P1    ingest (pre-split)             ${ingest_s}s (this book)     -           -
P1    ingest extrapolated to 10h     ${ingest_10h}s              <=120s      $(verdict "$ingest_10h" 120)
P2    feed render  p50/p95/p99       ${f50}/${f95}/${f99} ms   p95<200ms   $(verdict "$f95" 200)
P3    audio TTFB   p50/p95/p99       ${a50}/${a95}/${a99} ms   p95<300ms   $(verdict "$a95" 300)
P4    idle RSS                       ${rss_mb} MB            <50MB       $(awk -v v="$rss_mb" 'BEGIN{if (v+0==v && v!="") print (v+0<=50) ? "PASS" : "FAIL"; else print "n/a"}')

feed max ${fmax} ms   audio max ${amax} ms
=============================================================
Numbers are host-specific; TTFB here is loopback (no network hop), so a real
LAN client will be slightly higher. See docs/benchmarks.md for methodology.
EOF
