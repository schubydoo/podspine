//! `scanner` — orchestrate `prober -> splitter -> index` for one audiobook.
//!
//! [`scan_book`] probes a file, splits its chapters into `<data>/books/<id>/`,
//! and persists a book + episodes to the index. It:
//! - falls back to a single episode for a chapter-less file,
//! - is **idempotent**: an unchanged source that is already fully indexed is not
//!   re-split (guids/pubDates are stable),
//! - **skips DRM-protected input** (AAX/AAXC/`.aa`/`.odm`) with a typed error —
//!   Podspine ships no circumvention (PRD W5).
//!
//! Multi-book library scanning and richer format tiers come later (Tasks 3.1/3.9).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use podspine_feed::{episode_guid, pubdate_epoch};
use podspine_index::{BookRow, EpisodeRow, Index, IndexError};
use podspine_prober::{ProbeError, probe};
use podspine_splitter::{ChapterCut, SplitError, split_book};

/// Extensions we refuse to ingest (DRM). Matched case-insensitively.
const DRM_EXTENSIONS: &[&str] = &["aax", "aaxc", "aa", "odm"];

/// Failure modes of a single-book scan.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The input path is not a regular file.
    #[error("not a file: {0}")]
    NotAFile(PathBuf),
    /// The input is DRM-protected and was skipped.
    #[error("DRM-protected input skipped (Podspine ships no circumvention): {0}")]
    UnsupportedDrm(PathBuf),
    /// The source mtime could not be read.
    #[error("could not read source mtime for {path}: {source}")]
    Mtime {
        /// The path.
        path: PathBuf,
        /// I/O error.
        source: std::io::Error,
    },
    /// Probing failed.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// Splitting failed.
    #[error(transparent)]
    Split(#[from] SplitError),
    /// An index operation failed.
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Scan one audiobook `input` into `index`, writing split episodes under
/// `<data_dir>/books/<id>/`. Returns the persisted [`BookRow`].
pub fn scan_book(input: &Path, data_dir: &Path, index: &Index) -> Result<BookRow, ScanError> {
    if !input.is_file() {
        return Err(ScanError::NotAFile(input.to_path_buf()));
    }
    if is_drm(input) {
        return Err(ScanError::UnsupportedDrm(input.to_path_buf()));
    }

    let id = slugify(&file_stem(input));
    let source_mtime = mtime_epoch(input)?;
    let book_out = data_dir.join("books").join(&id);

    // Idempotency: already indexed at this mtime with all files present -> done,
    // no re-probe / re-split.
    if let Some(existing) = index.get_book(&id)?
        && existing.source_mtime == source_mtime
    {
        let eps = index.episodes_for_book(&id)?;
        if !eps.is_empty() && eps.iter().all(|e| Path::new(&e.file_path).exists()) {
            return Ok(existing);
        }
    }

    let probed = probe(input)?;

    // Chapters -> (cut, title). Chapter-less -> a single episode over the file.
    let specs: Vec<(ChapterCut, String)> = if probed.chapters.is_empty() {
        vec![(
            ChapterCut {
                idx: 0,
                start_sec: 0.0,
                end_sec: probed.duration_sec,
            },
            file_stem(input),
        )]
    } else {
        probed
            .chapters
            .iter()
            .map(|c| {
                (
                    ChapterCut {
                        idx: c.idx,
                        start_sec: c.start_sec,
                        end_sec: c.end_sec,
                    },
                    c.title
                        .clone()
                        .unwrap_or_else(|| format!("Chapter {}", c.idx + 1)),
                )
            })
            .collect()
    };
    let n = specs.len();
    let cuts: Vec<ChapterCut> = specs.iter().map(|(cut, _)| cut.clone()).collect();
    let episodes = split_book(input, &book_out, &cuts)?;

    let book = BookRow {
        id: id.clone(),
        slug: id.clone(),
        title: file_stem(input),
        author: None,
        cover_path: None,
        source_path: input.to_string_lossy().into_owned(),
        source_mtime,
        status: "ready".to_string(),
    };
    index.upsert_book(&book)?;

    for (ep, (_, title)) in episodes.iter().zip(&specs) {
        index.upsert_episode(&EpisodeRow {
            guid: episode_guid(&id, ep.idx, source_mtime),
            book_id: id.clone(),
            idx: ep.idx as i64,
            title: title.clone(),
            file_path: ep.path.to_string_lossy().into_owned(),
            byte_length: ep.byte_length as i64,
            duration_sec: ep.duration_sec,
            pubdate_epoch: pubdate_epoch(source_mtime, ep.idx, n),
        })?;
    }

    Ok(book)
}

/// Whether a path has a DRM extension we refuse to ingest.
fn is_drm(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| DRM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// File stem as a lossy string (fallback `"book"`).
fn file_stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string())
}

