---
default: minor
---

Scan the library recursively, so an `Author/Title/book.m4b` layout — what Audiobookshelf, Plex and Jellyfin produce — is indexed as-is instead of returning nothing; nested books are titled by their folder and slugged by their path, a folder of several `.m4b` files is now several books, and the walk skips dot-directories, `@eaDir`, `lost+found` and a `--data-dir` placed inside the library
