#!/usr/bin/env sh
# Lint .changeset/*.md fragments for knope (knope-dev/knope). Two rules, both
# learned the hard way against knope 0.23:
#   1. Every file needs YAML front matter (opens with `---`, closes with `---`).
#      knope parses EVERY .md in .changeset/, so a stray README.md fails the whole
#      release with "missing front matter".
#   2. The body (after the front matter) must be a SINGLE non-empty line. knope
#      renders any 2nd line / paragraph as a `#### heading` block instead of a clean
#      changelog bullet, which corrupts the release notes.
# Mirrors clauster's scripts/lint_changesets.py. POSIX sh; no runtime deps.
set -eu

fail=0
found=0
for f in .changeset/*.md; do
  [ -e "$f" ] || continue
  found=$((found + 1))

  if [ "$(sed -n '1p' "$f")" != "---" ]; then
    printf '  x %s: no YAML front matter. Do NOT put a README/doc in .changeset/ — knope parses every .md there.\n' "$f"
    fail=1
    continue
  fi
  close=$(awk 'NR>1 && /^---[[:space:]]*$/ {print NR; exit}' "$f")
  if [ -z "$close" ]; then
    printf '  x %s: front matter never closes (no second ---).\n' "$f"
    fail=1
    continue
  fi
  # Count non-blank body lines after the closing ---.
  bodylines=$(awk -v c="$close" 'NR>c && NF > 0 {n++} END {print n + 0}' "$f")
  if [ "$bodylines" -eq 0 ]; then
    printf '  x %s: empty body — needs a one-line summary.\n' "$f"
    fail=1
  elif [ "$bodylines" -gt 1 ]; then
    printf '  x %s: body spans %s lines — knope renders that as a #### heading, not a bullet. Keep the WHOLE entry on ONE line.\n' "$f" "$bodylines"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "Changeset lint FAILED." >&2
  exit 1
fi
echo "Changeset lint: $found fragment(s) OK."