/// Lowercase ASCII slug: alphanumerics kept, runs of anything else become a
/// single `-`; falls back to `"book"`.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "book".to_string()
    } else {
        trimmed.to_string()
    }
}

/// File mtime as Unix epoch seconds (0 if before the epoch).
fn mtime_epoch(p: &Path) -> Result<i64, ScanError> {
    let modified = std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map_err(|source| ScanError::Mtime {
            path: p.to_path_buf(),
            source,
        })?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("podspine-scan").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Synthesize an AAC file; `chapters` true embeds three 10s chapters.
    fn synth(dir: &Path, chapters: bool) -> PathBuf {
        let name = if chapters { "chapters" } else { "flat" };
        let input = dir.join(format!("{name}.m4a"));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"]);
        if chapters {
            let meta = dir.join("meta.txt");
            std::fs::write(
                &meta,
                ";FFMETADATA1\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30000\ntitle=Three\n",
            )
            .unwrap();
            cmd.arg("sine=frequency=440:duration=30")
                .arg("-i")
                .arg(&meta)
                .args(["-map_metadata", "1", "-map", "0:a", "-c:a", "aac"]);
        } else {
            cmd.arg("sine=frequency=330:duration=12")
                .args(["-c:a", "aac"]);
        }
        let status = cmd.arg(&input).status().expect("spawn ffmpeg");
        assert!(status.success(), "ffmpeg synth failed");
        input
    }

    #[test]
    fn slugify_cases() {
        assert_eq!(slugify("A Book - Title!"), "a-book-title");
        assert_eq!(slugify("  spaced  "), "spaced");
        assert_eq!(slugify("***"), "book");
    }

    #[test]
    fn drm_input_is_skipped() {
        let dir = scratch("drm");
        let f = dir.join("audible.aax");
        std::fs::write(&f, b"drm").unwrap();
        let index = Index::open_in_memory().unwrap();
        assert!(matches!(
            scan_book(&f, &dir, &index),
            Err(ScanError::UnsupportedDrm(_))
        ));
    }

    #[test]
    fn missing_file_is_reported() {
        let index = Index::open_in_memory().unwrap();
        let err = scan_book(Path::new("/no/such/file.m4b"), Path::new("/tmp"), &index).unwrap_err();
        assert!(matches!(err, ScanError::NotAFile(_)));
    }

    #[test]
    fn scans_chapters_into_the_index_and_is_idempotent() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("chapters");
        let input = synth(&dir, true);
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();
        assert_eq!(book.status, "ready");

        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 3, "3 chapters -> 3 episodes");
        // idx order, positive sizes, files on disk, strictly increasing pubDates.
        for (i, e) in eps.iter().enumerate() {
            assert_eq!(e.idx, i as i64);
            assert!(e.byte_length > 0);
            assert!(Path::new(&e.file_path).exists());
            assert_eq!(e.guid, episode_guid(&book.id, i, book.source_mtime));
        }
        for w in eps.windows(2) {
            assert!(w[0].pubdate_epoch < w[1].pubdate_epoch, "pubDates increase");
        }

        // Re-scan: idempotent, and no re-split (episode file mtime unchanged).
        let ep0 = PathBuf::from(&eps[0].file_path);
        let m1 = std::fs::metadata(&ep0).unwrap().modified().unwrap();
        let book2 = scan_book(&input, &data, &index).unwrap();
        assert_eq!(book2, book);
        assert_eq!(index.episodes_for_book(&book.id).unwrap().len(), 3);
        let m2 = std::fs::metadata(&ep0).unwrap().modified().unwrap();
        assert_eq!(m1, m2, "unchanged source must not be re-split");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chapterless_file_becomes_a_single_episode() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flat");
        let input = synth(&dir, false);
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1, "chapter-less -> single episode");
        assert!(Path::new(&eps[0].file_path).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
