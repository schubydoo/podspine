//! Feed self-check — validate a built [`rss::Channel`] before it is ever served.
//!
//! This guards the failure modes that make podcast apps misbehave: episodes out
//! of order (non-monotonic pubDates), a missing `enclosure length`, or a missing
//! `itunes:duration`/`itunes:episode`. It operates on the *rendered* channel (not
//! the domain input) so it catches builder bugs too, and it parses pubDates back
//! to real timestamps so the ordering check is genuine. (PRD S4, TAD §4.)

use rss::Channel;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

/// A specific reason a feed failed self-check. `idx` is the zero-based item
/// position; `episode` is the `itunes:episode` number.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelfCheckError {
    /// The channel has no items.
    #[error("feed has no items")]
    NoItems,
    /// An item has no `<enclosure>`.
    #[error("item {idx} has no enclosure")]
    MissingEnclosure {
        /// Item position.
        idx: usize,
    },
    /// An item's `<enclosure>` has an empty or zero `length`.
    #[error("item {idx} has a missing/zero enclosure length")]
    MissingEnclosureLength {
        /// Item position.
        idx: usize,
    },
    /// An item's `<enclosure>` has an empty `url`.
    #[error("item {idx} has an empty enclosure url")]
    MissingEnclosureUrl {
        /// Item position.
        idx: usize,
    },
    /// An item is missing a parseable `<itunes:episode>`.
    #[error("item {idx} has no itunes:episode")]
    MissingItunesEpisode {
        /// Item position.
        idx: usize,
    },
    /// An item is missing a non-empty `<itunes:duration>`.
    #[error("item {idx} has no itunes:duration")]
    MissingItunesDuration {
        /// Item position.
        idx: usize,
    },
    /// An item's `<pubDate>` is missing or not a valid RFC 2822 date.
    #[error("item {idx} has a missing/invalid pubDate ({value:?})")]
    BadPubDate {
        /// Item position.
        idx: usize,
        /// The raw pubDate string, if any.
        value: String,
    },
    /// Ordered by `itunes:episode`, pubDates are not strictly increasing — the
    /// classic "episodes play out of order" bug.
    #[error("pubDate for episode {episode} is not later than the previous episode")]
    NonMonotonicPubDates {
        /// The episode whose pubDate breaks the strictly-increasing order.
        episode: i64,
    },
}

/// A passing self-check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheckReport {
    /// Number of items validated.
    pub items_checked: usize,
}

/// Validate a channel. `Ok` if every required tag is present and pubDates are
/// strictly increasing by episode; otherwise `Err` with *all* problems found.
pub fn check(channel: &Channel) -> Result<SelfCheckReport, Vec<SelfCheckError>> {
    let items = channel.items();
    if items.is_empty() {
        return Err(vec![SelfCheckError::NoItems]);
    }

    let mut errors = Vec::new();
    // (episode, pubdate_epoch) for the monotonicity check — only items that have
    // both a parseable episode and pubDate contribute.
    let mut ordered: Vec<(i64, i64)> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        match item.enclosure() {
            None => errors.push(SelfCheckError::MissingEnclosure { idx }),
            Some(enc) => {
                let len = enc.length().trim();
                if len.is_empty() || len == "0" {
                    errors.push(SelfCheckError::MissingEnclosureLength { idx });
                }
                if enc.url().trim().is_empty() {
                    errors.push(SelfCheckError::MissingEnclosureUrl { idx });
                }
            }
        }

        let itunes = item.itunes_ext();
        let episode = itunes
            .and_then(|e| e.episode())
            .and_then(|s| s.trim().parse::<i64>().ok());
        if episode.is_none() {
            errors.push(SelfCheckError::MissingItunesEpisode { idx });
        }
        let has_duration = itunes
            .and_then(|e| e.duration())
            .is_some_and(|d| !d.trim().is_empty());
        if !has_duration {
            errors.push(SelfCheckError::MissingItunesDuration { idx });
        }

        match item.pub_date().and_then(parse_rfc2822) {
            Some(epoch) => {
                if let Some(ep) = episode {
                    ordered.push((ep, epoch));
                }
            }
            None => errors.push(SelfCheckError::BadPubDate {
                idx,
                value: item.pub_date().unwrap_or_default().to_string(),
            }),
        }
    }

    // Strictly-increasing pubDates when ordered by episode number.
    ordered.sort_by_key(|(episode, _)| *episode);
    for win in ordered.windows(2) {
        let (_, prev_epoch) = win[0];
        let (episode, epoch) = win[1];
        if epoch <= prev_epoch {
            errors.push(SelfCheckError::NonMonotonicPubDates { episode });
        }
    }

    if errors.is_empty() {
        Ok(SelfCheckReport {
            items_checked: items.len(),
        })
    } else {
        Err(errors)
    }
}

