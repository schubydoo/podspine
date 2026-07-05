---
default: patch
---

Set the audio Content-Type so Apple Podcasts and other iOS players can play episodes

The `/audio/{feed_id}/{n}` endpoint streamed episodes with no `Content-Type`
header — `axum-range` emits Content-Range/Accept-Ranges/Content-Length but not a
type. Strict clients (Apple Podcasts / iOS AVPlayer) then refuse to play with
"This episode can't be played on this device", even though the RSS enclosure
already carried `type=`. The response now sets the codec-appropriate type (e.g.
`audio/mp4` for m4a/m4b, `audio/mpeg` for mp3) on both the full `200` and the
`206` Range responses.
