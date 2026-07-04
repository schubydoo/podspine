//! `chapters` — resolve where a book's chapters come from.
//!
//! A companion **sidecar** beside the audio file takes precedence over embedded
//! chapters (PRD §3.0, Task 3.8). v1 supports two sidecar formats, in priority
//! order:
//! 1. **`.cue`** — `INDEX 01 mm:ss:ff` timestamps at **75 frames/sec** (the
//!    default when present).
//! 2. **`.ffmeta` / `.ffmetadata`** — ffmpeg's metadata format (`[CHAPTER]`
//!    blocks with `TIMEBASE`/`START`/`END`/`title`).
//! 3. **Embedded** chapters (from `ffprobe`) — the fallback.
//!
//! `.opf` / `.nfo` / OverDrive `.odm` are metadata/manifest files and are
//! **never** treated as chapter sources. A config override can force embedded
//! chapters even when a sidecar exists.
//!
//! Parsing is pure (string in, chapters out) so it is unit-tested without files.

use std::fs;
use std::path::{Path, PathBuf};

use podspine_prober::Chapter;

/// Where the resolved chapters came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterSource {
    /// Embedded markers read by ffprobe (the fallback).
    Embedded,
    /// A `.cue` sidecar.
    Cue,
    /// A `.ffmeta` / `.ffmetadata` sidecar.
    Ffmeta,
}

/// The outcome of chapter resolution.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Which source won.
    pub source: ChapterSource,
    /// Chapters in order, `idx` renumbered 0-based.
    pub chapters: Vec<Chapter>,
}

/// Resolve the chapters for `audio`. When `force_embedded` is false, a sibling
/// `.cue` (then `.ffmeta`/`.ffmetadata`) that parses to at least one chapter is
/// preferred over `embedded`; otherwise `embedded` is returned as-is.
/// `duration_sec` provides the end time of the last `.cue` chapter (which has no
/// explicit end).
pub fn resolve(
    audio: &Path,
    embedded: &[Chapter],
    duration_sec: f64,
    force_embedded: bool,
) -> Resolved {
    if !force_embedded {
        if let Some(text) = sidecar(audio, &["cue"]) {
            let chapters = parse_cue(&text, duration_sec);
            if !chapters.is_empty() {
                return Resolved {
                    source: ChapterSource::Cue,
                    chapters,
                };
            }
        }
        if let Some(text) = sidecar(audio, &["ffmeta", "ffmetadata"]) {
            let chapters = parse_ffmeta(&text);
            if !chapters.is_empty() {
                return Resolved {
                    source: ChapterSource::Ffmeta,
                    chapters,
                };
            }
        }
    }
    Resolved {
        source: ChapterSource::Embedded,
        chapters: renumber(embedded.to_vec()),
    }
}

/// Read the first existing sibling sidecar of `audio` with one of `exts`
/// (case-insensitive extension), e.g. `book.m4b` -> `book.cue`.
fn sidecar(audio: &Path, exts: &[&str]) -> Option<String> {
    for ext in exts {
        for cased in [ext.to_string(), ext.to_ascii_uppercase()] {
            let candidate: PathBuf = audio.with_extension(&cased);
            if candidate.is_file()
                && let Ok(text) = fs::read_to_string(&candidate)
            {
                return Some(text);
            }
        }
    }
    None
}

/// Renumber chapters' `idx` to 0-based file order (parsers build order; embedded
/// already is, but this keeps the contract uniform).
fn renumber(mut chapters: Vec<Chapter>) -> Vec<Chapter> {
    for (i, c) in chapters.iter_mut().enumerate() {
        c.idx = i;
    }
    chapters
}

/// Parse a `.cue` sheet into chapters. Each `TRACK` becomes one chapter, its
/// start from `INDEX 01 mm:ss:ff` (75 frames/sec); the end is the next track's
/// start, and the last track ends at `duration_sec`. Tracks without an
/// `INDEX 01` are skipped.
pub fn parse_cue(text: &str, duration_sec: f64) -> Vec<Chapter> {
    struct Track {
        start: f64,
        title: Option<String>,
    }
    let mut tracks: Vec<Track> = Vec::new();
    let mut in_track = false;

    for line in text.lines() {
        let t = line.trim();
        let upper = t.to_ascii_uppercase();
        if upper.starts_with("TRACK ") {
            in_track = true;
            tracks.push(Track {
                start: f64::NAN,
                title: None,
            });
        } else if in_track
            && upper.starts_with("TITLE ")
            && let Some(cur) = tracks.last_mut()
        {
            cur.title = cue_quoted(t);
        } else if in_track
            && upper.starts_with("INDEX 01 ")
            && let (Some(cur), Some(secs)) = (tracks.last_mut(), cue_index_secs(t))
        {
            cur.start = secs;
        }
    }

    // Keep only tracks that actually got an INDEX 01, in order.
    let mut starts: Vec<(f64, Option<String>)> = tracks
        .into_iter()
        .filter(|tr| tr.start.is_finite())
        .map(|tr| (tr.start, tr.title))
        .collect();
    starts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let n = starts.len();
    starts
        .iter()
        .enumerate()
        .map(|(i, (start, title))| {
            let end = if i + 1 < n {
                starts[i + 1].0
            } else {
                duration_sec.max(*start)
            };
            Chapter {
                idx: i,
                start_sec: *start,
                end_sec: end,
                title: title.clone().filter(|s| !s.is_empty()),
            }
        })
        .collect()
}

