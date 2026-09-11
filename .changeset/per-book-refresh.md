---
default: minor
---

Add a per-book "Refresh" button on the book page. It forces a one-time re-ingest of that book: re-probe, re-split, and re-extract the cover. A changed title, chapters, or cover art is then picked up without editing the file or restarting the server. The button posts to `/book/{slug}/refresh`, guarded to same-origin like the regenerate control. The handler invalidates the book in the index and asks the watcher (the single reconciler) to re-ingest it. The capability URL and episode guids stay stable. The re-split runs in the background, so reload the page to see the result.
