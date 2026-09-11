---
default: perf
---

Split a saver-mode book's chapters in parallel, like full mode already does. Saver mode used to split each chapter one at a time. So a many-chapter book on a multi-core host ingested far slower than the same book in full mode. It now reuses the same parallel splitter, bounded by the existing process-wide ffmpeg gate. Measured on a 20-core host, a 200-book by 8-chapter library scanned about 4.8 times faster in saver mode (168s to 35s). That matches full mode. The one trade-off is transient disk. A book's whole split set now exists briefly before it is measured and deleted, rather than one chapter at a time. Steady-state saver disk is unchanged.
