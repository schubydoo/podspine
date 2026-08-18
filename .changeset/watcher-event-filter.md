---
default: patch
---

Stop the library watcher from re-scanning every few seconds while another app streams the files: watch events are now filtered to real library changes — reads (atime bumps) are ignored, as are podspine's own split output under the data dir and the dir names discovery already skips (dotdirs, `@eaDir`, `lost+found`) — so an unrelated trickle of events no longer defeats the debounce
