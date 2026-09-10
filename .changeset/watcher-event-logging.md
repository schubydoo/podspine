---
default: patch
---

Log the library watch event that triggers a rescan, at the debug level, so a surprise rescan is explainable. Each triggering event records its kind and paths, and the reconcile line now reports how many events were coalesced. Turn it on with `--log-level debug`. It helps most where another app shares the library directory and keeps touching files.
