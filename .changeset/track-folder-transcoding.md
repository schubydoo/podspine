---
default: minor
---

Re-encode a folder book's tracks when `PODSPINE_TRANSCODE` is on. A folder of FLAC, Ogg, or Opus tracks was served as it is, so a podcast app that does not play those formats could not play the book, although a single chapterless file of the same format was already re-encoded. Each such track is now re-encoded once at ingest into the data directory, and the book records the mode, so the server never rebuilds or evicts those files. A folder can hold both kinds: an MP3 track still streams in place from the library beside a re-encoded FLAC one. The re-encodes are all or none. A folder publishes new track files only after every one of them succeeds, so a track that fails to encode leaves every episode the feed already advertises exactly where it was. A disk fault in the middle of the publishing step is the one case no rename sequence can undo, and the next scan re-ingests that book.
