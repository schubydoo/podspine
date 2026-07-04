---
name: Bug Report
about: Something isn't working — a feed, an episode, or the scan
title: '[BUG] '
labels: bug
assignees: ''
---

## What happened

A clear description of the bug, and what you expected instead.

## Steps to reproduce

1. Point Podspine at '...'
2. Open '...' / subscribe in '...'
3. See '...'

## The audiobook

- Format: [e.g. M4B, folder of MP3s, FLAC + .cue, Opus]
- Chapters: [embedded / `.cue` sidecar / `.ffmeta` / none]
- Anything unusual about it? [e.g. no titles, odd track numbers, huge file]

> Please don't attach copyrighted audio. A tiny synthetic sample or an
> `ffprobe -show_format -show_chapters` dump (with paths/titles redacted) is
> ideal.

## Environment

- Podspine version / image tag: [e.g. v1.0.0, ghcr.io/schubydoo/podspine:latest]
- Install method: [Docker / prebuilt binary / from source]
- OS & arch: [e.g. Debian 12 amd64, Raspberry Pi OS arm64]
- `ffmpeg -version` (first line): [e.g. ffmpeg version 6.1]
- Podcast app (if relevant): [e.g. Apple Podcasts, Pocket Casts, AntennaPod]

## Logs

Relevant server log lines (run with `RUST_LOG=info` or `debug`). **Redact** any
file paths, tokens, or private titles.

```
paste logs here
```

## Additional context

Anything else that might help.