/// The quoted (or bare) argument of a `.cue` `TITLE`/`PERFORMER` line.
fn cue_quoted(line: &str) -> Option<String> {
    let rest = line.split_once(char::is_whitespace)?.1.trim();
    let unquoted = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
    Some(unquoted.unwrap_or(rest).to_string())
}

/// Parse `mm:ss:ff` (frames at 75/sec) from an `INDEX 01 mm:ss:ff` line.
fn cue_index_secs(line: &str) -> Option<f64> {
    let ts = line.split_whitespace().nth(2)?;
    let mut parts = ts.split(':');
    let mm: f64 = parts.next()?.parse().ok()?;
    let ss: f64 = parts.next()?.parse().ok()?;
    let ff: f64 = parts.next()?.parse().ok()?;
    Some(mm * 60.0 + ss + ff / 75.0)
}

/// Parse an ffmpeg-metadata (`;FFMETADATA1`) sidecar's `[CHAPTER]` blocks. Each
/// carries a `TIMEBASE=num/den` scaling `START`/`END` to seconds, plus `title`.
pub fn parse_ffmeta(text: &str) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    let mut in_chapter = false;
    let (mut timebase, mut start, mut end, mut title) = (1.0 / 1000.0, None, None, None);

    let flush = |timebase: f64,
                 start: &mut Option<i64>,
                 end: &mut Option<i64>,
                 title: &mut Option<String>,
                 out: &mut Vec<Chapter>| {
        if let (Some(s), Some(e)) = (start.take(), end.take()) {
            out.push(Chapter {
                idx: out.len(),
                start_sec: s as f64 * timebase,
                end_sec: e as f64 * timebase,
                title: title.take().filter(|t: &String| !t.is_empty()),
            });
        }
    };

    for line in text.lines() {
        let t = line.trim();
        if t.eq_ignore_ascii_case("[CHAPTER]") {
            if in_chapter {
                flush(timebase, &mut start, &mut end, &mut title, &mut chapters);
            }
            in_chapter = true;
            timebase = 1.0 / 1000.0;
            continue;
        }
        if t.starts_with('[') {
            // A different section (e.g. [STREAM]) ends any open chapter.
            if in_chapter {
                flush(timebase, &mut start, &mut end, &mut title, &mut chapters);
                in_chapter = false;
            }
            continue;
        }
        if !in_chapter {
            continue;
        }
        if let Some((key, val)) = t.split_once('=') {
            match key.trim().to_ascii_uppercase().as_str() {
                "TIMEBASE" => {
                    if let Some((num, den)) = val.trim().split_once('/')
                        && let (Ok(n), Ok(d)) = (num.parse::<f64>(), den.parse::<f64>())
                        && d != 0.0
                    {
                        timebase = n / d;
                    }
                }
                "START" => start = val.trim().parse::<i64>().ok(),
                "END" => end = val.trim().parse::<i64>().ok(),
                "TITLE" => title = Some(val.trim().to_string()),
                _ => {}
            }
        }
    }
    if in_chapter {
        flush(timebase, &mut start, &mut end, &mut title, &mut chapters);
    }
    chapters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded() -> Vec<Chapter> {
        vec![Chapter {
            idx: 0,
            start_sec: 0.0,
            end_sec: 100.0,
            title: Some("Embedded".into()),
        }]
    }

    #[test]
    fn cue_index_75_frames_per_second() {
        // 01:00:37 -> 60 + 37/75 = 60.4933...
        assert!((cue_index_secs("INDEX 01 01:00:37").unwrap() - 60.4933333).abs() < 1e-6);
        assert_eq!(cue_index_secs("INDEX 01 00:00:00"), Some(0.0));
    }

    #[test]
    fn parse_cue_tracks_titles_and_ends() {
        let cue = r#"
TITLE "The Album"
FILE "book.m4b" WAVE
  TRACK 01 AUDIO
    TITLE "Chapter One"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Chapter Two"
    INDEX 01 30:00:00
"#;
        let ch = parse_cue(cue, 3600.0);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].idx, 0);
        assert_eq!(ch[0].start_sec, 0.0);
        assert_eq!(ch[0].end_sec, 1800.0, "ends where track 2 begins");
        assert_eq!(ch[0].title.as_deref(), Some("Chapter One"));
        assert_eq!(ch[1].start_sec, 1800.0);
        assert_eq!(ch[1].end_sec, 3600.0, "last chapter ends at duration");
        assert_eq!(ch[1].title.as_deref(), Some("Chapter Two"));
    }

    #[test]
    fn parse_ffmeta_chapters_with_timebase() {
        let meta = ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=25000\ntitle=Two\n";
        let ch = parse_ffmeta(meta);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].start_sec, 0.0);
        assert_eq!(ch[0].end_sec, 10.0);
        assert_eq!(ch[0].title.as_deref(), Some("One"));
        assert_eq!(ch[1].start_sec, 10.0);
        assert_eq!(ch[1].end_sec, 25.0);
    }

    #[test]
    fn resolve_prefers_cue_then_ffmeta_then_embedded() {
        let dir = std::env::temp_dir().join("podspine-chapters-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("book.m4b");
        std::fs::write(&audio, b"not real audio").unwrap();

        // No sidecar -> embedded.
        let r = resolve(&audio, &embedded(), 100.0, false);
        assert_eq!(r.source, ChapterSource::Embedded);

        // .ffmeta present -> ffmeta.
        std::fs::write(
            dir.join("book.ffmeta"),
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=5000\ntitle=A\n",
        )
        .unwrap();
        let r = resolve(&audio, &embedded(), 100.0, false);
        assert_eq!(r.source, ChapterSource::Ffmeta);

        // .cue present -> cue wins over ffmeta (higher priority).
        std::fs::write(
            dir.join("book.cue"),
            "TRACK 01 AUDIO\n  TITLE \"C\"\n  INDEX 01 00:00:00\n",
        )
        .unwrap();
        let r = resolve(&audio, &embedded(), 100.0, false);
        assert_eq!(r.source, ChapterSource::Cue);
        assert_eq!(r.chapters.len(), 1);

        // force_embedded overrides even with sidecars present.
        let r = resolve(&audio, &embedded(), 100.0, true);
        assert_eq!(r.source, ChapterSource::Embedded);
        assert_eq!(r.chapters[0].title.as_deref(), Some("Embedded"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opf_nfo_odm_are_never_chapter_sources() {
        let dir = std::env::temp_dir().join("podspine-chapters-nonsources");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("book.m4b");
        std::fs::write(&audio, b"x").unwrap();
        for ext in ["opf", "nfo", "odm"] {
            std::fs::write(dir.join(format!("book.{ext}")), b"<metadata/>").unwrap();
        }
        // Only metadata/manifest siblings exist -> falls back to embedded.
        let r = resolve(&audio, &embedded(), 100.0, false);
        assert_eq!(r.source, ChapterSource::Embedded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_ffmeta_handles_sections_and_edge_keys() {
        // A trailing [STREAM] section ends the open chapter; an unknown key is
        // ignored; a zero-denominator TIMEBASE is rejected (default 1/1000
        // stays); a chapter with no END is dropped.
        let meta = ";FFMETADATA1\n\
            [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\ntitle=One\nARTIST=ignored\n\
            [CHAPTER]\nTIMEBASE=0/0\nSTART=2000\nEND=6000\ntitle=Two\n\
            [CHAPTER]\nSTART=6000\ntitle=NoEnd\n\
            [STREAM]\ncodec=aac\n";
        let ch = parse_ffmeta(meta);
        assert_eq!(ch.len(), 2, "the END-less chapter is dropped");
        assert_eq!(ch[0].end_sec, 2.0);
        assert_eq!(ch[1].start_sec, 2.0); // TIMEBASE 0/0 rejected -> 1/1000 default
        assert_eq!(ch[1].end_sec, 6.0);
    }

    #[test]
    fn resolve_falls_through_an_empty_cue_to_ffmeta() {
        let dir = std::env::temp_dir().join("podspine-chapters-fallthrough");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("book.m4b");
        std::fs::write(&audio, b"x").unwrap();
        // A .cue with no INDEX parses to zero chapters -> fall through to .ffmeta.
        std::fs::write(dir.join("book.cue"), "REM nothing usable here\n").unwrap();
        std::fs::write(
            dir.join("book.ffmeta"),
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=5000\ntitle=A\n",
        )
        .unwrap();
        let r = resolve(&audio, &embedded(), 100.0, false);
        assert_eq!(r.source, ChapterSource::Ffmeta);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
