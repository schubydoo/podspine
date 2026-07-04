//! `scanner` — orchestrate `prober -> splitter -> index`.
//!
//! [`scan_book`] probes one audio file, splits its chapters into
//! `<data>/books/<id>/`, and persists a book + episodes to the index. It:
//! - falls back to a single episode for a chapter-less file,
//! - is **idempotent**: an unchanged source that is already fully indexed is not
//!   re-split (guids/pubDates are stable),
//! - **skips DRM-protected input** (AAX/AAXC/`.aa`/`.odm`) with a typed error —
//!   Podspine ships no circumvention (PRD W5).
//!
//! [`scan_library`] walks a library root of many audiobooks (Task 3.1): each
//! top-level audio file and each per-book subfolder becomes one independent
//! book. It distinguishes single-file books (`.m4b`/`.m4a`, or a lone `.mp3`)
//! from multi-track MP3 folders (recognized here, ingested in Task 3.3), assigns
//! collision-free slugs deterministically, and never lets one bad book abort the
//! whole scan. Richer format tiers come later (Task 3.9).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use podspine_feed::{episode_guid, pubdate_epoch};
use podspine_index::{BookRow, EpisodeRow, Index, IndexError};
use podspine_prober::{ProbeError, probe};
use podspine_splitter::{ChapterCut, SplitError, extract_cover, split_book};

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

/// Scan one audiobook `input` into `index` under a slug derived from its file
/// name. Convenience wrapper over [`scan_book_as`] for single-book callers.
pub fn scan_book(input: &Path, data_dir: &Path, index: &Index) -> Result<BookRow, ScanError> {
    let id = slugify(&file_stem(input));
    scan_book_as(input, &id, data_dir, index)
}

/// Scan one audiobook `input` into `index` under the explicit `id` (also used as
/// the slug), writing split episodes under `<data_dir>/books/<id>/`. Returns the
/// persisted [`BookRow`]. The library scanner uses this to assign collision-free
/// slugs; single-book callers should use [`scan_book`].
pub fn scan_book_as(
    input: &Path,
    id: &str,
    data_dir: &Path,
    index: &Index,
) -> Result<BookRow, ScanError> {
    if !input.is_file() {
        return Err(ScanError::NotAFile(input.to_path_buf()));
    }
    if is_drm(input) {
        return Err(ScanError::UnsupportedDrm(input.to_path_buf()));
    }

    let id = id.to_string();
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

    // Extract the embedded cover, if any. A missing cover is a normal case, and
    // an extraction failure never fails the book — we just serve no cover art.
    let cover_path = if probed.has_cover {
        let ext = cover_ext(probed.cover_codec.as_deref());
        match extract_cover(input, &book_out, ext) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(err) => {
                tracing::warn!(error = %err, id = %id, "cover extraction failed; serving no cover");
                None
            }
        }
    } else {
        None
    };

    let book = BookRow {
        id: id.clone(),
        slug: id.clone(),
        title: file_stem(input),
        author: None,
        cover_path,
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

/// Outcome of a library scan (counts only — a library of thousands of books is
/// never held in memory; each is indexed and dropped in turn, NFR-P4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanSummary {
    /// Books successfully indexed.
    pub indexed: usize,
    /// Sources skipped: bad/DRM'd files, or MP3 folders pending Task 3.3.
    pub skipped: usize,
}

/// A discovered book source within the library.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BookSource {
    /// A single splittable audio file (`.m4b`/`.m4a`, or a lone `.mp3`).
    File(PathBuf),
    /// A folder of per-track MP3s — recognized in v1, ingested in Task 3.3.
    Mp3Folder(PathBuf),
}

impl BookSource {
    /// The base name a slug is derived from (file stem, or folder name).
    fn base_name(&self) -> String {
        match self {
            BookSource::File(p) => file_stem(p),
            // A folder name has no extension to strip; use it whole.
            BookSource::Mp3Folder(d) => d
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "book".to_string()),
        }
    }
}

/// Audio extensions we recognize at the library level. DRM is rejected deeper,
/// in [`scan_book_as`]; richer tiers (Ogg/Opus/FLAC) arrive in Task 3.9.
const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3"];

