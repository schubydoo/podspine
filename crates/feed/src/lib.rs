//! `feed` — builds one `rss::Channel` per book: N `<item>`s each with a stable
//! `<guid>`, sequential `<pubDate>` (oldest = ch1), `<itunes:episode>`,
//! `<itunes:duration>` (`HH:MM:SS`), `<enclosure>` with real byte `length` +
//! correct MIME, and `<itunes:image>`. Includes a self-check that fails
//! generation on missing enclosure length, non-monotonic pubDates, or invalid
//! XML. See TAD §4/§5.2. Implemented in Sprint 1 (Tasks 1.4 + 1.5).
