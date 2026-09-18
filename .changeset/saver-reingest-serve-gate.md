---
default: patch
---

Refuse to serve a chapter whose size is not the length the feed publishes. In saver mode the server rebuilds a chapter from the source file on demand. If the source changed since the ingest, the rebuild no longer matches the published length. Such a request now answers 503 with Retry-After, so a podcatcher retries and keeps the subscription. The server checks the size of every file it serves, and refuses any file that disagrees with its feed. A scan now treats a file of the wrong size as out of date, so it re-ingests that book and the two agree again.
