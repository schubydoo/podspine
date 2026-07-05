---
default: patch
---

Set the audio `Content-Type` on `/audio` responses so Apple Podcasts and other iOS clients can play episodes (axum-range sets none, which made playback fail with "this episode can't be played on this device").