/// Scan a library root of many audiobooks into `index`, writing each book's
/// episodes under `<data_dir>/books/<slug>/`. One independent book per top-level
/// audio file or per-book subfolder. Slugs are collision-free and deterministic
/// across re-scans; a single failing book is logged and skipped, never fatal.
pub fn scan_library(library: &Path, data_dir: &Path, index: &Index) -> ScanSummary {
    let sources = discover(library);

    let mut seen = HashSet::new();
    let mut summary = ScanSummary::default();
    for source in sources {
        // Reserve a slug for every candidate in deterministic order so a book's
        // slug is stable across re-scans regardless of siblings' outcomes.
        let slug = unique_slug(&slugify(&source.base_name()), &mut seen);
        match source {
            BookSource::File(path) => match scan_book_as(&path, &slug, data_dir, index) {
                Ok(book) => {
                    summary.indexed += 1;
                    tracing::info!(slug = %book.slug, title = %book.title, "indexed book");
                }
                Err(err) => {
                    summary.skipped += 1;
                    tracing::warn!(error = %err, path = %path.display(), "skipped");
                }
            },
            BookSource::Mp3Folder(dir) => {
                summary.skipped += 1;
                tracing::info!(
                    path = %dir.display(),
                    "recognized MP3-folder book; ingest lands in Task 3.3, skipping for now"
                );
            }
        }
    }
    tracing::info!(
        indexed = summary.indexed,
        skipped = summary.skipped,
        "library scan complete"
    );
    summary
}

/// Discover book sources one level under `library`, in a deterministic
/// (path-sorted) order so slug disambiguation is stable across re-scans.
fn discover(library: &Path) -> Vec<BookSource> {
    let mut entries = match std::fs::read_dir(library) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect::<Vec<_>>(),
        Err(err) => {
            tracing::error!(error = %err, library = %library.display(), "cannot read library");
            return Vec::new();
        }
    };
    entries.sort();

    let mut sources = Vec::new();
    for path in entries {
        if path.is_file() && is_audio(&path) {
            sources.push(BookSource::File(path));
        } else if path.is_dir()
            && let Some(src) = classify_dir(&path)
        {
            sources.push(src);
        }
    }
    sources
}

/// Classify a per-book subfolder: prefer a splittable `.m4b`/`.m4a`; a lone
/// `.mp3` is a single-file book; several `.mp3`s are a multi-track folder
/// (Task 3.3). A folder with no audio yields nothing.
fn classify_dir(dir: &Path) -> Option<BookSource> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut m4x = Vec::new();
    let mut mp3 = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if !path.is_file() {
            continue;
        }
        match ext_lower(&path).as_deref() {
            Some("m4b") | Some("m4a") => m4x.push(path),
            Some("mp3") => mp3.push(path),
            _ => {}
        }
    }
    m4x.sort();
    mp3.sort();

    if let Some(f) = m4x.into_iter().next() {
        Some(BookSource::File(f))
    } else if mp3.len() == 1 {
        Some(BookSource::File(mp3.into_iter().next().unwrap()))
    } else if !mp3.is_empty() {
        Some(BookSource::Mp3Folder(dir.to_path_buf()))
    } else {
        None
    }
}