/// Parse an RFC 2822 date to a Unix timestamp.
fn parse_rfc2822(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc2822)
        .ok()
        .map(|dt| dt.unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FeedBook, FeedEpisode, build_channel};
    use rss::Channel;

    fn sample(n: usize) -> FeedBook {
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
            source_mtime: 1_700_000_000,
            self_url: "http://host/feed/book.xml".to_string(),
            episodes,
        }
    }

    fn has(errors: &[SelfCheckError], want: impl Fn(&SelfCheckError) -> bool) -> bool {
        errors.iter().any(want)
    }

    #[test]
    fn valid_feed_passes() {
        let channel = build_channel(&sample(3));
        let report = check(&channel).expect("valid feed passes");
        assert_eq!(report.items_checked, 3);
    }

    #[test]
    fn empty_channel_is_rejected() {
        let errors = check(&Channel::default()).unwrap_err();
        assert_eq!(errors, vec![SelfCheckError::NoItems]);
    }

    #[test]
    fn broken_pubdate_order_is_rejected() {
        let mut channel = build_channel(&sample(3));
        let mut items = channel.items().to_vec();
        // Make episode 2 (item idx 1) far older than episode 1 -> out of order.
        items[1].set_pub_date(Some("Thu, 01 Jan 1970 00:00:00 +0000".to_string()));
        channel.set_items(items);

        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::NonMonotonicPubDates { episode: 2 }
            )),
            "expected NonMonotonicPubDates for episode 2, got {errors:?}"
        );
    }

    #[test]
    fn missing_enclosure_length_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        let mut enc = items[0].enclosure().unwrap().clone();
        enc.length = String::new();
        items[0].set_enclosure(enc);
        channel.set_items(items);

        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::MissingEnclosureLength { idx: 0 }
            )),
            "expected MissingEnclosureLength at idx 0, got {errors:?}"
        );
    }

    #[test]
    fn missing_itunes_duration_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        let mut it = items[0].itunes_ext().unwrap().clone();
        it.set_duration(None);
        items[0].set_itunes_ext(Some(it));
        channel.set_items(items);

        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::MissingItunesDuration { idx: 0 }
            )),
            "expected MissingItunesDuration at idx 0, got {errors:?}"
        );
    }

    #[test]
    fn render_checked_returns_xml_for_a_valid_book() {
        let xml = crate::render_checked(&sample(2)).expect("valid book renders");
        assert!(xml.contains("<itunes:duration>"));
    }

    #[test]
    fn missing_enclosure_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        items[0].set_enclosure(None::<rss::Enclosure>);
        channel.set_items(items);
        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::MissingEnclosure { idx: 0 }
            )),
            "{errors:?}"
        );
    }

    #[test]
    fn empty_enclosure_url_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        let mut enc = items[0].enclosure().unwrap().clone();
        enc.url = String::new();
        items[0].set_enclosure(enc);
        channel.set_items(items);
        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::MissingEnclosureUrl { idx: 0 }
            )),
            "{errors:?}"
        );
    }

    #[test]
    fn missing_itunes_episode_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        let mut it = items[0].itunes_ext().unwrap().clone();
        it.set_episode(None::<String>);
        items[0].set_itunes_ext(Some(it));
        channel.set_items(items);
        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::MissingItunesEpisode { idx: 0 }
            )),
            "{errors:?}"
        );
    }

    #[test]
    fn invalid_pubdate_is_rejected() {
        let mut channel = build_channel(&sample(2));
        let mut items = channel.items().to_vec();
        items[0].set_pub_date(Some("not a real date".to_string()));
        channel.set_items(items);
        let errors = check(&channel).unwrap_err();
        assert!(
            has(&errors, |e| matches!(
                e,
                SelfCheckError::BadPubDate { idx: 0, .. }
            )),
            "{errors:?}"
        );
    }
}
