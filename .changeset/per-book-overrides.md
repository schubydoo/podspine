---
default: minor
---

Add per-book `.podspine.toml` overrides: a sidecar beside a single-file book (`Author - Title.podspine.toml`) or inside a folder book overrides settings for just that book — `storage_mode`, `force_embedded_chapters`, `remux_non_faststart`, `default_cover_url`, plus troubleshooting knobs `disabled`, `title`, `author`, and `force_reingest` — with precedence sidecar → CLI/env → global config → default. Server-wide keys placed in a per-book file are ignored with a warning.
