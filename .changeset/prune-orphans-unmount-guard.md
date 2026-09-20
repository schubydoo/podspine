---
default: patch
---

Keep a book whose folder went away instead of deleting it from the index. A library root can hold several shares. When one share unmounts, its mount point stays behind as an empty folder, so the root is still populated and the whole-library guard does not fire. Every book of that share then looked deleted, and a delete takes the book's feed URL with it, so a remount minted a new URL and broke every subscription to those books. A scan now keeps a book whose nearest surviving folder is unreadable, or holds nothing but housekeeping files such as `.DS_Store` or `@eaDir`, and warns instead. The cost: a book you delete from a folder that is now empty stays indexed until you remove that folder.