/// Reserve `base` if free, else `base-2`, `base-3`, … Inserts the chosen slug
/// into `seen` and returns it.
fn unique_slug(base: &str, seen: &mut HashSet<String>) -> String {
    if seen.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if seen.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Whether a path has a recognized top-level audio extension.
fn is_audio(p: &Path) -> bool {
    ext_lower(p)
        .map(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}

/// A path's extension, lowercased.
fn ext_lower(p: &Path) -> Option<String> {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// File extension for an extracted cover, from its ffprobe codec name. Cover art
/// is almost always MJPEG or PNG; anything else defaults to `jpg`.
fn cover_ext(codec: Option<&str>) -> &'static str {
    match codec {
        Some("png") => "png",
        _ => "jpg", // mjpeg/mjpg/jpeg and any unknown codec
    }
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

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// Synthesize an AAC file with an embedded (attached-picture) cover.
    fn synth_with_cover(dir: &Path) -> PathBuf {
        let input = dir.join("cover.m4a");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=120x120:d=0.1",
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-frames:v",
                "1",
                "-c:a",
                "aac",
                "-c:v",
                "mjpeg",
                "-disposition:v:0",
                "attached_pic",
            ])
            .arg(&input)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "ffmpeg cover synth failed");
        input
    }

    #[test]
    fn cover_ext_by_codec() {
        assert_eq!(cover_ext(Some("mjpeg")), "jpg");
        assert_eq!(cover_ext(Some("jpeg")), "jpg");
        assert_eq!(cover_ext(Some("png")), "png");
        assert_eq!(cover_ext(None), "jpg");
    }

    #[test]
    fn scans_and_extracts_an_embedded_cover() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("cover");
        let input = synth_with_cover(&dir);
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();
        let cover = book.cover_path.expect("cover extracted");
        assert!(cover.ends_with("cover.jpg"), "got {cover}");
        let meta = std::fs::metadata(&cover).expect("cover file on disk");
        assert!(meta.len() > 0, "cover file non-empty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_slug_disambiguates_collisions() {
        let mut seen = HashSet::new();
        assert_eq!(unique_slug("dune", &mut seen), "dune");
        assert_eq!(unique_slug("dune", &mut seen), "dune-2");
        assert_eq!(unique_slug("dune", &mut seen), "dune-3");
        assert_eq!(unique_slug("other", &mut seen), "other");
    }

    #[test]
    fn discover_finds_files_and_folders_sorted() {
        let root = scratch("discover");
        // Top-level single-file book, plus per-book folders of each kind.
        touch(&root.join("Top Book.m4b"));
        touch(&root.join("a-m4b-book/book.m4b"));
        touch(&root.join("a-m4b-book/cover.jpg")); // ignored non-audio sibling
        touch(&root.join("mp3-single/only.mp3")); // lone mp3 -> single-file book
        touch(&root.join("mp3-multi/01.mp3"));
        touch(&root.join("mp3-multi/02.mp3")); // several mp3s -> folder book
        touch(&root.join("empty-folder/readme.txt")); // no audio -> ignored

        let found = discover(&root);
        // Path-sorted: "Top Book.m4b" < "a-m4b-book" < "mp3-multi" < "mp3-single".
        assert_eq!(
            found,
            vec![
                BookSource::File(root.join("Top Book.m4b")),
                BookSource::File(root.join("a-m4b-book/book.m4b")),
                BookSource::Mp3Folder(root.join("mp3-multi")),
                BookSource::File(root.join("mp3-single/only.mp3")),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_scan_disambiguates_same_named_books() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        // Two books that slugify identically, in separate folders.
        let root = scratch("dup-lib");
        let b1 = synth(
            &{
                let d = root.join("Dune");
                std::fs::create_dir_all(&d).unwrap();
                d
            },
            false,
        );
        std::fs::rename(&b1, root.join("Dune/Dune.m4a")).unwrap();
        let b2 = synth(
            &{
                let d = root.join("dune");
                std::fs::create_dir_all(&d).unwrap();
                d
            },
            false,
        );
        std::fs::rename(&b2, root.join("dune/Dune.m4a")).unwrap();

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index);

        assert_eq!(summary.indexed, 2, "both books indexed");
        assert_eq!(summary.skipped, 0);
        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 2, "no clobber: two distinct rows");
        let slugs: HashSet<_> = books.iter().map(|b| b.slug.clone()).collect();
        assert!(
            slugs.contains("dune") && slugs.contains("dune-2"),
            "got {slugs:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_scan_skips_bad_books_without_aborting() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("mixed-lib");
        synth(&root, true); // chapters.m4a at the top level (the good book)
        std::fs::write(root.join("broken.m4a"), b"not really audio").unwrap();
        touch(&root.join("mp3-multi/01.mp3"));
        touch(&root.join("mp3-multi/02.mp3"));

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index);

        assert_eq!(summary.indexed, 1, "only the good book");
        assert_eq!(summary.skipped, 2, "unprobeable file + MP3 folder skipped");
        assert_eq!(index.list_books().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&root);
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
