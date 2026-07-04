//! `feed` — builds one podcast RSS 2.0 channel per book.
//!
//! The correctness rules here are the ones that killed Podspine's predecessors,
//! so they are treated as first-class (a built-in self-check lands in Task 1.5):
//! - **Sequential `<pubDate>`, oldest = chapter 1.** Dates are anchored to the
//!   source mtime and stepped so every episode lands in the past and pubDates are
//!   strictly increasing with chapter order.
//! - **Stable `<guid>`** = `blake3(book.id : idx : source_mtime)` — stable across
//!   re-runs of an unchanged source; changes only when the source mtime changes.
//! - Every item carries `<itunes:episode>`, `<itunes:duration>` (`HH:MM:SS`) and
//!   an `<enclosure>` whose `length` is the **real** output byte size.
//!
//! See TAD §4/§5.2.

use std::collections::BTreeMap;

use rss::extension::itunes::{ITunesChannelExtension, ITunesItemExtension};
use rss::{Channel, Enclosure, Guid, Item};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

const PODCAST_NS: &str = "https://podcastindex.org/namespace/1.0";

/// Seconds between successive episode pubDates. Only the *ordering* matters to
/// podcast apps; the spacing just keeps the dates visibly distinct.
const PUBDATE_STEP_SECS: i64 = 60;

/// One episode's inputs to the feed.
#[derive(Debug, Clone)]
pub struct FeedEpisode {
    /// Zero-based chapter index (episode number in the feed is `idx + 1`).
    pub idx: usize,
    /// Episode title.
    pub title: String,
    /// Absolute URL to the audio file (the `<enclosure>` url).
    pub audio_url: String,
    /// Real output size in bytes — the `<enclosure>` `length`.
    pub byte_length: u64,
    /// Episode duration in seconds.
    pub duration_sec: f64,
    /// Enclosure MIME type (e.g. `audio/mp4`, `audio/mpeg`).
    pub mime_type: String,
}

/// One book's inputs to the feed.
#[derive(Debug, Clone)]
pub struct FeedBook {
    /// Opaque, stable book id — part of each episode guid.
    pub id: String,
    /// Feed/channel title.
    pub title: String,
    /// Author (`itunes:author`), if known.
    pub author: Option<String>,
    /// Channel description / `itunes:summary`.
    pub description: Option<String>,
    /// Cover image URL (`itunes:image`, per-item and channel-level).
    pub cover_url: Option<String>,
    /// Source file mtime (epoch seconds) — pubDate anchor + guid material.
    pub source_mtime: i64,
    /// The feed's own URL (channel `<link>`).
    pub self_url: String,
    /// Episodes in chapter order (idx ascending).
    pub episodes: Vec<FeedEpisode>,
}

/// Stable episode guid: `blake3(book.id : idx : source_mtime)` as hex.
pub fn episode_guid(book_id: &str, idx: usize, source_mtime: i64) -> String {
    let material = format!("{book_id}:{idx}:{source_mtime}");
    blake3::hash(material.as_bytes()).to_hex().to_string()
}

