---
default: patch
---

Wait for a re-ingest instead of serving a chapter of the wrong length. In saver mode the server rebuilds a chapter from the source file on demand. If the source changed since the ingest, or the Refresh button invalidated the book, the rebuild no longer matches the length the feed published. Such a request now answers 503 with Retry-After, so a podcatcher retries and keeps the subscription. A rebuild whose size does not match the published length is also dropped, never served. An already cached chapter is not affected, because the ingest that wrote the row produced it.
