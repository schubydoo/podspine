---
default: minor
---

Add an "Add to a podcast app" subscribe page with per-app deep links

The book-page QR now opens a `/subscribe/{feed_id}` helper page instead of
encoding the raw feed URL (which the iOS Camera couldn't open — it just showed
XML). The page offers one-tap "Open in…" deep links for Apple Podcasts, Overcast,
Pocket Casts, Castro, AntennaPod, and Podcast Addict, with per-app QR codes
(collapsed behind an expander) for scanning from another device, plus a
copy-the-URL fallback. No JavaScript required.
