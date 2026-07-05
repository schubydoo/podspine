# Adding a Podspine feed to your podcast app

Every book has its own feed URL — a private, unguessable **capability link**. In the
Podspine web UI (`http://<host>:8080/`), open a book and use **Copy feed URL** (or
scan the QR code with your phone). The URL looks like:

```
http://<your-host>:8080/feed/<feed_id>.xml
```

where `<feed_id>` is a random per-book id (not the title). Treat it like a password —
anyone with it can subscribe. If it leaks, use **Regenerate link** on the book page
to replace it (the old URL stops working). Then add it as a podcast **by URL** in your
app. Most apps hide this behind an "add by URL / RSS" option because these feeds are
deliberately kept out of the public podcast directories.

## Per-app steps

### Apple Podcasts
- **macOS:** File → *Add a Show by URL…* → paste the feed URL.
- **iOS:** there's no "add by URL" in the app itself. Subscribe once on a Mac
  signed into the same Apple ID, or use a third-party app (below) for phone-only
  setups.

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
