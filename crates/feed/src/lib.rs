//! `feed` — builds one podcast RSS 2.0 channel per book.
//!
//! The correctness rules here are the ones that killed Podspine's predecessors,
//! so they are treated as first-class, and the [`selfcheck`] module refuses to
//! let a broken feed be served:
//! - **Sequential `<pubDate>`, oldest = chapter 1.** Dates are anchored to the
//!   source mtime and stepped so every episode lands in the past and pubDates are
//!   strictly increasing with chapter order.
//! - **Stable `<guid>`** = `blake3(book.id : idx : source_mtime)`. It is
//!   stable across re-runs of an unchanged source; it changes only when the
//!   source mtime changes.
//! - Every item carries `<itunes:episode>`, `<itunes:duration>` (`HH:MM:SS`) and
//!   an `<enclosure>` whose `length` is the **real** output byte size.
//!
//! See TAD §4/§5.2.

use std::collections::BTreeMap;

use rss::extension::itunes::{ITunesChannelExtension, ITunesItemExtension};
use rss::{Channel, Enclosure, Guid, Item};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

pub mod selfcheck;

const PODCAST_NS: &str = "https://podcastindex.org/namespace/1.0";

/// Seconds between successive episode pubDates. Only the *ordering* matters
/// to podcast apps; the spacing only keeps the dates visibly distinct.
const PUBDATE_STEP_SECS: i64 = 60;

/// One episode's inputs to the feed.
#[derive(Debug, Clone)]
pub struct FeedEpisode {
    /// Zero-based chapter index (episode number in the feed is `idx + 1`).
    pub idx: usize,
    /// The guid recorded at ingest. The feed publishes it as it stands, so
    /// that a client keeps the episode it already downloaded.
    pub guid: String,
    /// The pubDate epoch recorded at ingest, in seconds.
    pub pubdate_epoch: i64,
    /// Episode title.
    pub title: String,
    /// Absolute URL to the audio file (the `<enclosure>` url).
    pub audio_url: String,
    /// Real output size in bytes: the `<enclosure>` `length`.
    pub byte_length: u64,
    /// Episode duration in seconds.
    pub duration_sec: f64,
    /// Enclosure MIME type (e.g. `audio/mp4`, `audio/mpeg`).
    pub mime_type: String,
}

/// One book's inputs to the feed.
#[derive(Debug, Clone)]
pub struct FeedBook {
    /// Opaque, stable book id.
    pub id: String,
    /// Feed/channel title.
    pub title: String,
    /// Author (`itunes:author`), if known.
    pub author: Option<String>,
    /// Channel description / `itunes:summary`.
    pub description: Option<String>,
    /// Cover image URL (`itunes:image`, per-item and channel-level).
    pub cover_url: Option<String>,
    /// Source file mtime (epoch seconds). The scanner anchors the stored
    /// episode pubDates on it at ingest. The feed itself reads it only as the
    /// channel date of a book with no episodes.
    pub source_mtime: i64,
    /// The feed's own URL (channel `<link>`).
    pub self_url: String,
    /// Episodes in chapter order (idx ascending).
    pub episodes: Vec<FeedEpisode>,
}

/// Stable episode guid: `blake3(book.id : idx : source_mtime)` as hex.
///
/// This is the identity of a chapter, which is a sub-range of one container:
/// the position IS what the chapter is, and the whole book re-splits whenever
/// its source changes. A folder book is the other shape, and it uses
/// [`track_guid`].
pub fn episode_guid(book_id: &str, idx: usize, source_mtime: i64) -> String {
    let material = format!("{book_id}:{idx}:{source_mtime}");
    blake3::hash(material.as_bytes()).to_hex().to_string()
}

