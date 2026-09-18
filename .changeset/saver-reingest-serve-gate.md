---
default: patch
---

Wait for a re-ingest instead of serving a chapter of the wrong length. In saver mode the server rebuilds a chapter from the source file on demand. If the source changed since the ingest, or the Refresh button invalidated the book, the rebuild no longer matches the length the feed published. Such a request now answers 503 with Retry-After, so a podcatcher retries and keeps the subscription. The server also checks, on every request, that the file it is about to serve is the length the feed advertises. A file that is not gets refused, and the book is marked for re-ingest so that the mismatch heals.
