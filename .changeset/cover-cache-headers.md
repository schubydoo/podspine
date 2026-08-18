---
default: patch
---

Cache cover images: `/cover` now sends an `ETag` (a hash of the image bytes) and `Cache-Control`, and honours `If-None-Match` with a bodyless `304`, so a browser stops re-downloading every cover on each page refresh — a real speed-up over slow links like Tailscale where the grid was pulling several MB of images every time