/// Stable episode guid for one track of a folder book:
/// `blake3(book.id : file name : mtime)` as hex.
///
/// A folder track is a file of its own, so its identity follows that file and
/// not its position. Position cannot work here: deleting one track renumbers
/// every track after it, while the folder's `source_mtime` (the newest track
/// mtime) does not move, so a position-based guid would hand a later track the
/// guid a subscriber already holds for the one that went, and a podcast app
/// would keep the audio it already downloaded under that guid.
///
/// `file_name` is the track's own name inside the folder, which is unique
/// there, and `mtime` is that file's own timestamp, so replacing one track
/// leaves every other guid alone.
///
/// The name arrives as the platform's own **bytes**
/// (`OsStr::as_encoded_bytes`), never as a lossy string. A Unix filename is
/// any byte sequence, and two names that differ only in bytes no `str` can
/// hold would otherwise hash the same: the guid is the episode table's
/// primary key, so one track would overwrite the other and vanish from the
/// feed (Greptile P1). A name that IS valid UTF-8 hashes exactly
/// `book_id:name:mtime`, so such a guid does not move.
pub fn track_guid(book_id: &str, file_name: &[u8], mtime: i64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(book_id.as_bytes());
    hasher.update(b":");
    hasher.update(file_name);
    hasher.update(b":");
    hasher.update(mtime.to_string().as_bytes());
    hasher.finalize().to_hex().to_string()
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

/// pubDate epoch for episode `idx` of `n`, anchored so that the last episode
/// sits at `anchor` and earlier ones step backwards. So every date is
/// `<= anchor` (in the past) and strictly increases with `idx` (chapter 1 is
/// the oldest).
///
/// Public so the scanner can persist the same value it would render, keeping the
/// index and the feed in agreement.
pub fn pubdate_epoch(anchor: i64, idx: usize, n: usize) -> i64 {
    let back = (n as i64 - 1 - idx as i64) * PUBDATE_STEP_SECS;
    anchor - back
}

/// Format an epoch as an RFC 2822 date string (RSS `<pubDate>` format).
///
/// The empty-string fallback is unreachable with real mtimes: every epoch in
/// 0..now is well inside `OffsetDateTime`'s range, and Rfc2822 formats any such
/// datetime. Item-level dates are additionally guarded by selfcheck's
/// `BadPubDate`; channel-level dates are not, which is why this stays a plain
/// fallback rather than an `Option`.
fn format_rfc2822(epoch: i64) -> String {
    OffsetDateTime::from_unix_timestamp(epoch)
        .ok()
        .and_then(|dt| dt.format(&Rfc2822).ok())
        .unwrap_or_default()
}

/// Build the RSS [`Channel`] for a book. Items are emitted in chapter order
/// (oldest first); the ordering guarantees come from pubDate and
/// `itunes:episode`.
///
/// The guid and the pubDate of each item come from the values the scanner
/// recorded at ingest, and are never re-derived here. The book's
/// `source_mtime` changes on a re-ingest, and a `Refresh` sets it to a
/// sentinel until the watcher finishes. A feed that re-derived from it would
/// publish a new guid for every episode in that window, and a client would
/// then download the whole book again, twice.
pub fn build_channel(book: &FeedBook) -> Channel {
    let items = book
        .episodes
        .iter()
        .map(|ep| {
            let mut item = Item::default();
            item.set_title(ep.title.clone());
            item.set_pub_date(format_rfc2822(ep.pubdate_epoch));

            item.set_enclosure(Enclosure {
                url: ep.audio_url.clone(),
                length: ep.byte_length.to_string(),
                mime_type: ep.mime_type.clone(),
            });
            item.set_guid(Guid {
                value: ep.guid.clone(),
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
    // The newest episode date, so the channel agrees with the items it
    // carries. `source_mtime` is the fallback for a book with no episodes.
    let channel_date = book
        .episodes
        .iter()
        .map(|ep| ep.pubdate_epoch)
        .max()
        .unwrap_or(book.source_mtime);
    channel.set_last_build_date(format_rfc2822(channel_date));
    channel.set_pub_date(format_rfc2822(channel_date));

    let mut ch_it = ITunesChannelExtension::default();
    ch_it.set_author(book.author.clone());
    ch_it.set_image(book.cover_url.clone());
    ch_it.set_summary(book.description.clone());
    // Feeds are private capability URLs. Always ask directories not to list
    // them.
    ch_it.set_block(Some("Yes".to_string()));
    channel.set_itunes_ext(Some(ch_it));

    // The rss crate emits xmlns:itunes automatically when itunes_ext is set;
    // only the podcast namespace needs a declaration here.
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

/// Build, self-check, and render a book's feed. Return the XML only when the
/// feed passes [`selfcheck::check`]. This is the entry point for the server,
/// so that a broken feed is never served.
pub fn render_checked(book: &FeedBook) -> Result<String, Vec<selfcheck::SelfCheckError>> {
    let channel = build_channel(book);
    selfcheck::check(&channel)?;
    Ok(channel.to_string())
}

/// An n-episode `FeedBook` fixture, shared by this file's tests and
/// `selfcheck`'s (the one duplicate-prone literal in the crate).
#[cfg(test)]
pub(crate) fn sample_book(n: usize, mtime: i64) -> FeedBook {
    const ID: &str = "book-1";
    // The scanner's own values, so a fixture cannot drift from an ingest.
    let episodes = (0..n)
        .map(|idx| FeedEpisode {
            idx,
            guid: episode_guid(ID, idx, mtime),
            pubdate_epoch: pubdate_epoch(mtime, idx, n),
            title: format!("Chapter {}", idx + 1),
            audio_url: format!("http://host/audio/book/{:03}.m4a", idx + 1),
            byte_length: 1000 + idx as u64,
            duration_sec: 61.0 * (idx as f64 + 1.0),
            mime_type: "audio/mp4".to_string(),
        })
        .collect();
    FeedBook {
        id: ID.to_string(),
        title: "A Test Book".to_string(),
        author: Some("An Author".to_string()),
        description: Some("A description".to_string()),
        cover_url: Some("http://host/cover.jpg".to_string()),
        source_mtime: mtime,
        self_url: "http://host/feed/book.xml".to_string(),
        episodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_is_stable_and_mtime_sensitive() {
        assert_eq!(episode_guid("b", 0, 100), episode_guid("b", 0, 100));
        assert_ne!(episode_guid("b", 0, 100), episode_guid("b", 0, 101));
        assert_ne!(episode_guid("b", 0, 100), episode_guid("b", 1, 100));
        assert_ne!(episode_guid("a", 0, 100), episode_guid("b", 0, 100));
    }

    #[test]
    fn track_guids_separate_names_that_differ_outside_utf8() {
        // Two Unix filenames that a lossy conversion maps to one string. The
        // guid is the episode table's primary key, so sharing one would make
        // a track overwrite its sibling and vanish from the feed.
        let odd_a: &[u8] = b"track\xff.mp3";
        let odd_b: &[u8] = b"track\xfe.mp3";
        assert_ne!(track_guid("book", odd_a, 7), track_guid("book", odd_b, 7));

        // The file and its own mtime are what the identity follows.
        assert_eq!(
            track_guid("book", b"01.mp3", 7),
            track_guid("book", b"01.mp3", 7)
        );
        assert_ne!(
            track_guid("book", b"01.mp3", 7),
            track_guid("book", b"01.mp3", 8)
        );
        assert_ne!(
            track_guid("book", b"01.mp3", 7),
            track_guid("book", b"02.mp3", 7)
        );
        assert_ne!(track_guid("a", b"01.mp3", 7), track_guid("b", b"01.mp3", 7));

        // A name that is valid UTF-8 hashes exactly `book_id:name:mtime`, so
        // moving to bytes moved nobody's guid.
        assert_eq!(
            track_guid("book", b"01.mp3", 7),
            blake3::hash(b"book:01.mp3:7").to_hex().to_string()
        );
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
    fn items_publish_the_stored_guid_and_pubdate() {
        // A refreshed book: `source_mtime` holds the re-ingest sentinel, and
        // the episodes still carry what the last ingest recorded. The feed
        // must publish the recorded values, so that a client keeps the
        // episodes it already has.
        let mut book = sample_book(3, 1_700_000_000);
        let stored: Vec<(String, i64)> = book
            .episodes
            .iter()
            .map(|ep| (ep.guid.clone(), ep.pubdate_epoch))
            .collect();
        book.source_mtime = -1;

        let channel = build_channel(&book);
        for (item, (guid, epoch)) in channel.items().iter().zip(&stored) {
            assert_eq!(item.guid().expect("guid present").value(), guid);
            assert_eq!(item.pub_date(), Some(format_rfc2822(*epoch)).as_deref());
        }
        // The channel date follows the items, not the sentinel.
        let newest = stored.last().expect("three episodes").1;
        assert_eq!(channel.pub_date(), Some(format_rfc2822(newest)).as_deref());
        assert_eq!(
            channel.last_build_date(),
            Some(format_rfc2822(newest)).as_deref()
        );
        // And the feed still passes the self-check with the sentinel in place.
        render_checked(&book).expect("a refreshed book still renders a valid feed");
    }

    #[test]
    fn channel_items_carry_required_tags() {
        let book = sample_book(3, 1_700_000_000);
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
        let xml = render(&sample_book(2, 1_700_000_000));
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
        let xml = render(&sample_book(4, 1_700_000_000));
        assert_eq!(
            xml.matches("<pubDate>").count(),
            4 + 1,
            "one per item + channel"
        );
    }
}
