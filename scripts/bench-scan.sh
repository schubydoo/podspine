#!/usr/bin/env bash
# Podspine first-scan profiler — where does a large library's first scan spend
# its time, and how does it scale?
#
# The sibling scripts/bench.sh times ONE book's ingest. This one builds a large
# synthetic library (many books), runs the first scan once with per-stage timing
# turned on, and prints a breakdown: how long each stage (probe, resolve, split,
# cover, index) took across all books, plus the whole-scan wall-clock and a
# serialization ratio that shows how much cross-book parallelism could still win.
#
# The per-stage numbers come from the scanner's debug timing (target
# `podspine::scan_timing`), enabled here via RUST_LOG. They are inert at the
# default log level, so this measures the real scan with no behavior change.
#
# It is deliberately dependency-light (bash, ffmpeg, curl, awk), like bench.sh,
# and touches nothing the server ships. It builds the release binary,
# synthesizes ONE chaptered .m4a, copies it into BOOKS book folders (distinct
# paths scan as distinct books), boots podspine on loopback, waits for every
# book to be indexed, prints the report, and tears everything down.
#
# Usage:
#   scripts/bench-scan.sh                        # defaults: 200 books, 8 chapters
#   BOOKS=500 CHAPTERS=20 scripts/bench-scan.sh
#   STORAGE_MODE=saver scripts/bench-scan.sh     # measure saver mode
#   KEEP=1 scripts/bench-scan.sh                 # keep the temp dir + server log
#
# Env knobs (all optional):
#   BOOKS         number of books in the library         (default 200)
#   CHAPTERS      chapters per book                       (default 8)
#   DURATION_SEC  per-book length in seconds              (default 300)
#   STORAGE_MODE  full | saver                            (default full)
#   PORT          loopback port to bind                   (default 18081)
#   KEEP          non-empty to keep the temp working dir  (default unset)
set -euo pipefail

BOOKS="${BOOKS:-200}"
CHAPTERS="${CHAPTERS:-8}"
DURATION_SEC="${DURATION_SEC:-300}"
STORAGE_MODE="${STORAGE_MODE:-full}"
PORT="${PORT:-18081}"
BASE="http://127.0.0.1:${PORT}"

# --- preflight -------------------------------------------------------------
for tool in ffmpeg ffprobe curl awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "bench-scan: missing required tool: $tool" >&2; exit 1; }
done

# Fractional epoch seconds, portably (same helper as bench.sh).
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
  echo "bench-scan: building release binary (one-time)..." >&2
  ( cd "$ROOT" && cargo build --release --quiet )
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/podspine-benchscan.XXXXXX")"
LIBRARY="$WORK/library"
DATA="$WORK/data"
mkdir -p "$LIBRARY" "$DATA"

SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
  if [ -n "${KEEP:-}" ]; then
    echo "bench-scan: kept work dir: $WORK" >&2
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

# --- synthesize ONE chaptered book, then copy it into BOOKS folders --------
echo "bench-scan: synthesizing 1 book (${DURATION_SEC}s / ${CHAPTERS} chapters), copying into ${BOOKS} folders..." >&2
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

SEED="$WORK/seed.m4a"
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=220:duration=${DURATION_SEC}" \
  -i "$META" -map_metadata 1 -map 0:a -c:a aac -b:a 64k \
  "$SEED"

# Distinct paths scan as distinct books even though the bytes are identical (the
# scanner dedups by source path, not content). Copying is fast; ffmpeg runs once.
i=1
while [ "$i" -le "$BOOKS" ]; do
  d="$LIBRARY/$(printf 'book-%04d' "$i")"
  mkdir -p "$d"
  cp "$SEED" "$d/audiobook.m4a"
  i=$(( i + 1 ))
done
rm -f "$SEED"

# --- boot the server, timing the whole first scan -------------------------
echo "bench-scan: booting server and timing the first scan (storage=${STORAGE_MODE})..." >&2
LOG="$WORK/server.log"
t_start=$(now)
RUST_LOG="info,podspine::scan_timing=debug" \
PODSPINE_STORAGE_MODE="$STORAGE_MODE" \
  "$BIN" --library "$LIBRARY" --data-dir "$DATA" \
  --bind "127.0.0.1:${PORT}" --base-url "$BASE" >"$LOG" 2>&1 &
SERVER_PID=$!

