---
default: minor
---

Bind the HTTP port immediately and run the initial library scan in the background, so a first boot serves a self-refreshing "Scanning…" page at `/` and 503 + `Retry-After` on the `/feed`, `/audio` and `/cover` routes instead of leaving a proxy or Funnel to return 502 for the minutes the first scan takes
