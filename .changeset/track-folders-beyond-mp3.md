---
default: minor
---

Read a folder of Ogg, Opus, or FLAC tracks as one book, the same rule a folder of `.mp3` tracks has always had. Such a folder was skipped with a warning before, so those books never appeared. With `PODSPINE_TRANSCODE` on, each track whose codec no podcast app plays is re-encoded once at ingest, exactly as a single chapterless file of the same format already was; with it off the tracks stream from the library untouched. A folder of several `.m4b`/`.m4a` files is still several books, one per file, because that is what an author folder usually is and every one of those books keeps its feed URL. Set `folder_is_one_book = true` in a folder's `.podspine.toml` when it is really one book split by disc: every audio file in it then becomes one track of one book. An MP3 folder keeps exactly its `.mp3` tracks, so a stray file of another format cannot renumber a feed that subscribers already hold.
