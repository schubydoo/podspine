---
default: patch
---

Log every previously swallowed error — internal 500s, skipped cache evictions, failed disabled-book prunes, and skipped id-reuse scans now leave a diagnosable trail in the server log instead of failing silently
