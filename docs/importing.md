# Adding a Podspine feed to your podcast app

Every book has its own feed URL — a private, unguessable **capability link**.

## The easy way: the subscribe page

In the Podspine web UI (`http://<host>:8080/`), open a book and **scan its QR code**
with your phone (or open the book page directly). The QR opens that book's
**subscribe page** (`/subscribe/<feed_id>`) — a set of one-tap **"Open in…"** deep
links for Apple Podcasts, Overcast, Pocket Casts, Castro, AntennaPod, and Podcast
Addict, with a per-app QR behind an expander. Tap the app you use and it opens with
the feed ready to add. This is the phone-friendly path — especially on iOS, where
Apple Podcasts has no "add by URL" of its own.

## The manual way: add by URL

Prefer to paste the URL yourself? On the book page use **Copy feed URL**. It looks
like:

```
http://<your-host>:8080/feed/<feed_id>.xml
```

where `<feed_id>` is a random per-book id (not the title). Treat it like a password —
anyone with it can subscribe. If it leaks, use **Regenerate link** on the book page
to replace it (the old URL stops working). Then add it as a podcast **by URL** in your
app. Most apps hide this behind an "add by URL / RSS" option because these feeds are
deliberately kept out of the public podcast directories.

## Per-app steps (adding by URL)

These are the manual steps if you copied the feed URL. On a phone, the
[subscribe page](#the-easy-way-the-subscribe-page) is usually quicker.

### Apple Podcasts
- **macOS:** File → *Add a Show by URL…* → paste the feed URL.
- **iOS:** Apple Podcasts has no "add by URL" of its own. Use the **subscribe
  page's "Open in Apple Podcasts"** deep link (scan the book's QR), subscribe once
  on a Mac signed into the same Apple ID, or use a third-party app (below).

### Pocket Casts
- Profile → *Add Podcast* → *Add URL* → paste the feed URL.
- Works on mobile and web.

### Overcast (iOS)
- Tap **＋** (add) → *Add URL* → paste the feed URL.

### AntennaPod (Android)
- *Add Podcast* → *Add podcast by RSS address* → paste the feed URL.

### Other apps
Anything that reads standard RSS works — gPodder, Podverse, iVoox, browser
podcast extensions, etc. Look for "subscribe by URL" or "add RSS feed."

## Troubleshooting

### Episodes play out of order
Podspine emits sequential `pubDate`s (oldest = chapter 1) and `itunes:episode`
numbers specifically to prevent this. If an app still shows them reversed, sort
the show by **oldest first** / **publish date ascending** in the app's episode
list settings — some apps default to newest-first for the *display* even though
playback order is correct.

### The feed won't load / "invalid feed"
- Confirm the URL is reachable **from the device running the app**, not just from
  the Podspine host. If you used `localhost`, that's the problem — set
  `PODSPINE_BASE_URL` to your LAN IP or hostname and re-copy the URL.
- Check `http://<host>:8080/healthz` returns `ok`.
- If you're behind a reverse proxy, make sure it forwards to Podspine and that
  `PODSPINE_BASE_URL` matches the public URL.

### Audio won't play or won't scrub/seek
- Podspine supports HTTP Range (seek) on `/audio/...`. If a proxy strips
  `Range`/`Accept-Ranges` headers, seeking breaks — configure it to pass them
  through.
- Confirm the episode file exists under your data volume
  (`<data>/books/<slug>/`).

### A book didn't appear
- Check the startup logs. Common reasons a book is skipped:
  - **DRM** (`.aax`/`.aaxc`/`.aa`/`.odm`) — skipped by design; convert to a
    DRM-free format first.
  - Unsupported/unreadable file, or a folder with no audio.
- A folder of MP3s with **missing or duplicate track numbers** falls back to
  **filename order** — rename files so they sort correctly (e.g. `01 - …`,
  `02 - …`).

### Chapters are wrong / missing titles
- FLAC (and some Ogg) files don't carry titled embedded chapters. Add a `.cue`
  sidecar next to the audio file; Podspine prefers it automatically.
- To ignore a sidecar and use embedded chapters, run with
  `--force-embedded-chapters`.

### The cover art is missing
- Books with no embedded cover show a lettered placeholder in the UI and no
  `itunes:image` unless you set `--default-cover-url`.

## See also

- [README](../README.md) — what Podspine is and the quick start.
- [DEPLOYMENT.md](DEPLOYMENT.md) — running it, reverse proxy, and the full config reference.
- [SECURITY.md](../SECURITY.md) — why feed URLs are capability links, and how to expose them safely.
