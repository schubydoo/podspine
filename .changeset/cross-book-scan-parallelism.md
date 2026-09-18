---
default: perf
---

Scan books in parallel instead of one at a time. A library scan used to walk its books one at a time. So a first scan of a large library left most cores idle. Each book now splits into three steps. The index reads and the id assignment stay serial, in discovery order. The probe, split, and cover work runs on a worker pool. The index writes happen back on the scan thread, in discovery order. The pool is the size of the existing process-wide ffmpeg gate. That gate still caps how many ffmpeg children run at once, so a scan does not swamp a small host. Measured on a 20-core host, a 200-book by 8-chapter library scanned about 4.5 times faster. Full mode went from 36.1s to 8.0s, and saver mode from 35s to 7.7s. Feeds are unchanged. Each book keeps its sequential pubDates. Each enclosure length is still the real output file size. Ids and guids stay stable across a rescan.