# Poll the grid until all BOOKS books are indexed (that many /book/ links).
count=0
for _ in $(seq 1 3000); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "bench-scan: server exited early; log:" >&2; cat "$LOG" >&2; exit 1
  fi
  home="$(curl -fsS "$BASE/" 2>/dev/null || true)"
  # `|| true`: grep exits 1 (and pipefail would abort) while the grid is still
  # empty early in the scan; wc still prints a count, so this yields 0 then.
  count="$(printf '%s' "$home" | grep -oE '/book/[a-z0-9-]+' | sort -u | wc -l | tr -d ' ' || true)"
  [ "${count:-0}" -ge "$BOOKS" ] && break
  sleep 0.2
done
t_ready=$(now)
[ "$count" -ge "$BOOKS" ] || {
  echo "bench-scan: only ${count}/${BOOKS} books indexed before timeout; log tail:" >&2
  tail -20 "$LOG" >&2; exit 1
}

wall_s="$(awk -v a="$t_start" -v b="$t_ready" 'BEGIN{printf "%.2f", b-a}')"

# --- aggregate per-stage timings from the log -----------------------------
# Lines look like: "... DEBUG podspine::scan_timing: stage timing book=... stage="probe" ms=1.23"
# Sum + count per stage; the scanner's own `scan_total` is the pure scan time.
REPORT="$(awk '
  # The fmt subscriber colorizes even to a file, so strip ANSI escapes first,
  # otherwise the field regexes below cannot match through the color codes.
  { gsub(/\033\[[0-9;]*m/, "") }
  /stage timing/ {
    if (match($0, /stage="[^"]+"/)) { s = substr($0, RSTART+7, RLENGTH-8) } else next
    if (match($0, /ms=[0-9.]+/))    { m = substr($0, RSTART+3, RLENGTH-3) + 0 } else next
    sum[s] += m; cnt[s]++
  }
  END {
    # Per-book stages, in pipeline order. book_total is the per-book sum;
    # scan_total is the whole serial scan the scanner measured.
    n = split("probe resolve split cover index book_total", order, " ")
    printf "  %-11s %10s %8s %9s\n", "stage", "total(s)", "count", "mean(ms)"
    printf "  %-11s %10s %8s %9s\n", "-----", "--------", "-----", "--------"
    for (k = 1; k <= n; k++) {
      s = order[k]
      if (cnt[s] == 0) continue
      printf "  %-11s %10.2f %8d %9.2f\n", s, sum[s]/1000.0, cnt[s], sum[s]/cnt[s]
    }
    printf "SCAN_TOTAL_S=%.2f\n", sum["scan_total"]/1000.0
    printf "BOOK_TOTAL_S=%.2f\n", sum["book_total"]/1000.0
  }
' "$LOG")"

table="$(printf '%s\n' "$REPORT" | grep -vE '^(SCAN_TOTAL_S|BOOK_TOTAL_S)=')"
scan_total_s="$(printf '%s\n' "$REPORT" | awk -F= '/^SCAN_TOTAL_S=/{print $2}')"
book_total_s="$(printf '%s\n' "$REPORT" | awk -F= '/^BOOK_TOTAL_S=/{print $2}')"
# Effective parallelism during the scan: summed per-book work / wall-clock scan
# time. ~1.0 = fully serial (cores idle); higher = the split parallelism helped.
# Well below the core count means cross-book parallelism has headroom.
ratio="$(awk -v b="$book_total_s" -v s="$scan_total_s" 'BEGIN{ if (s+0>0) printf "%.2f", b/s; else print "n/a" }')"
cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo '?')"

cat <<EOF

============== Podspine first-scan profile ==============
host:      $(uname -sm), ${cores} cores
library:   ${BOOKS} books x ${CHAPTERS} chapters (${DURATION_SEC}s each), storage=${STORAGE_MODE}

per-stage totals across all books (from scanner debug timing):
${table}

scan_total (scanner, pure scan):  ${scan_total_s}s
sum of per-book work (book_total): ${book_total_s}s
effective parallelism (book_total/scan_total): ${ratio}x  of ${cores} cores
launch -> all indexed (wall):     ${wall_s}s  (includes startup + HTTP poll)
========================================================
A ratio near 1.0 means the scan ran serially across books and most cores sat
idle -> cross-book parallelism is the win. A high per-stage total (e.g. split or
index) names the stage to target. Numbers are host-specific; see
docs/benchmarks.md for methodology.
EOF
