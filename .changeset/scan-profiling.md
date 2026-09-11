---
default: patch
---

Add per-stage timing to the library scan and a large-library profiling script. The scanner now logs each stage at the debug level under the `podspine::scan_timing` target: probe, chapter resolve, split, cover, and index writes. It also logs a per-book total and a whole-scan total. The logs stay silent at the default level, so normal operation does not change. A new `scripts/bench-scan.sh` builds a synthetic many-book library and runs the first scan with this timing on. It reports where the time goes and how serial the scan is. See `docs/benchmarks.md`.
