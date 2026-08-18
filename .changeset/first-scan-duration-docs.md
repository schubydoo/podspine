---
default: patch
---

Document that the first scan can take a while: the README and docs quick-start now explain that the initial run splits every book (so a large library takes minutes), shows a self-refreshing "Scanning…" page meanwhile, and answers 503 + Retry-After on feed/audio until it finishes — while a warm restart stays fast