/// Format a duration as `HH:MM:SS` for `<itunes:duration>`.
pub fn format_itunes_duration(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs.round() as i64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// pubDate epoch for episode `idx` of `n`, anchored so the last episode sits at
/// `anchor` and earlier ones step backwards — i.e. every date is `<= anchor`
/// (in the past) and strictly increasing with `idx` (chapter 1 oldest).
fn pubdate_epoch(anchor: i64, idx: usize, n: usize) -> i64 {
    let back = (n as i64 - 1 - idx as i64) * PUBDATE_STEP_SECS;
    anchor - back
}

/// Format an epoch as an RFC 2822 date string (RSS `<pubDate>` format).
fn format_rfc2822(epoch: i64) -> String {
    OffsetDateTime::from_unix_timestamp(epoch)
        .ok()
        .and_then(|dt| dt.format(&Rfc2822).ok())
        .unwrap_or_default()
}

/// Build the RSS [`Channel`] for a book. Items are emitted in chapter order
/// (oldest first); ordering guarantees come from pubDate + `itunes:episode`.
pub fn build_channel(book: &FeedBook) -> Channel {
    let n = book.episodes.len();

    let items = book
        .episodes
        .iter()
        .map(|ep| {
            let mut item = Item::default();
            item.set_title(ep.title.clone());
            item.set_pub_date(format_rfc2822(pubdate_epoch(book.source_mtime, ep.idx, n)));

            item.set_enclosure(Enclosure {
                url: ep.audio_url.clone(),
                length: ep.byte_length.to_string(),
                mime_type: ep.mime_type.clone(),
            });
            item.set_guid(Guid {
                value: episode_guid(&book.id, ep.idx, book.source_mtime),
                permalink: false,
            });

            let mut it = ITunesItemExtension::default();
            it.set_episode(Some((ep.idx + 1).to_string()));
            it.set_duration(Some(format_itunes_duration(ep.duration_sec)));
            if let Some(cover) = &book.cover_url {
                it.set_image(Some(cover.clone()));
            }
            item.set_itunes_ext(Some(it));

            item
        })
        .collect::<Vec<_>>();

    let mut channel = Channel::default();
    channel.set_title(book.title.clone());
    channel.set_link(book.self_url.clone());
    channel.set_description(
        book.description
            .clone()
            .unwrap_or_else(|| book.title.clone()),
    );
    channel.set_language("en".to_string());
    channel.set_last_build_date(format_rfc2822(book.source_mtime));
    channel.set_pub_date(format_rfc2822(book.source_mtime));

    let mut ch_it = ITunesChannelExtension::default();
    ch_it.set_author(book.author.clone());
    ch_it.set_image(book.cover_url.clone());
    ch_it.set_summary(book.description.clone());
    channel.set_itunes_ext(Some(ch_it));

    // The rss crate emits xmlns:itunes automatically when itunes_ext is set; we
    // only need to declare the podcast namespace here.
    let mut ns = BTreeMap::new();
    ns.insert("podcast".to_string(), PODCAST_NS.to_string());
    channel.set_namespaces(ns);

    channel.set_items(items);
    channel
}

/// Render a book's feed to an XML string.
pub fn render(book: &FeedBook) -> String {
    build_channel(book).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize, mtime: i64) -> FeedBook {
        let episodes = (0..n)
            .map(|idx| FeedEpisode {
                idx,
                title: format!("Chapter {}", idx + 1),
                audio_url: format!("http://host/audio/book/{:03}.m4a", idx + 1),
                byte_length: 1000 + idx as u64,
                duration_sec: 61.0 * (idx as f64 + 1.0),
                mime_type: "audio/mp4".to_string(),
            })
            .collect();
        FeedBook {
            id: "book-1".to_string(),
            title: "A Test Book".to_string(),
            author: Some("An Author".to_string()),
            description: Some("A description".to_string()),
            cover_url: Some("http://host/cover.jpg".to_string()),
            source_mtime: mtime,
            self_url: "http://host/feed/book.xml".to_string(),
            episodes,
        }
    }

    #[test]
    fn guid_is_stable_and_mtime_sensitive() {
        assert_eq!(episode_guid("b", 0, 100), episode_guid("b", 0, 100));
        assert_ne!(episode_guid("b", 0, 100), episode_guid("b", 0, 101));
        assert_ne!(episode_guid("b", 0, 100), episode_guid("b", 1, 100));
        assert_ne!(episode_guid("a", 0, 100), episode_guid("b", 0, 100));
    }

    #[test]
    fn duration_is_hh_mm_ss() {
        assert_eq!(format_itunes_duration(0.0), "00:00:00");
        assert_eq!(format_itunes_duration(61.0), "00:01:01");
        assert_eq!(format_itunes_duration(3661.0), "01:01:01");
        assert_eq!(format_itunes_duration(-5.0), "00:00:00");
    }

    #[test]
    fn pubdates_are_monotonic_oldest_first_and_in_the_past() {
        let anchor = 1_700_000_000;
        let n = 5;
        let epochs: Vec<i64> = (0..n).map(|i| pubdate_epoch(anchor, i, n)).collect();
        for w in epochs.windows(2) {
            assert!(
                w[0] < w[1],
                "pubDates must strictly increase with chapter idx"
            );
        }
        assert_eq!(
            *epochs.last().unwrap(),
            anchor,
            "last episode anchored to mtime"
        );
        assert!(
            epochs.iter().all(|&e| e <= anchor),
            "all pubDates <= anchor (past)"
        );
    }

    #[test]
    fn channel_items_carry_required_tags() {
        let book = sample(3, 1_700_000_000);
        let channel = build_channel(&book);
        assert_eq!(channel.items().len(), 3);

        for (i, item) in channel.items().iter().enumerate() {
            let enc = item.enclosure().expect("enclosure present");
            assert!(!enc.length().is_empty(), "enclosure length non-empty");
            assert_eq!(enc.length(), (1000 + i).to_string());
            assert_eq!(enc.mime_type(), "audio/mp4");

            let it = item.itunes_ext().expect("itunes ext present");
            assert_eq!(it.episode(), Some((i + 1).to_string()).as_deref());
            assert!(it.duration().is_some(), "itunes:duration present");

            let guid = item.guid().expect("guid present");
            assert!(!guid.is_permalink(), "guid is not a permalink");
            assert_eq!(guid.value(), episode_guid(&book.id, i, book.source_mtime));
        }
    }

    #[test]
    fn rendered_xml_has_namespaces_and_required_elements() {
        let xml = render(&sample(2, 1_700_000_000));
        assert_eq!(
            xml.matches("xmlns:itunes").count(),
            1,
            "exactly one itunes ns"
        );
        assert!(xml.contains("xmlns:podcast"), "podcast namespace declared");
        assert!(xml.contains("<itunes:duration>"));
        assert!(xml.contains("<itunes:episode>"));
        assert!(xml.contains("<enclosure "));
        assert!(xml.contains("length=\"1000\""));
        assert!(xml.contains("<pubDate>"));
        assert!(xml.contains("<guid"));
    }

    #[test]
    fn rendered_pubdates_are_present_for_every_item() {
        let xml = render(&sample(4, 1_700_000_000));
        assert_eq!(
            xml.matches("<pubDate>").count(),
            4 + 1,
            "one per item + channel"
        );
    }
}
