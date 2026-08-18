---
default: patch
---

Document that the first scan can take a while: the README and docs quick-start now explain that the initial run splits chaptered books into per-chapter episodes (whole-file books stream in place), so a large chaptered library takes minutes, shows a self-refreshing "Scanning…" page meanwhile, and answers 503 + Retry-After on feed/audio until it finishes — while a warm restart stays fast
