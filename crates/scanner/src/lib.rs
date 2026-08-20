//! The `scanner` crate runs the ingest pipeline: `prober` -> `splitter` -> `index`.
//!
//! [`scan_book`] ingests one audio file. It probes the file, resolves the
//! chapter source, splits the chapters into `<data>/books/<id>/`, and persists
//! one book and its episodes to the index. A sibling `.cue`/`.ffmeta` sidecar
//! wins over embedded markers unless `force_embedded` is set (Task 3.8).
//! Three more rules apply:
//! - A file with no chapters becomes a single episode.
//! - The scan is **idempotent**. If an unchanged source is already fully
//!   indexed, the scanner does not split it again. The `guid`s and `pubDate`s
//!   stay stable.
//! - The scanner **skips DRM-protected input** (AAX/AAXC/`.aa`/`.odm`) and
//!   returns a typed error. Podspine ships no circumvention (PRD W5).
//!
//! [`scan_library`] walks a library root that holds many audiobooks (Task 3.1).
//! Each top-level audio file and each per-book subfolder becomes one
//! independent book. Books have two shapes:
//! - A single-file book (`.m4b`/`.m4a`, or a lone `.mp3`). The scanner splits
//!   it by chapters.
//! - A multi-track **MP3 folder** (Task 3.3): a folder of per-chapter MP3s.
//!   The scanner ingests one episode per file, with **no split and no
//!   re-encode**. Track number sets the order; filename order is the fallback.
//!
//! The scanner assigns collision-free slugs deterministically. One bad book
//! never aborts the whole scan. The scanner stream-copies Tier-2 inputs
//! (Ogg Vorbis/Opus/FLAC) into a matching container (Task 3.9). It skips DRM
//! inputs (AAX/AAXC/`.aa`/`.odm`) and logs a notice (PRD W5).
//!
//! **The server serves whole-file episodes in place (Sprint 6.2).** An episode
//! can be a whole source file: every MP3-folder track, and each chapterless
//! single file. The server streams such an episode directly from the read-only
//! library, and the scanner records its `source_path`. The scanner copies
//! nothing under `<data_dir>`. Only chaptered books are extracted
//! (`full`/`saver`), because their episodes are sub-ranges of one container.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// All three re-exports are part of this crate's public surface. `BookOverrides`
// is a parameter of the scan API. `StorageMode` and `TranscodeMode` are fields
// of [`ScanOptions`].
use podspine_config::book_overrides;
pub use podspine_config::{BookOverrides, StorageMode, TranscodeMode};
use podspine_feed::{episode_guid, pubdate_epoch};
use podspine_index::{BookRow, EpisodeRow, Index, IndexError};
use podspine_prober::{ProbeError, needs_faststart, probe};
use podspine_splitter::{
    ChapterCut, Encoding, SplitEpisode, SplitError, cover_thumb_path, extract_cover,
    extract_cover_thumb, remux_faststart, split_book_encoded, split_chapter_encoded,
    transcode_whole,
};

/// DRM extensions that the scanner refuses to ingest. The match ignores case.
const DRM_EXTENSIONS: &[&str] = &["aax", "aaxc", "aa", "odm"];

/// The server-global ingest options, passed through the scan API as one value.
///
/// Per-book `.podspine.toml` overrides refine these values per book
/// (Sprint 6.4). A field here is the *default* for a book, not the final value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Ignore any `.cue`/`.ffmeta` sidecar and use embedded chapters (Task 3.8).
    pub force_embedded: bool,
    /// Storage strategy for chaptered books (Sprint 5.1). `Saver` splits at
    /// ingest to record each real `byte_length`, then deletes the files and
    /// regenerates them on demand. `Full` (the default) pre-splits and keeps
    /// the files.
    pub storage: StorageMode,
    /// Remux a non-faststart whole-file mp4 to a faststart cache copy instead of
    /// serving it in place (Sprint 6.3).
    pub remux_non_faststart: bool,
    /// Re-encode sources that podcatchers do not play reliably (FLAC/Vorbis/
    /// Opus/ALAC) to AAC or MP3 at ingest (Task 5.2). The default is off:
    /// Podspine is copy-first. Podspine never re-encodes MP3/AAC sources,
    /// independent of this setting.
    pub transcode: TranscodeMode,
}

/// Failure modes of a single-book scan.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The input path is not a regular file.
    #[error("not a file: {0}")]
    NotAFile(PathBuf),
    /// The input is DRM-protected, so the scanner skipped it.
    #[error("DRM-protected input skipped (Podspine ships no circumvention): {0}")]
    UnsupportedDrm(PathBuf),
    /// The scanner could not read the source `mtime`.
    #[error("could not read source mtime for {path}: {source}")]
    Mtime {
        /// The path.
        path: PathBuf,
        /// I/O error.
        source: std::io::Error,
    },
    /// An MP3 folder held no ingestable (probeable) audio.
    #[error("no ingestable audio in folder: {0}")]
    EmptyFolder(PathBuf),
    /// A filesystem operation (stat, mkdir, or split-file delete) failed during ingest.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// The path involved.
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
/// name. This is a convenience wrapper over [`scan_book_as`] for single-book
/// callers.
pub fn scan_book(input: &Path, data_dir: &Path, index: &Index) -> Result<BookRow, ScanError> {
    let id = slugify(&file_stem(input));
    scan_book_as(
        input,
        &id,
        data_dir,
        index,
        ScanOptions::default(),
        &BookOverrides::default(),
    )
}

/// Scan one audiobook `input` into `index` under the explicit `id`, which is
/// also the slug. Write the split episodes under `<data_dir>/books/<id>/`.
/// Return the persisted [`BookRow`]. The library scanner uses this function to
/// assign collision-free slugs. Single-book callers should use [`scan_book`].
///
/// `force_embedded` skips sidecar (`.cue`/`.ffmeta`) chapter resolution. It
/// uses the embedded chapters even when a sidecar exists (Task 3.8).
///
/// `saver` is the on-demand storage mode (Sprint 5.1). The scan still splits
/// each chapter once, so that it records the real `byte_length` (the
/// `enclosure length`). It then deletes the file immediately; the http layer
/// regenerates the file on demand. The peak extra disk usage is one chapter,
/// not a full second copy of the book. The default is `false` (pre-split, keep
/// the files).
pub fn scan_book_as(
    input: &Path,
    id: &str,
    data_dir: &Path,
    index: &Index,
    opts: ScanOptions,
    overrides: &BookOverrides,
) -> Result<BookRow, ScanError> {
    if !input.is_file() {
        return Err(ScanError::NotAFile(input.to_path_buf()));
    }
    if is_drm(input) {
        return Err(ScanError::UnsupportedDrm(input.to_path_buf()));
    }
    // Persist an ABSOLUTE, symlink-resolved source path. In-place serving and
    // saver regeneration resolve this path later from the server's cwd. A
    // relative `--library` path stored verbatim would 404 after a restart from
    // a different directory (systemd/Docker). `is_file` above proved that the
    // file exists, so `canonicalize` succeeds. The fallback only guards a race.
    let input_canonical = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());
    let input = input_canonical.as_path();

    // Per-book `.podspine.toml` overrides refine the global flags for this book
    // (Sprint 6.4). The caller handles `disabled` before it calls this function.
    let force_embedded = overrides
        .force_embedded_chapters
        .unwrap_or(opts.force_embedded);
    let remux_non_faststart = overrides
        .remux_non_faststart
        .unwrap_or(opts.remux_non_faststart);
    let storage = overrides.storage_mode.unwrap_or(opts.storage);
    let saver = storage == StorageMode::Saver;
    let force_reingest = overrides.force_reingest == Some(true);

    // Compute the effective per-book metadata (override → default) up here, for
    // two reasons. The idempotency check can then spot a `.podspine.toml` edit
    // and re-ingest (such an edit does not change the audio mtime). And the
    // `BookRow` build below reuses these values.
    let eff_title = overrides.title.clone().unwrap_or_else(|| file_stem(input));
    let eff_author = overrides.author.clone();
    let eff_cover = overrides.default_cover_url.clone();

    let id = id.to_string();
    let source_mtime = mtime_epoch(input)?;
    let book_out = data_dir.join("books").join(&id);

    // Idempotency check: if the book is already indexed at this mtime and all
    // files are present, the scan is done. Do not re-probe or re-split.
    if let Some(existing) = index.get_book(&id)?
        && existing.source_mtime == source_mtime
    {
        let eps = index.episodes_for_book(&id)?;
        // In `saver` mode the split files are intentionally absent (the http
        // layer regenerates them on demand). Do not require them on disk. The
        // index entry is enough.
        //
        // BUT guard against a migrated database. `Index::migrate` back-fills
        // `start_sec = 0` for pre-5.1 rows. A non-first chapter with
        // `start_sec == 0` cannot drive correct on-demand regeneration: it
        // would run ffmpeg with `-ss 0` and serve the book's opening seconds.
        // Force a one-time re-split (skip this early return), so that the
        // re-split records the real offsets before any eviction can serve the
        // wrong segment. Chapter 0 legitimately starts at 0, so this check
        // covers only non-first chapters.
        let start_secs_recorded =
            !saver || eps.iter().filter(|e| e.idx > 0).all(|e| e.start_sec > 0.0);
        // Faststart re-ingest guard (Sprint 6.3). `PODSPINE_REMUX_NON_FASTSTART`
        // can change between scans. The recorded serve mode of a
        // `needs_faststart` whole-file episode (in place ⇒
        // `file_path == source_path`; remuxed ⇒ `file_path != source_path`)
        // then no longer matches the flag. Re-ingest, so that the scan records
        // `byte_length`/`file_path` again for the current mode.
        let faststart_consistent = eps.iter().all(|e| {
            !e.needs_faststart
                || e.source_path.is_empty()
                || (e.file_path != e.source_path) == remux_non_faststart
        });
        // An episode's file may legitimately be absent when the server
        // regenerates it on demand: a saver chapter, or a remuxed whole-file
        // cache copy. Everything else (full chapters, in-place whole files)
        // must be present on disk.
        let files_present = eps.iter().all(|e| {
            let regenerable = (saver && e.source_path.is_empty())
                || (!e.source_path.is_empty() && e.file_path != e.source_path);
            regenerable || Path::new(&e.file_path).exists()
        });
        // A `.podspine.toml` edit does not change the audio mtime. So also
        // re-ingest when the persisted metadata no longer matches the current
        // overrides (Greptile 6.4 P1). Otherwise a changed
        // title/author/storage_mode/cover would stay stale in the index.
        // `source_mtime` is unchanged, so episode `guid`s stay stable (clients
        // do not re-download anything).
        let metadata_consistent = existing.title == eff_title
            && existing.author == eff_author
            && existing.storage_mode == Some(storage)
            && existing.default_cover_url == eff_cover
            // `force_embedded_chapters` changes the chapter SOURCE (embedded vs
            // a `.cue`/`.ffmeta` sidecar) and touches none of the fields above.
            // So a toggle must also re-ingest (Greptile 6.4 P1).
            && existing.force_embedded == force_embedded;
        // Transcode toggle guard (Task 5.2). A `PODSPINE_TRANSCODE` flip changes
        // the episode container AND every recorded `byte_length`, and touches
        // no source mtime. So re-ingest when the persisted mode no longer
        // matches what this setting would produce. A stream copy produced each
        // pre-5.2 row (`None`), and a stream copy is exactly what `Off` means.
        // Such a row is therefore not a mismatch, and an upgrade re-splits
        // nobody's library.
        let stored_transcode = existing.transcode.unwrap_or(TranscodeMode::Off);
        let transcode_consistent = expected_transcode(input, opts.transcode)
            .is_none_or(|expected| stored_transcode == expected);
        // `force_reingest` (a troubleshooting option) always skips the early
        // return. While it is set, every scan re-processes the book.
        if !force_reingest
            && metadata_consistent
            && !eps.is_empty()
            && start_secs_recorded
            && faststart_consistent
            && transcode_consistent
            && files_present
        {
            // The book is up to date, but a browse-UI thumbnail can be missing:
            // a library that predates thumbnails, or a thumbnail deleted from
            // the cache. Backfill it from the already-extracted cover, without
            // a re-split. The grid then gets thumbnails on the next reconcile,
            // not on a re-index.
            if let Some(cover) = existing.cover_path.as_deref()
                && !cover_thumb_path(&book_out).exists()
                && let Err(err) = extract_cover_thumb(Path::new(cover), &book_out)
            {
                tracing::warn!(error = %err, id = %id, "cover thumbnail backfill failed; browse UI will use the full cover");
            }
            return Ok(existing);
        }
    }

    let probed = probe(input)?;

    // Resolve the chapter source: a sibling `.cue`/`.ffmeta` sidecar wins over
    // embedded markers unless `force_embedded` overrides it (Task 3.8).
    let resolved =
        podspine_chapters::resolve(input, &probed.chapters, probed.duration_sec, force_embedded);
    if resolved.source != podspine_chapters::ChapterSource::Embedded {
        tracing::info!(id = %id, source = ?resolved.source, "using sidecar chapters");
    }

    // Transcoding (Task 5.2). When the operator opts in, the scan re-encodes at
    // ingest each source that podcatchers do not play reliably
    // (FLAC/Vorbis/Opus/ALAC). The scan always stream-copies MP3/AAC sources,
    // independent of the flag.
    let enc = encoding_for(probed.audio_codec.as_deref(), opts.transcode);
    let transcoding = enc != Encoding::Copy;
    if transcoding {
        tracing::info!(
            id = %id,
            codec = probed.audio_codec.as_deref().unwrap_or("unknown"),
            target = opts.transcode.label(),
            "re-encoding a non-podcast-safe source (transcoding is on)"
        );
    }

    // A chapterless file becomes ONE whole-file episode. The server streams it
    // in place from the library (no split, no copy under `<data_dir>`). The
    // splitter extracts a chaptered book per chapter (full/saver). See TAD
    // §5.3. The server never serves a transcoded book in place: clients get
    // the re-encoded bytes, and those bytes exist only under `<data_dir>`.
    let chapterless = resolved.chapters.is_empty();
    let serve_in_place = chapterless && !transcoding;
    // Map chapters to (cut, title) pairs. A chapterless file gets a single
    // episode that spans the whole file.
    let specs: Vec<(ChapterCut, String)> = if chapterless {
        tracing::warn!(
            id = %id,
            "no chapters (embedded or sidecar) — emitting a single-episode feed"
        );
        vec![(
            ChapterCut {
                idx: 0,
                start_sec: 0.0,
                end_sec: probed.duration_sec,
            },
            file_stem(input),
        )]
    } else {
        resolved
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
    // Pick the output container: one that matches the source codec for a stream
    // copy (Task 3.9), or the transcode target's container for a re-encode
    // (Task 5.2).
    let out_ext = episode_ext(probed.audio_codec.as_deref(), enc);
    // The whole-file branch below sets this flag. Chaptered episodes never need
    // faststart (`split_chapter` already writes `moov` first).
    let mut needs_ft = false;
    // A re-encode is not byte-reproducible across ffmpeg builds. So the scan
    // always materializes a transcoded book here, and nothing regenerates it on
    // demand. That rule keeps the published `enclosure length` equal to the
    // bytes served. The serve/evict layers read `book.transcode` and skip
    // regeneration and eviction to match. The scan therefore deliberately
    // overrides an explicit `saver` request for such a book.
    if transcoding && saver {
        tracing::info!(
            id = %id,
            "transcoded book: storing chapters in full (a re-encode can't be regenerated byte-for-byte)"
        );
    }
    let episodes = if chapterless && transcoding {
        // One whole-file episode, re-encoded into `<data_dir>`. No `-ss`/`-t`:
        // the episode is the entire file, so a short probed duration cannot
        // clip it.
        vec![transcode_whole(
            input,
            &book_out,
            0,
            out_ext,
            probed.duration_sec,
            enc,
        )?]
    } else if serve_in_place {
        // The episode is the whole source file, so there is nothing to extract.
        // (The cleanup below reclaims any per-episode copy that a previous
        // ingest left, after the index update.) Faststart check (Sprint 6.3): a
        // non-faststart whole-file mp4 (`moov` after `mdat`) seeks slowly when
        // streamed in place. Detect it without ffmpeg.
        needs_ft = needs_faststart(input);
        if needs_ft && remux_non_faststart {
            // Opt-in remux: write a faststart cache copy (byte-deterministic
            // `-c copy`), measure it, then delete it. The http layer
            // regenerates the copy on demand and evicts it under the cache cap.
            // The source stays untouched.
            std::fs::create_dir_all(&book_out).map_err(|source| ScanError::Io {
                path: book_out.clone(),
                source,
            })?;
            let ep = remux_faststart(input, &book_out, 0, out_ext, probed.duration_sec)?;
            std::fs::remove_file(&ep.path).map_err(|source| ScanError::Io {
                path: ep.path.clone(),
                source,
            })?;
            vec![ep]
        } else {
            // Serve in place from the read-only library: no ffmpeg, no copy.
            // The enclosure length is the real source size. A non-faststart mp4
            // still plays, so only log a one-line notice that names the
            // (opt-in) fix.
            if needs_ft {
                tracing::warn!(
                    id = %id,
                    book = %file_stem(input),
                    "non-faststart MP4 (moov after mdat): plays but seeks slowly. Set PODSPINE_REMUX_NON_FASTSTART=true to remux it to faststart."
                );
            }
            let byte_length = std::fs::metadata(input)
                .map_err(|source| ScanError::Io {
                    path: input.to_path_buf(),
                    source,
                })?
                .len();
            vec![SplitEpisode {
                idx: 0,
                path: input.to_path_buf(),
                byte_length,
                duration_sec: probed.duration_sec,
            }]
        }
    } else if saver && !transcoding {
        // Split each chapter to record its real byte size, then delete it. The
        // http layer regenerates the file on demand (the stream copy is
        // deterministic, so the regenerated bytes match the recorded length).
        // The peak extra disk usage is one chapter.
        std::fs::create_dir_all(&book_out).map_err(|source| ScanError::Io {
            path: book_out.clone(),
            source,
        })?;
        let mut eps = Vec::with_capacity(cuts.len());
        for ch in &cuts {
            let ep = split_chapter_encoded(input, &book_out, ch, out_ext, enc)?;
            std::fs::remove_file(&ep.path).map_err(|source| ScanError::Io {
                path: ep.path.clone(),
                source,
            })?;
            eps.push(ep);
        }
        eps
    } else {
        split_book_encoded(input, &book_out, &cuts, out_ext, enc)?
    };

    // Extract the embedded cover, if any. A missing cover is a normal case. An
    // extraction failure never fails the book; the server then serves no cover
    // art.
    let cover_path = if probed.has_cover {
        let ext = cover_ext(probed.cover_codec.as_deref());
        match extract_cover(input, &book_out, ext) {
            Ok(path) => {
                // Regenerate the browse-UI thumbnail from the freshly extracted
                // cover, in this same (single) scanner thread and atomically,
                // so that it always matches the cover. The http layer only ever
                // *serves* the thumbnail. That split keeps thumbnail and cover
                // consistent with no cross-thread race.
                //
                // Delete the previous thumbnail FIRST. If regeneration then
                // fails, the book is left with NO thumbnail, not with a stale
                // one derived from the old cover. (The serve layer falls back
                // to the current full cover, and the next reconcile backfills
                // the thumbnail.)
                let _ = std::fs::remove_file(cover_thumb_path(&book_out));
                if let Err(err) = extract_cover_thumb(&path, &book_out) {
                    tracing::warn!(error = %err, id = %id, "cover thumbnail failed; browse UI will use the full cover");
                }
                Some(path.to_string_lossy().into_owned())
            }
            Err(err) => {
                // `extract_cover` publishes atomically: it writes a `.part`
                // sibling and renames it only on success. A failed extraction
                // therefore leaves the previous `cover.jpg`, and its thumbnail,
                // intact on disk. Keep the stored path; do not drop it to
                // `None` and orphan that still-valid art. `None` would 404 both
                // cover routes and block the reconcile thumbnail backfill,
                // which requires a populated `cover_path`. `upsert_book` below
                // has not run yet, so this read still sees the prior row.
                let kept = index
                    .get_book(&id)
                    .ok()
                    .flatten()
                    .and_then(|b| b.cover_path)
                    .filter(|p| Path::new(p).exists());
                if kept.is_some() {
                    tracing::warn!(error = %err, id = %id, "cover extraction failed; keeping the previously extracted cover");
                } else {
                    tracing::warn!(error = %err, id = %id, "cover extraction failed; serving no cover");
                }
                kept
            }
        }
    } else {
        None
    };

    let book = BookRow {
        id: id.clone(),
        slug: id.clone(),
        feed_id: podspine_index::capability::generate(),
        // Per-book overrides (Sprint 6.4). The code above computes them, and
        // the idempotency guard re-checks them, so a sidecar edit re-persists
        // them.
        title: eff_title,
        author: eff_author,
        cover_path,
        source_path: input.to_string_lossy().into_owned(),
        source_mtime,
        // Persist the effective mode so serve/evict honor it without the sidecar.
        storage_mode: Some(storage),
        default_cover_url: eff_cover,
        force_embedded,
        // What actually happened to this book's audio (Task 5.2): `Off` means a
        // stream copy. The serve/evict layers read this field (nothing
        // regenerates a transcoded book), and so does the toggle guard above.
        transcode: Some(if transcoding {
            opts.transcode
        } else {
            TranscodeMode::Off
        }),
    };
    index.upsert_book(&book)?;

    for (ep, (cut, title)) in episodes.iter().zip(&specs) {
        index.upsert_episode(&EpisodeRow {
            guid: episode_guid(&id, ep.idx, source_mtime),
            book_id: id.clone(),
            idx: ep.idx as i64,
            title: title.clone(),
            file_path: ep.path.to_string_lossy().into_owned(),
            // This field is non-empty for a whole-file episode (the source
            // path) and empty for an extracted chapter under `<data_dir>`.
            // `file_path == source_path` means in-place serving.
            // `file_path != source_path` means a remux to the faststart cache.
            source_path: if serve_in_place {
                input.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            // This flag is only ever true for the single whole-file episode.
            // It drives the http remux-vs-in-place decision and the toggle
            // guard above.
            needs_faststart: needs_ft,
            byte_length: ep.byte_length as i64,
            duration_sec: ep.duration_sec,
            start_sec: cut.start_sec,
            pubdate_epoch: pubdate_epoch(source_mtime, ep.idx, n),
        })?;
    }

    // Sweep leftovers only now, AFTER the index points at this ingest's
    // episodes. Until that upsert lands, the server still serves the old files,
    // and this function can sit inside a re-encode for minutes. An up-front
    // delete would 404 every request for that whole window. And if the encode
    // then failed, it would leave the book with no playable episodes at all.
    // Once the rows are written, any file left in another container is
    // unreferenced.
    //
    // A residual window remains, deliberately unguarded. A request can
    // snapshot an episode row just before the upsert and reach its
    // `File::open` just after the unlink. That request gets a clean 404 (the
    // http layer fails closed at three points; it never serves partial or
    // wrong bytes). Three facts bound the window:
    // - Only the few microseconds between that snapshot and the open are
    //   exposed.
    // - It can only fire on an ingest that actually CHANGED a book's
    //   container: a transcode flag or target flip. A steady-state rescan
    //   deletes nothing.
    // - On POSIX, a reader that already opened the file keeps its inode
    //   regardless.
    // A guard would mean one of two bad options: hold the index lock across
    // blocking file I/O, or add a re-resolve-and-retry in the handler that no
    // test can drive deterministically. Neither is worth it for one retryable
    // 404 during an operator-triggered re-ingest.
    if serve_in_place {
        // Episodes stream from the library now. Any per-episode copy that a
        // pre-6.2 ingest or a previous transcode left under `<data_dir>` is
        // therefore dead weight.
        remove_stale_episode_copies(&book_out);
    } else {
        remove_episode_files_in_other_containers(&book_out, out_ext);
    }

    Ok(book)
}

/// Remove episode files under `book_out` that this ingest will **not**
/// overwrite: leftovers in a container that the book no longer uses.
///
/// A switch between stream copy and a transcode target (Task 5.2), or between
/// the AAC and MP3 targets, changes the episode extension: `001.flac` becomes
/// `001.m4a`. The index points at the new path, so the old files are
/// unreferenced from that moment on. Nothing else reclaims them: the cache
/// eviction only touches regenerable (`saver`, stream-copied) books, and a
/// transcoded book is never regenerable. Left in place, the old files cost a
/// full extra copy of the audiobook per mode change.
///
/// This sweep only considers numbered episode files (`NNN.<ext>`) and their
/// `NNN.part.<ext>` temporaries, so it never touches an extracted `cover.*`.
/// It leaves files already in `keep_ext` for the ingest to overwrite. It is
/// best-effort: it logs a missing directory or a failed unlink, and neither is
/// fatal.
fn remove_episode_files_in_other_containers(book_out: &Path, keep_ext: &str) {
    let Ok(entries) = std::fs::read_dir(book_out) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext.eq_ignore_ascii_case(keep_ext) {
            continue;
        }
        if is_episode_stem(&path)
            && path.is_file()
            && let Err(err) = std::fs::remove_file(&path)
        {
            tracing::warn!(error = %err, path = %path.display(), "failed to remove an episode file from a previous container");
        }
    }
}

/// Remove per-episode audio copies that a previous (pre-6.2) ingest wrote
/// under `<data_dir>/books/<id>/`. This book's episodes now stream in place
/// from the library. The sweep only removes numbered episode files
/// (`NNN.<ext>`); it leaves an extracted `cover.*` in place. It is
/// best-effort: it logs a missing directory or a failed unlink, and neither is
/// fatal (the book still serves from the library).
fn remove_stale_episode_copies(book_out: &Path) {
    let Ok(entries) = std::fs::read_dir(book_out) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_episode_stem(&path)
            && path.is_file()
            && let Err(err) = std::fs::remove_file(&path)
        {
            tracing::warn!(error = %err, path = %path.display(), "failed to remove stale episode copy");
        }
    }
}

/// Whether `path` is a produced episode file, not something else in the book
/// directory. That means a numbered `NNN.<ext>` (the splitter's cross-crate
/// [`podspine_splitter::episode_file_name`] contract), or the `NNN.part.<ext>`
/// temporary that an interrupted encode can leave (see
/// `podspine_splitter::part_path`). An extracted `cover.*` matches neither, so
/// no sweep ever removes it.
fn is_episode_stem(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".part").unwrap_or(s))
        .is_some_and(podspine_splitter::is_episode_stem)
}

/// One per-chapter MP3 track discovered in a folder, with the metadata needed to
/// order and index it.
struct Mp3Track {
    /// Source path in the library.
    path: PathBuf,
    /// Duration in seconds (from ffprobe).
    duration_sec: f64,
    /// Track number tag, if present.
    track: Option<u32>,
    /// Episode title (ID3 `title` tag, else the file stem).
    title: String,
}

/// Ingest a folder of per-chapter MP3s as one book under `id`: one episode per
/// file, with **no split, no re-encode, and no copy**. The server serves each
/// track in place from the library (Sprint 6.2). Track number sets the file
/// order when every track number is present and distinct; otherwise filename
/// order applies and the scan logs a warning. The scan is idempotent on an
/// unchanged folder.
fn scan_mp3_folder(
    dir: &Path,
    id: &str,
    data_dir: &Path,
    index: &Index,
    overrides: &BookOverrides,
    library_root: &Path,
) -> Result<BookRow, ScanError> {
    // Canonicalize the folder, so that every track path stored below is
    // absolute and symlink-resolved. In-place serving must not depend on the
    // server's cwd.
    let dir_canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let dir = dir_canonical.as_path();
    let files = collect_mp3s(dir, library_root);
    if files.is_empty() {
        return Err(ScanError::EmptyFolder(dir.to_path_buf()));
    }

    // The book mtime is the newest track mtime. It is stable while the folder
    // is unchanged, and it bumps when a track is replaced.
    let source_mtime = files
        .iter()
        .map(|f| mtime_epoch(f))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let book_out = data_dir.join("books").join(id);

    // Compute the effective per-book metadata (override → default) up here to
    // detect a `.podspine.toml` edit; the `BookRow` build below reuses it.
    // `storage_mode`/`remux`/`force_embedded` are no-ops for MP3 folders, so
    // only title/author/cover apply.
    let eff_title = overrides.title.clone().unwrap_or_else(|| dir_name(dir));
    let eff_author = overrides.author.clone();
    let eff_cover = overrides.default_cover_url.clone();

    // Idempotency check: if the folder is unchanged and already served in
    // place, do not re-probe. The `source_path` guard forces a one-time
    // re-ingest of a pre-6.2 book (tracks copied under `<data_dir>`, empty
    // `source_path`). The book then flips to in-place serving, and the sweep
    // reclaims its copies. The metadata checks re-ingest on a `.podspine.toml`
    // edit that did not change the folder mtime (Greptile P1).
    if overrides.force_reingest != Some(true)
        && let Some(existing) = index.get_book(id)?
        && existing.source_mtime == source_mtime
        && existing.title == eff_title
        && existing.author == eff_author
        && existing.default_cover_url == eff_cover
    {
        let eps = index.episodes_for_book(id)?;
        if !eps.is_empty()
            && eps
                .iter()
                .all(|e| !e.source_path.is_empty() && Path::new(&e.source_path).exists())
        {
            return Ok(existing);
        }
    }

    // Probe each track for duration/track/title. The scan skips a corrupt
    // file; it is not fatal to the book.
    let mut tracks: Vec<Mp3Track> = Vec::new();
    for path in &files {
        match probe(path) {
            Ok(p) => tracks.push(Mp3Track {
                duration_sec: p.duration_sec,
                track: p.track,
                title: p.title.unwrap_or_else(|| file_stem(path)),
                path: path.clone(),
            }),
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "skipping unprobeable mp3")
            }
        }
    }
    if tracks.is_empty() {
        return Err(ScanError::EmptyFolder(dir.to_path_buf()));
    }
    order_mp3_tracks(&mut tracks, dir);

    let book = BookRow {
        id: id.to_string(),
        slug: id.to_string(),
        feed_id: podspine_index::capability::generate(),
        // Per-book overrides (Sprint 6.4). The code above computes them, and
        // the idempotency guard re-checks them, so a sidecar edit re-persists
        // them. `storage_mode`/`remux`/`force_embedded` are no-ops for MP3
        // folders (the server serves tracks in place), so persist no
        // `storage_mode` (`None` = follow the global setting).
        title: eff_title,
        author: eff_author,
        cover_path: None,
        source_path: dir.to_string_lossy().into_owned(),
        source_mtime,
        storage_mode: None,
        default_cover_url: eff_cover,
        // An MP3 folder has no chapters, so `force_embedded` never applies.
        force_embedded: false,
        // MP3 is podcast-safe: the scan never re-encodes an MP3 folder
        // (Task 5.2).
        transcode: Some(TranscodeMode::Off),
    };
    index.upsert_book(&book)?;

    let n = tracks.len();
    // Each track is a whole file. The server serves it in place from the
    // library, with no copy. Reclaim any verbatim copies that a pre-6.2 ingest
    // wrote under `<data_dir>`.
    remove_stale_episode_copies(&book_out);
    for (idx, t) in tracks.iter().enumerate() {
        let byte_length = std::fs::metadata(&t.path)
            .map_err(|source| ScanError::Io {
                path: t.path.clone(),
                source,
            })?
            .len();
        index.upsert_episode(&EpisodeRow {
            guid: episode_guid(id, idx, source_mtime),
            book_id: id.to_string(),
            idx: idx as i64,
            title: t.title.clone(),
            file_path: t.path.to_string_lossy().into_owned(),
            // A folder track IS a whole source file. Stream it in place.
            source_path: t.path.to_string_lossy().into_owned(),
            // MP3 has no `moov` atom, so faststart never applies.
            needs_faststart: false,
            byte_length: byte_length as i64,
            duration_sec: t.duration_sec,
            // Tracks are whole files, not sub-ranges of a container, so each
            // starts at 0.
            start_sec: 0.0,
            pubdate_epoch: pubdate_epoch(source_mtime, idx, n),
        })?;
    }

    Ok(book)
}

/// Order tracks by track number when every number is present and distinct.
/// Otherwise fall back to a case-insensitive filename sort and log a warning.
fn order_mp3_tracks(tracks: &mut [Mp3Track], dir: &Path) {
    let numbers: Option<Vec<u32>> = tracks.iter().map(|t| t.track).collect();
    let usable = numbers.as_ref().is_some_and(|v| {
        let distinct: HashSet<u32> = v.iter().copied().collect();
        distinct.len() == v.len()
    });
    if usable {
        tracks.sort_by_key(|t| t.track.unwrap());
    } else {
        tracing::warn!(
            path = %dir.display(),
            "MP3 folder has missing or duplicate track numbers; ordering by filename"
        );
        tracks.sort_by_key(|t| {
            t.path
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        });
    }
}

/// Collect the top-level `.mp3` files in `dir` (unordered; the caller sorts).
fn collect_mp3s(dir: &Path, library_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && ext_lower(p).as_deref() == Some("mp3"))
        // A track can itself be a symlink out of the library even when its
        // folder is inside it: `source_is_inside` only vetted the folder. Such
        // a track would land in the feed and then 404 at serve time, because
        // the http layer canonicalizes each enclosure and refuses anything
        // outside the library root. Drop it here, loudly (Greptile):
        // [`resolve_inside`] warns.
        .filter(|p| resolve_inside(p, library_root).is_some())
        .collect()
}

/// A directory's own name (fallback `"book"`), used as an MP3-folder book title.
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string())
}

/// Outcome of a library scan. Counts only: the scan never holds a library of
/// thousands of books in memory; it indexes each book and drops it in turn
/// (NFR-P4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanSummary {
    /// Books successfully indexed.
    pub indexed: usize,
    /// Sources skipped: bad/DRM'd files, or MP3 folders pending Task 3.3.
    pub skipped: usize,
    /// Orphaned books pruned (set by [`reconcile`]; `scan_library` leaves it 0).
    pub pruned: usize,
}

/// A discovered book source within the library.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BookSource {
    /// A single splittable audio file (`.m4b`/`.m4a`, or a lone `.mp3`).
    File(PathBuf),
    /// A folder of per-track MP3s (recognized in v1, ingested since Task 3.3).
    Mp3Folder(PathBuf),
}

impl BookSource {
    /// The source's filesystem path: the audio file, or the MP3 folder.
    fn path(&self) -> &Path {
        match self {
            BookSource::File(p) => p,
            BookSource::Mp3Folder(d) => d,
        }
    }

    /// The base name that a slug is derived from.
    ///
    /// Some books were discoverable before recursive walking existed: a file
    /// at the library root, or a book folder directly below it. For those, the
    /// base name is exactly what it always was: the file stem, or the folder
    /// name. That is deliberate and load-bearing. The slug becomes `book.id`,
    /// and a book's capability `feed_id` is preserved per id across re-scans.
    /// A change to how an existing book's id is derived would therefore rotate
    /// its feed URL and silently break every subscriber.
    ///
    /// A book found *deeper* than that has never been indexed before, so its
    /// name can be the better one: the path from the library root to its
    /// folder, joined. `Jules Verne/The Mysterious Island/…m4b` becomes
    /// `Jules Verne - The Mysterious Island`. That reads well. It also keeps
    /// two books with the same title under different authors from colliding
    /// into `the-mysterious-island` and `-2`, whose assignment would depend on
    /// walk order.
    fn base_name(&self, library_root: &Path) -> String {
        match self {
            BookSource::File(p) => match nested_prefix(p.parent(), library_root) {
                Some(name) => name,
                None => file_stem(p),
            },
            BookSource::Mp3Folder(d) => match nested_prefix(Some(d), library_root) {
                Some(name) => name,
                // A folder name has no extension to strip; use it whole.
                None => d
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "book".to_string()),
            },
        }
    }
}

/// The folder that a nested book sits in, as its display title:
/// `Jules Verne/The Mysterious Island/Jules Verne -   - The Mysterious Island.m4b`
/// becomes `Some("The Mysterious Island")`.
///
/// A nested library names the book on the folder and treats the filename as a
/// dumping ground for author, narrator, and separators. So the folder is
/// almost always the better title. The result is `None` for a book at or
/// directly below the root; such a book keeps the file stem as its title.
/// Those books may already be indexed, and a feed title must not change under
/// a subscriber by accident. A `.podspine.toml` `title` still wins over this.
fn nested_title(source: &BookSource, library_root: &Path) -> Option<String> {
    let BookSource::File(path) = source else {
        // An MP3-folder book is already titled by its folder.
        return None;
    };
    let dir = path.parent()?;
    // This applies only to a book below the first level:
    // `<root>/<author>/<title>/…`.
    nested_prefix(Some(dir), library_root)?;
    dir.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// The joined path from `library_root` to a book folder, for a book nested
/// deeper than one level: `<root>/Jules Verne/The Mysterious Island` becomes
/// `Some("Jules Verne - The Mysterious Island")`.
///
/// The result is `None` for a book at or directly below the root (those keep
/// their historical name, see [`BookSource::base_name`]), and for anything
/// outside the root.
fn nested_prefix(book_dir: Option<&Path>, library_root: &Path) -> Option<String> {
    let rel = book_dir?.strip_prefix(library_root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (parts.len() > 1).then(|| parts.join(" - "))
}

/// Audio extensions that Podspine ingests: Tier-1 (M4B/M4A/MP3) and Tier-2
/// (Ogg Vorbis/Opus/FLAC, Task 3.9). DRM inputs (AAX/AAXC/`.aa`/`.odm`) are
/// deliberately absent from this list; discovery logs them as skipped
/// (PRD W5).
const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3", "ogg", "oga", "opus", "flac"];

/// How far below the library root the walk descends before it gives up. The
/// limit is deep enough for `shelf/author/series/title/`, and shallow enough
/// that a symlink that the loop guard somehow misses cannot spin.
const MAX_LIBRARY_DEPTH: usize = 8;

/// Resolve and parse a book's `.podspine.toml` (Sprint 6.4). A missing sidecar
/// yields the empty default. The scan logs and drops a bad sidecar, and also a
/// server-global key that does not apply per book; neither is fatal. `source`
/// is canonicalized, so the folder-vs-library-root check compares like for
/// like.
fn resolve_book_overrides(source: &Path, library_root: &Path) -> BookOverrides {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    match book_overrides::load(&source, library_root) {
        Ok(Some(o)) => {
            for key in o.ignored_global_keys() {
                tracing::warn!(source = %source.display(), key, "ignoring server-global key in .podspine.toml");
            }
            o
        }
        Ok(None) => BookOverrides::default(),
        Err(msg) => {
            tracing::warn!("{msg}; ignoring per-book overrides");
            BookOverrides::default()
        }
    }
}

/// Collapse any set of index rows that share one source to a single row. Keep
/// the **earliest-created** row (the one whose feed subscribers have held
/// longest). Delete the rest, together with their extracted output.
///
/// This is the floor that the rest of the identity logic stands on: **one
/// source, one book row, one feed.** Nothing the current scanner writes
/// creates a second row for one source; the source→id reuse map in
/// [`scan_library`] sees to that. But a database written by an earlier build,
/// or edited by hand, can hold such rows, and neither reuse nor orphan pruning
/// would ever reconcile it: the map keeps one id arbitrarily, and both rows
/// survive pruning because their shared source still exists. The book would
/// stay listed under two capability URLs forever. Running this first makes the
/// reuse map unambiguous and heals such a database in one reconcile.
///
/// A row whose source is *gone* is left for [`prune_orphans`]. The collapse
/// groups only canonicalizable paths, so an unmounted library (every source
/// missing) collapses nothing. It is best-effort: a failed lookup leaves the
/// index untouched.
fn collapse_duplicate_source_rows(index: &Index, data_dir: &Path) {
    let identities = match index.book_source_identities() {
        Ok(identities) => identities,
        Err(err) => {
            tracing::warn!(error = %err, "duplicate-row collapse skipped: identity listing failed");
            return;
        }
    };
    // `book_source_identities` is ordered oldest-first (`created_at` asc). So
    // the FIRST row seen for a source is the earliest-created one (the feed
    // that subscribers have held longest), and it is the one to keep. A sort
    // by id instead would drop an established `foo-2` in favour of a `foo`
    // that a later bug duplicated. That would delete the very feed people use
    // (Greptile).
    let mut kept_for_source: HashMap<PathBuf, String> = HashMap::new();
    for (id, source_path, _created_at) in identities {
        // A gone source is orphan-pruning's job. The grouping also covers only
        // canonicalizable paths, so an unmounted library collapses nothing.
        let Ok(real) = Path::new(&source_path).canonicalize() else {
            continue;
        };
        match kept_for_source.get(&real) {
            None => {
                kept_for_source.insert(real, id);
            }
            Some(kept) => {
                let book_out = data_dir.join("books").join(&id);
                if book_out.exists()
                    && let Err(err) = std::fs::remove_dir_all(&book_out)
                {
                    tracing::warn!(error = %err, dir = %book_out.display(), "could not remove a duplicate book's output");
                }
                match index.delete_book(&id) {
                    Ok(_) => {
                        tracing::warn!(id = %id, kept = %kept, "removed a duplicate feed row for one source (kept the earliest)")
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, id = %id, "could not remove a duplicate feed row")
                    }
                }
            }
        }
    }
}

/// Scan a library root of many audiobooks into `index`. Write each book's
/// episodes under `<data_dir>/books/<slug>/`. Each top-level audio file and
/// each per-book subfolder becomes one independent book. Slugs are
/// collision-free and deterministic across re-scans. The scan logs and skips a
/// single failing book; it is never fatal.
pub fn scan_library(
    library: &Path,
    data_dir: &Path,
    index: &Index,
    opts: ScanOptions,
) -> ScanSummary {
    // The canonical library root serves two purposes. It resolves per-book
    // `.podspine.toml` sidecars (Sprint 6.4; the match is against each book's
    // canonical source path). And it names books found below the top level.
    let library_root = library
        .canonicalize()
        .unwrap_or_else(|_| library.to_path_buf());
    let sources = discover(&library_root, data_dir);

    // Enforce one row per source BEFORE this scan reads the reuse map, so that
    // the map cannot inherit a duplicate (invariant A; see the function's doc).
    collapse_duplicate_source_rows(index, data_dir);

    // Every already-indexed book, keyed by its (canonical) source path. A
    // source that is already indexed keeps whatever id it has, including a
    // `-2` suffix it once earned; the scan does not re-derive the id from the
    // name. Without this map, a book once suffixed (because a now-pruned stale
    // row occupied its base id) would ALSO be indexed under the freed base id
    // on the next scan: one audiobook under two capability feeds (Greptile).
    // Reuse makes the id stable per source.
    let existing_by_source: HashMap<PathBuf, String> = match index.list_books() {
        Ok(books) => books
            .into_iter()
            .map(|b| (PathBuf::from(b.source_path), b.id))
            .collect(),
        Err(err) => {
            // Proceed with an empty map (same behavior as before), but say so:
            // this scan skips id reuse, the primary feed-URL stability
            // mechanism. Only the `owns_a_different_source` guard remains.
            tracing::warn!(error = %err, "listing books failed; id reuse skipped this scan");
            HashMap::new()
        }
    };

    let mut seen = HashSet::new();
    let mut summary = ScanSummary::default();
    for source in sources {
        let source_path = source.path();
        // If this exact source is already indexed, keep its id. That rule
        // makes a feed URL stable across scans, and it stops a once-suffixed
        // book from landing under a since-freed base id. Otherwise reserve a
        // slug in deterministic order, and never one that a different
        // still-present book holds.
        let slug = match source_path
            .canonicalize()
            .ok()
            .and_then(|c| existing_by_source.get(&c))
        {
            Some(id) => {
                seen.insert(id.clone());
                id.clone()
            }
            None => assign_slug(
                &slugify(&source.base_name(&library_root)),
                source_path,
                index,
                &mut seen,
            ),
        };
        let mut overrides = resolve_book_overrides(source_path, &library_root);
        // A nested book's filename is usually `Author -   - Title.m4b`; its
        // folder is just `Title`. Seed the title from the folder unless the
        // book's own `.podspine.toml` says otherwise. An explicit title always
        // wins.
        if overrides.title.is_none()
            && let Some(title) = nested_title(&source, &library_root)
        {
            overrides.title = Some(title);
        }
        // `disabled` (a `.podspine.toml` troubleshooting option): drop the
        // book from every surface. Prune it if it was previously indexed, then
        // skip it.
        if overrides.disabled == Some(true) {
            // `delete_book` reports via its bool whether a row existed, so this
            // code needs no pre-check. Surface a failed delete: the book would
            // otherwise silently stay indexed and keep serving its feed.
            if let Err(err) = index.delete_book(&slug) {
                tracing::warn!(slug = %slug, error = %err, "failed to prune disabled book");
            }
            tracing::info!(slug = %slug, "book disabled by .podspine.toml — skipped");
            summary.skipped += 1;
            continue;
        }
        match source {
            BookSource::File(path) => {
                match scan_book_as(&path, &slug, data_dir, index, opts, &overrides) {
                    Ok(book) => {
                        summary.indexed += 1;
                        tracing::info!(slug = %book.slug, title = %book.title, "indexed book");
                    }
                    Err(err) => {
                        summary.skipped += 1;
                        tracing::warn!(error = %err, path = %path.display(), "skipped");
                    }
                }
            }
            BookSource::Mp3Folder(dir) => {
                match scan_mp3_folder(&dir, &slug, data_dir, index, &overrides, &library_root) {
                    Ok(book) => {
                        summary.indexed += 1;
                        tracing::info!(slug = %book.slug, title = %book.title, "indexed MP3-folder book");
                    }
                    Err(err) => {
                        summary.skipped += 1;
                        tracing::warn!(error = %err, path = %dir.display(), "skipped");
                    }
                }
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

/// Remove indexed books whose source file or folder no longer exists, together
/// with their split output under `<data_dir>/books/<id>/`. Return the count of
/// pruned books.
///
/// **Empty-root guard:** if the library root is missing, unreadable, or empty,
/// the prune removes nothing. A transiently unmounted library looks like
/// "every source vanished"; without this guard, an unmount would wipe the
/// whole index. The cost: when you genuinely delete your *last* book, it stays
/// indexed until another book is present. That is a safe trade.
pub fn prune_orphans(library: &Path, data_dir: &Path, index: &Index) -> Result<usize, ScanError> {
    let root_has_entries = std::fs::read_dir(library)
        .map(|mut rd| rd.next().is_some())
        .unwrap_or(false);
    if !root_has_entries {
        tracing::warn!(
            library = %library.display(),
            "library root empty or unreadable — skipping orphan prune (unmount guard)"
        );
        return Ok(0);
    }

    let mut pruned = 0;
    for book in index.list_books()? {
        if Path::new(&book.source_path).exists() {
            continue;
        }
        let book_out = data_dir.join("books").join(&book.id);
        if book_out.exists()
            && let Err(err) = std::fs::remove_dir_all(&book_out)
        {
            tracing::warn!(error = %err, dir = %book_out.display(),
                "could not remove split output for a pruned book");
        }
        index.delete_book(&book.id)?;
        pruned += 1;
        tracing::info!(slug = %book.slug, "pruned orphaned book (source gone)");
    }
    Ok(pruned)
}

/// Reconcile the index with the library: [`scan_library`] (add/update), then
/// [`prune_orphans`] (remove sources that disappeared). The auto-watch runs
/// this after each debounced batch of changes. The server also runs it at
/// startup, so that it cleans up a book deleted while the server was down.
pub fn reconcile(library: &Path, data_dir: &Path, index: &Index, opts: ScanOptions) -> ScanSummary {
    let mut summary = scan_library(library, data_dir, index, opts);
    summary.pruned = prune_orphans(library, data_dir, index).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "orphan prune failed");
        0
    });
    // Read the count from the index; do not accumulate this run's adds. The
    // metric then also reflects a prune, and a book that failed to ingest,
    // accurately. Reconcile runs only at startup and on the debounced watch,
    // so the extra read is cheap.
    match index.list_books() {
        Ok(books) => podspine_metrics::set_books_indexed(books.len() as u64),
        Err(err) => tracing::warn!(error = %err, "could not count books for metrics"),
    }
    summary
}

/// Debounce window. One big file copy lands as many filesystem events; the
/// watch loop coalesces such a burst into a single reconcile once the events
/// stop.
const WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Spawn a background thread that establishes the library watch, runs the
/// **initial** reconcile, and then runs [`reconcile`] again whenever the
/// library changes (debounced). The thread opens its **own** index connection
/// on `db_path`. With WAL enabled, its rescans (including a long split of a
/// newly added book) do not block the server's feed/audio reads.
///
/// The thread registers the watch *before* the initial reconcile. A change
/// that lands during the first scan is then buffered in the channel, and the
/// loop picks it up; it is not missed until the next event or a restart
/// (issue 159). `on_initial_scan` fires once the initial reconcile returns.
/// The server uses it to leave its "Scanning…" holding state and start serving
/// feeds. It runs even if the index cannot be opened or the watch cannot be
/// established, so the server never hangs on "Scanning…".
///
/// This function returns immediately; the watcher runs for the process
/// lifetime. The thread logs a setup failure (or the end of the watch) and
/// simply disables auto-refresh; the server keeps serving what is already
/// indexed. (Task 4.3 / PRD C2.)
pub fn spawn_library_watcher(
    library: PathBuf,
    data_dir: PathBuf,
    db_path: PathBuf,
    opts: ScanOptions,
    on_initial_scan: impl FnOnce() + Send + 'static,
) {
    std::thread::spawn(move || {
        if let Err(err) = watch_loop(&library, &data_dir, &db_path, opts, on_initial_scan) {
            tracing::error!(error = %err, "library watcher stopped — auto-refresh disabled");
        }
    });
}

fn watch_loop(
    library: &Path,
    data_dir: &Path,
    db_path: &Path,
    opts: ScanOptions,
    on_initial_scan: impl FnOnce(),
) -> Result<(), Box<dyn std::error::Error>> {
    use notify::{RecursiveMode, Watcher};

    let index = match Index::open(db_path) {
        Ok(index) => index,
        // No index means no scan and no watch. Flip readiness anyway, so that
        // the server stops holding "Scanning…" (it serves whatever is already
        // indexed). Then surface the failure.
        Err(err) => {
            on_initial_scan();
            return Err(err.into());
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();
    // Filter events at the source, so that an unrelated write cannot spin the
    // watcher into a rescan-every-few-seconds loop. Ignore three groups:
    // - reads (another app that streams the files bumps atime but changes
    //   nothing),
    // - our own split output under `data_dir` (an episode written there would
    //   otherwise self-trigger a rescan forever),
    // - the dir names discovery never walks (dotdirs, `@eaDir`, `lost+found`:
    //   NAS junk and thumbnail caches).
    // Only a real library change reaches the debounced reconcile below. An
    // irrelevant trickle no longer defeats the debounce.
    let library_root = library.to_path_buf();
    let data_dir_owned = data_dir.to_path_buf();
    let data_canon = data_dir.canonicalize().ok();
    // Bring the watch up BEFORE the initial reconcile. A change that lands
    // during the scan's directory walk is then buffered in `rx`, and the loop
    // below picks it up. It is not missed until the next event or a restart
    // (issue 159).
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // Drop irrelevant events. Forward everything else, including errors,
        // so that the loop still sees a watch failure.
        if let Ok(event) = &res
            && !watch_event_is_relevant(
                event,
                &library_root,
                &data_dir_owned,
                data_canon.as_deref(),
            )
        {
            return;
        }
        let _ = tx.send(res);
    })
    .and_then(|mut watcher| {
        watcher.watch(library, RecursiveMode::Recursive)?;
        Ok(watcher)
    });
    let watcher = match watcher {
        Ok(watcher) => {
            tracing::info!(library = %library.display(), "watching library for changes");
            Some(watcher)
        }
        // The watch could not be established (e.g. the inotify watch limit).
        // Still run the initial scan and mark ready below; the server must not
        // hang on "Scanning…". Only auto-refresh is lost.
        Err(err) => {
            tracing::error!(error = %err, "could not watch the library — auto-refresh disabled");
            None
        }
    };

    // Initial reconcile: index new/changed books, and prune books removed
    // while the server was down. It runs after the watch is live, so no
    // in-window change is lost.
    let s = reconcile(library, data_dir, &index, opts);
    tracing::info!(
        indexed = s.indexed,
        skipped = s.skipped,
        pruned = s.pruned,
        "initial scan complete"
    );
    // The first scan is done: the server leaves its "Scanning…" holding state.
    on_initial_scan();

    // If the watch never came up, there is nothing to loop on.
    let Some(_watcher) = watcher else {
        return Ok(());
    };

    // Block for an event. Then drain the burst until it is quiet for the
    // debounce window. Then reconcile once. `_watcher` stays alive in scope,
    // so `rx` never disconnects and the loop runs for the process lifetime.
    while rx.recv().is_ok() {
        while rx.recv_timeout(WATCH_DEBOUNCE).is_ok() {}
        tracing::info!("library changed — reconciling");
        let s = reconcile(library, data_dir, &index, opts);
        tracing::info!(
            indexed = s.indexed,
            skipped = s.skipped,
            pruned = s.pruned,
            "reconcile complete"
        );
    }
    Ok(())
}

/// Whether a watch event should trigger a reconcile. Reads (open/close/access,
/// including the atime bumps another app makes while it streams the files)
/// never change content, so the filter drops them. Otherwise the event is
/// relevant if *any* of its paths is a real library path (notify batches
/// related paths, e.g. a rename's from+to). An event with no paths (a rescan
/// hint) counts as relevant: the filter cannot judge it, so it errs toward a
/// reconcile.
fn watch_event_is_relevant(
    event: &notify::Event,
    library_root: &Path,
    data_dir: &Path,
    data_canon: Option<&Path>,
) -> bool {
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return false;
    }
    if event.paths.is_empty() {
        return true;
    }
    event
        .paths
        .iter()
        .any(|p| watch_path_is_relevant(p, library_root, data_dir, data_canon))
}

/// Whether a changed path is a real library source, not something the walk
/// ignores. This mirrors discovery, so the watch and the walk agree. Skip
/// podspine's own output under `data_dir` (an episode that a nested
/// `--data-dir` writes must not read as a library change). Also skip any path
/// with an ignored component (dotdirs, `@eaDir`, `lost+found`) below the
/// library root. The check is purely lexical (no fs calls): a `Remove` event's
/// path is already gone, and this runs in the hot event handler.
fn watch_path_is_relevant(
    path: &Path,
    library_root: &Path,
    data_dir: &Path,
    data_canon: Option<&Path>,
) -> bool {
    if path.starts_with(data_dir) || data_canon.is_some_and(|d| path.starts_with(d)) {
        return false;
    }
    // Only inspect components *below* the root, so that a library that itself
    // lives under a dot-path (e.g. `…/.local/share/books`) is not wholly
    // ignored.
    let rel = path.strip_prefix(library_root).unwrap_or(path);
    !rel.components().any(|c| match c {
        std::path::Component::Normal(name) => name.to_str().is_some_and(is_ignored_name),
        _ => false,
    })
}

/// Discover book sources under `library`, in a deterministic (path-sorted)
/// order, so that slug disambiguation is stable across re-scans.
fn discover(library: &Path, data_dir: &Path) -> Vec<BookSource> {
    // The walk must never enter Podspine's own output. A `--data-dir` nested
    // inside the library (a perfectly reasonable
    // `-v /books:/library -e DATA_DIR=/library/.podspine`) holds extracted
    // episodes. A recursive walk would otherwise index them as books and then
    // re-split them: it would feed its own output back in.
    let data_root = data_dir.canonicalize().ok();
    // The containment check uses the canonical root, but the walk descends the
    // path as given, so discovered sources keep the caller's spelling.
    let root = library
        .canonicalize()
        .unwrap_or_else(|_| library.to_path_buf());
    let mut sources = Vec::new();
    let mut visited = HashSet::new();
    walk_library(
        library,
        0,
        &root,
        data_root.as_deref(),
        &mut visited,
        &mut sources,
    );
    sources
}

/// One level of the library walk.
///
/// At every level: files are books in their own right, and a subdirectory is
/// either a book (it holds audio *directly*) or a container to walk (an
/// author, a series, a shelf). That rule makes an `Author/Title/book.m4b`
/// library work without a rearrange, and that is the layout Audiobookshelf,
/// Plex, and Jellyfin all produce.
///
/// The walk does **not** descend into a directory that classifies as a book:
/// the first level that holds audio wins. (Documented consequence: a
/// multi-disc book laid out as `Title/CD1/*.mp3` + `Title/CD2/*.mp3` becomes
/// two books, because `Title/` itself holds no audio.)
fn walk_library(
    dir: &Path,
    depth: usize,
    library_root: &Path,
    data_root: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<BookSource>,
) {
    if depth > MAX_LIBRARY_DEPTH {
        tracing::warn!(
            dir = %dir.display(),
            max = MAX_LIBRARY_DEPTH,
            "library nesting deeper than the walk limit — not descending further"
        );
        return;
    }
    // Canonicalize for the loop guard. A symlink that points back up the tree
    // (or at a sibling already walked) would otherwise recurse until the depth
    // cap, and index the same book twice on the way.
    //
    // Below the root, the walk also enforces containment ([`resolve_inside`]).
    // `is_dir` follows symlinks, so the walk would otherwise index a link
    // inside the library that points at audio elsewhere on the host. That book
    // would then be unplayable, because the serve layer canonicalizes an
    // episode's source and refuses anything outside the library root (TAD §7).
    // A feed whose audio 404s is worse than a book that never appears, so the
    // walk refuses to leave the tree. The root itself is exempt (it defines
    // the tree).
    let real = if depth == 0 {
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
    } else {
        match resolve_inside(dir, library_root) {
            Some(real) => real,
            None => return,
        }
    };
    if data_root.is_some_and(|d| real.starts_with(d)) {
        return;
    }
    if !visited.insert(real) {
        return;
    }

    let mut entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect::<Vec<_>>(),
        Err(err) => {
            // A root failure is an operator error worth a loud log. A
            // subdirectory failure (permissions, a race with a move) is one
            // book lost, not a broken scan.
            if depth == 0 {
                tracing::error!(error = %err, library = %dir.display(), "cannot read library");
            } else {
                tracing::warn!(error = %err, dir = %dir.display(), "cannot read directory — skipping");
            }
            return;
        }
    };
    // Sort, so that slug assignment (and therefore collision suffixes) is
    // identical on every scan, independent of what the filesystem hands back.
    entries.sort();

    // Below the root, a directory that holds audio IS a book (or several), and
    // the walk does not descend into it: the first level with audio wins. That
    // rule keeps a book's own `extras/` or `bonus/` folder from becoming
    // another book. The documented cost: a mixed `Author/{loose.m4b, Title/…}`
    // yields only `loose.m4b`.
    if depth > 0 {
        let books: Vec<BookSource> = classify_dir(dir)
            .into_iter()
            .filter(|src| source_is_inside(src, library_root))
            .collect();
        if !books.is_empty() {
            out.extend(books);
            return;
        }
    }

    // This is the root, or a container. The loop emits files and
    // subdirectories in one path-sorted pass, which is exactly the order
    // discovery has always produced. It matters: the order decides which of
    // two same-named books gets the bare slug and which gets the `-2` suffix,
    // and that slug is the book id whose capability feed id is preserved
    // across re-scans. A pass that emitted all files ahead of all directories
    // would swap `Dracula.m4b` and `Dracula/`: it would hand each subscriber
    // the other audiobook under the URL they already have.
    //
    // A root file is a book in its own right, never grouped (several loose
    // `.mp3` at the root are separate books, not one book's tracks), again as
    // before.
    for path in entries {
        if path.is_file() {
            if depth > 0 {
                continue;
            }
            if is_drm(&path) {
                tracing::warn!(
                    path = %path.display(),
                    "skipping DRM-protected file (Podspine ships no circumvention)"
                );
            } else if is_audio(&path) {
                let src = BookSource::File(path);
                if source_is_inside(&src, library_root) {
                    out.push(src);
                }
            }
        } else if path.is_dir() && !is_ignored_dir(&path) {
            walk_library(&path, depth + 1, library_root, data_root, visited, out);
        }
    }
}

/// Whether a discovered source really lives inside the library.
///
/// A symlinked *file* escapes the directory guard in [`walk_library`], and it
/// would land in a feed that the serve layer then refuses to play. The check
/// logs rejected sources; it never drops them silently. A book missing from
/// the UI with nothing in the log is the worst version of this.
fn source_is_inside(src: &BookSource, library_root: &Path) -> bool {
    resolve_inside(src.path(), library_root).is_some()
}

/// Canonicalize `path` and keep it only if it resolves inside `root`. This is
/// the symlink-containment rule shared by the walk, source vetting, and
/// MP3-track collection. The serve layer canonicalizes every source and
/// refuses anything outside the library root (TAD §7), so an indexed escapee
/// would publish audio that 404s. The check drops, with a warning, a path that
/// resolves outside the root or cannot be resolved at all. A book missing from
/// the UI with nothing in the log is the worst version of this.
fn resolve_inside(path: &Path, root: &Path) -> Option<PathBuf> {
    match path.canonicalize() {
        Ok(real) if real.starts_with(root) => Some(real),
        Ok(real) => {
            tracing::warn!(
                path = %path.display(),
                target = %real.display(),
                "skipping a link that leaves the library root"
            );
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "skipping an unreadable source");
            None
        }
    }
}

/// Directories that the walk never enters: dotfiles (`.stfolder`, `.git`, …)
/// and the thumbnail caches that NAS software scatters through a share. None
/// of them hold audiobooks, and a walk of them is pure cost on a large
/// library.
fn is_ignored_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(is_ignored_name)
}

/// The single ignore rule shared by the walk ([`is_ignored_dir`]) and the
/// watch-event filter ([`watch_path_is_relevant`]), so that the two never
/// disagree on what to skip: dotfiles/dotdirs and the NAS junk caches
/// `@eaDir` / `lost+found`.
fn is_ignored_name(name: &str) -> bool {
    name.starts_with('.') || name == "@eaDir" || name == "lost+found"
}

/// Classify a per-book subfolder. Prefer a splittable `.m4b`/`.m4a`. A lone
/// `.mp3` is a single-file book. Several `.mp3`s are a multi-track folder
/// (Task 3.3). A folder with no audio yields nothing.
fn classify_dir(dir: &Path) -> Vec<BookSource> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut m4x = Vec::new();
    let mut mp3 = Vec::new();
    // Tier-2 containers (Ogg/Opus/FLAC): a lone one is a book in its own
    // folder, exactly as it would be at the library root. A folder that holds
    // several is not a Tier-2 equivalent of an MP3 folder (that path only
    // reads `.mp3` tracks). So it stays unclassified; a half-ingest would be
    // worse.
    let mut tier2 = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if !path.is_file() {
            continue;
        }
        // The scan refuses DRM wherever it turns up in the tree, loudly
        // (PRD W5).
        if is_drm(&path) {
            tracing::warn!(
                path = %path.display(),
                "skipping DRM-protected file (Podspine ships no circumvention)"
            );
            continue;
        }
        if !is_audio(&path) {
            continue;
        }
        match ext_lower(&path).as_deref() {
            Some("m4b") | Some("m4a") => m4x.push(path),
            Some("mp3") => mp3.push(path),
            // Everything else that `is_audio` admits is Tier-2.
            _ => tier2.push(path),
        }
    }
    m4x.sort();
    mp3.sort();
    tier2.sort();

    if !m4x.is_empty() {
        // One `.m4b`/`.m4a` is one whole book, so a folder that holds several
        // holds several books: typically an author folder of single-file
        // audiobooks. (`.mp3` is the opposite: several of those are usually
        // one book's tracks.)
        m4x.into_iter().map(BookSource::File).collect()
    } else if mp3.len() == 1 {
        vec![BookSource::File(mp3.into_iter().next().unwrap())]
    } else if !mp3.is_empty() {
        vec![BookSource::Mp3Folder(dir.to_path_buf())]
    } else if tier2.len() == 1 {
        vec![BookSource::File(tier2.into_iter().next().unwrap())]
    } else {
        if tier2.len() > 1 {
            tracing::warn!(
                dir = %dir.display(),
                count = tier2.len(),
                "several Ogg/Opus/FLAC files in one folder — skipped (only .mp3 folders are ingested as a book's tracks); give each book its own folder, or add a .cue"
            );
        }
        Vec::new()
    }
}

/// Choose this source's slug, which becomes its `book.id`: reserve `base` if
/// it is free, else `base-2`, `base-3`, … Insert the chosen slug into `seen`
/// and return it.
///
/// Two constraints apply, both about identity theft:
///
/// - The slug is unique within this scan, so two same-named books get `x` and
///   `x-2`.
/// - **The slug is never one that a live index row holds for a different
///   source.** `upsert_book` preserves a book's capability `feed_id` per id.
///   A hand-over of an existing id to another file would keep the
///   subscriber's URL alive and swap the audiobook underneath it. That is the
///   one failure this crate must never produce, and dedup within a single
///   scan cannot see it: the walk may discover the colliding book later, or
///   may not rediscover it at all.
///
/// A row whose source no longer exists still owns its id during this scan
/// (see [`owns_a_different_source`] for why the guard never frees an id
/// early). The same reconcile prunes that row afterwards ([`prune_orphans`]),
/// so the id frees up for the next scan.
fn assign_slug(base: &str, source: &Path, index: &Index, seen: &mut HashSet<String>) -> String {
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        // A candidate refused because someone else owns it is NOT consumed.
        // The walk often discovers the rightful owner later in the same scan,
        // and that book must still be able to claim its slug. If this loop
        // consumed the slug, it would push that book onto a fresh id: a new
        // capability feed, plus a stale row that still serves the old one.
        if !seen.contains(&candidate) && !owns_a_different_source(index, &candidate, source) {
            seen.insert(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// Whether `slug` is an indexed book built from a *different* file than
/// `source`.
///
/// The guard deliberately does NOT consult the existence of that book's file.
/// An earlier version freed the id when the stored source was gone (to let a
/// moved book reclaim it). But that reopened the exact theft this guard
/// exists to stop: a newcomer that sorts before a moved book's new location
/// would inherit the vacated id, and its preserved capability feed, while the
/// moved book was pushed onto a fresh feed (Greptile). No content identity
/// can tell "the moved book" from "a different book of the same name". So the
/// safe rule is uniform: a live index row owns its id until [`prune_orphans`]
/// retires it, one reconcile later. A book therefore keeps its feed across an
/// in-place re-ingest (same path) but not across a move. That is the same
/// feed-rotates-on-rename contract the flat scanner had.
///
/// Index errors count as "not owned": a failed lookup must not rename every
/// book.
fn owns_a_different_source(index: &Index, slug: &str, source: &Path) -> bool {
    let Ok(Some(existing)) = index.get_book(slug) else {
        return false;
    };
    // Compare canonically where both paths resolve. The stored path is
    // canonical; a discovered one need not be (`/tmp` is a symlink to
    // `/private/var` on macOS). Fall back to the raw strings when the stored
    // file is gone.
    match (
        Path::new(&existing.source_path).canonicalize(),
        source.canonicalize(),
    ) {
        (Ok(indexed), Ok(discovered)) => indexed != discovered,
        _ => existing.source_path != source.to_string_lossy(),
    }
}

/// A path's extension, lowercased.
fn ext_lower(p: &Path) -> Option<String> {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Stream-copy output container extension for an audio codec. Each Tier-2
/// codec needs its own container (mp4 cannot hold FLAC/Vorbis). Unknown codecs
/// default to `m4a` (the Tier-1 case). There is no re-encode; this only names
/// the muxer.
fn output_ext(codec: Option<&str>) -> &'static str {
    match codec {
        Some("mp3") => "mp3",
        Some("flac") => "flac",
        Some("vorbis") => "ogg",
        Some("opus") => "opus",
        _ => "m4a", // aac/alac and any unknown codec
    }
}

/// Whether a probed codec is one that podcatchers play reliably.
///
/// MP3 and AAC are the two that every client handles. An unknown codec
/// (`None`: a probe that named no audio codec) counts as safe. A wrong guess
/// there would burn a whole re-encode on a file that probably plays fine.
fn is_podcast_safe(codec: Option<&str>) -> bool {
    matches!(codec, Some("mp3" | "aac") | None)
}

/// The [`Encoding`] that produces this book's episodes (Task 5.2): a re-encode
/// only when transcoding is on *and* the source codec is not podcast-safe.
/// Everything else is a stream copy.
fn encoding_for(codec: Option<&str>, mode: TranscodeMode) -> Encoding {
    if is_podcast_safe(codec) {
        return Encoding::Copy;
    }
    match mode {
        TranscodeMode::Off => Encoding::Copy,
        TranscodeMode::Aac => Encoding::Aac,
        TranscodeMode::Mp3 => Encoding::Mp3,
    }
}

/// The container extension for one produced episode: the transcode target's
/// for a re-encode, else one that matches the source codec (Task 3.9).
fn episode_ext(codec: Option<&str>, enc: Encoding) -> &'static str {
    match enc {
        Encoding::Copy => output_ext(codec),
        Encoding::Aac => "m4a",
        Encoding::Mp3 => "mp3",
    }
}

/// The `book.transcode` value that an already-indexed book *should* carry
/// under the current setting, judged from its file extension alone. The
/// idempotency guard runs before the probe and must not force one.
///
/// `None` means "this cannot be known without a probe, so accept whatever is
/// stored". An `.m4a`/`.m4b` may hold AAC (podcast-safe, copied) or ALAC
/// (re-encoded). A guess either way would make the guard either miss a real
/// toggle, or re-ingest the book on every single scan. An ALAC book therefore
/// keeps its existing episodes until its source changes.
/// `force_reingest = true` in its `.podspine.toml` picks up the new setting
/// immediately.
fn expected_transcode(input: &Path, mode: TranscodeMode) -> Option<TranscodeMode> {
    if !mode.is_on() {
        // Nothing may stay transcoded once the flag is off.
        return Some(TranscodeMode::Off);
    }
    match ext_lower(input).as_deref() {
        // Tier-2 containers: never podcast-safe, so they get the target codec.
        Some("flac" | "ogg" | "oga" | "opus") => Some(mode),
        // MP3 is podcast-safe; the scan never re-encodes it.
        Some("mp3") => Some(TranscodeMode::Off),
        _ => None,
    }
}

/// File extension for an extracted cover, from its ffprobe codec name. Cover art
/// is almost always MJPEG or PNG; anything else defaults to `jpg`.
fn cover_ext(codec: Option<&str>) -> &'static str {
    match codec {
        Some("png") => "png",
        _ => "jpg", // mjpeg/mjpg/jpeg and any unknown codec
    }
}

/// Whether a path is an audio file Podspine can ingest.
fn is_audio(p: &Path) -> bool {
    ext_lower(p)
        .map(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}

/// Whether a path has a DRM extension that Podspine refuses to ingest.
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

/// Lowercase ASCII slug: keep alphanumerics; each run of anything else becomes
/// a single `-`. The fallback is `"book"`.
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
    use podspine_test_support::{
        ScratchDir, skip, skip_unless_ffmpeg, synth_sine, synth_three_chapters, synth_with_cover,
    };
    use std::process::Command;

    /// Crate-prefixed [`podspine_test_support::scratch`], so that scanner test
    /// dirs cannot collide with another crate's over a shared short name.
    fn scratch(name: &str) -> ScratchDir {
        podspine_test_support::scratch(&format!("scan-{name}"))
    }

    #[test]
    fn an_mp3_folder_with_no_tracks_is_a_typed_error() {
        // A folder of cover art and metadata with no audio is a user mistake.
        // It must surface as `EmptyFolder`, not as a crash, and not as a
        // silently empty book.
        let dir = scratch("empty-mp3-folder");
        std::fs::write(dir.join("cover.jpg"), b"img").unwrap();
        std::fs::write(dir.join("notes.txt"), b"no audio here").unwrap();
        let index = Index::open_in_memory().unwrap();
        let data = dir.join("data");

        let err = scan_mp3_folder(
            &dir,
            "book-id",
            &data,
            &index,
            &BookOverrides::default(),
            &dir,
        )
        .expect_err("a folder with no mp3s must not scan");

        assert!(matches!(err, ScanError::EmptyFolder(_)), "got {err:?}");
    }

    /// Synthesize an AAC file. When `chapters` is true, embed three 10-second
    /// chapters.
    fn synth(dir: &Path, chapters: bool) -> PathBuf {
        if chapters {
            synth_three_chapters(dir, "chapters.m4a")
        } else {
            synth_sine(dir, "flat.m4a", 12.0)
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// Synthesize a real MP3 with an optional `track` tag. Return `None` if
    /// the ffmpeg build has no MP3 encoder (the test then skips).
    fn synth_mp3(dir: &Path, name: &str, track: Option<u32>, dur: u32) -> Option<PathBuf> {
        std::fs::create_dir_all(dir).unwrap();
        let out = dir.join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=300:duration={dur}"),
        ]);
        if let Some(t) = track {
            cmd.args(["-metadata", &format!("track={t}")]);
        }
        cmd.args(["-c:a", "libmp3lame"]).arg(&out);
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        ok.then_some(out)
    }

    #[test]
    fn mp3_folder_serves_tracks_in_place_in_track_order() {
        skip_unless_ffmpeg!();
        let root = scratch("mp3-order");
        let book = root.join("A Folder Book");
        // Filenames are deliberately NOT in track order; durations tag each track.
        let a = synth_mp3(&book, "z-first.mp3", Some(1), 2);
        let b = synth_mp3(&book, "a-second.mp3", Some(2), 4);
        let c = synth_mp3(&book, "m-third.mp3", Some(3), 2);
        if a.is_none() || b.is_none() || c.is_none() {
            skip!("ffmpeg has no libmp3lame encoder");
        }

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(summary.indexed, 1);
        assert_eq!(summary.skipped, 0);

        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 1);
        let eps = index.episodes_for_book(&books[0].id).unwrap();
        assert_eq!(eps.len(), 3, "one episode per MP3");

        // Track order (1,2,3) gives durations of ~2,4,2 seconds. Filename
        // order would give ~4,2,2.
        assert!(
            (eps[1].duration_sec - 4.0).abs() < 0.6,
            "middle is track 2 (4s)"
        );
        let book_c = book.canonicalize().unwrap();
        for (i, e) in eps.iter().enumerate() {
            assert_eq!(e.idx, i as i64);
            let p = PathBuf::from(&e.file_path);
            assert!(p.exists(), "track file on disk");
            assert!(
                p.starts_with(&book_c),
                "served in place from the library (canonical), not copied"
            );
            assert!(!p.starts_with(&data), "nothing served from the data dir");
            assert_eq!(
                e.source_path, e.file_path,
                "source_path marks the in-place track"
            );
            assert!(e.file_path.ends_with(".mp3"));
            assert!(e.byte_length > 0);
        }
        for w in eps.windows(2) {
            assert!(w[0].pubdate_epoch < w[1].pubdate_epoch, "pubDates increase");
        }

        // The scan wrote no per-track copies under `<data>/books/<id>/`.
        let book_out = data.join("books").join(&books[0].id);
        for i in 1..=eps.len() {
            assert!(
                !book_out.join(format!("{i:03}.mp3")).exists(),
                "MP3-folder track {i} is not copied to the data dir"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mp3_folder_falls_back_to_filename_order_on_missing_track() {
        skip_unless_ffmpeg!();
        let root = scratch("mp3-fallback");
        let book = root.join("Mixed Book");
        // One track is tagged and one is not. The mixed set falls back to the
        // filename sort (01 before 02).
        let a = synth_mp3(&book, "01-intro.mp3", None, 2);
        let b = synth_mp3(&book, "02-body.mp3", Some(5), 4);
        if a.is_none() || b.is_none() {
            skip!("ffmpeg has no libmp3lame encoder");
        }

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        assert_eq!(
            scan_library(&root, &data, &index, ScanOptions::default()).indexed,
            1
        );
        let books = index.list_books().unwrap();
        let eps = index.episodes_for_book(&books[0].id).unwrap();
        assert_eq!(eps.len(), 2);
        // Filename order: 01-intro (2s) then 02-body (4s).
        assert!((eps[0].duration_sec - 2.0).abs() < 0.6, "01-intro first");
        assert!((eps[1].duration_sec - 4.0).abs() < 0.6, "02-body second");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cue_sidecar_overrides_embedded_chapters() {
        skip_unless_ffmpeg!();
        // synth() embeds THREE 10-second chapters. A sibling .cue defines only
        // TWO chapters (0–5s, 5–30s). The cue chapters must win.
        let dir = scratch("cue-sidecar");
        let input = synth(&dir, true); // chapters.m4a, 3 embedded chapters
        std::fs::write(
            input.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"Front\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Back\"\n  INDEX 01 00:05:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 2, "cue's 2 chapters win over 3 embedded");
        assert_eq!(eps[0].title, "Front");
        assert_eq!(eps[1].title, "Back");

        // `force_embedded` ignores the sidecar; the scan is back to 3 embedded
        // chapters.
        let data2 = dir.join("data2");
        let index2 = Index::open_in_memory().unwrap();
        let book2 = scan_book_as(
            &input,
            "forced",
            &data2,
            &index2,
            ScanOptions {
                force_embedded: true,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            index2.episodes_for_book(&book2.id).unwrap().len(),
            3,
            "force_embedded uses the 3 embedded chapters"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saver_mode_records_real_sizes_and_starts_but_deletes_the_files() {
        skip_unless_ffmpeg!();
        let dir = scratch("saver-mode");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = synth(&dir, true); // 3 chapters at 0s / 10s / 20s
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &input,
            "saver-book",
            &data,
            &index,
            ScanOptions {
                storage: StorageMode::Saver,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 3);
        for (i, e) in eps.iter().enumerate() {
            // The scan records the real enclosure length even though the file
            // is gone.
            assert!(e.byte_length > 0, "byte_length recorded for chapter {i}");
            // The scan persists the chapter start offset, so that http can
            // regenerate the file.
            assert!(
                (e.start_sec - (i as f64) * 10.0).abs() < 0.5,
                "start_sec ~= {}s, got {}",
                i * 10,
                e.start_sec
            );
            // Saver mode deleted the split file (the peak extra disk usage is
            // one chapter).
            assert!(
                !Path::new(&e.file_path).exists(),
                "saver deletes the split file: {}",
                e.file_path
            );
        }

        // Idempotency: a re-scan at the same mtime is a no-op even though the
        // files are absent (the index entry alone satisfies the check in saver
        // mode).
        let again = scan_book_as(
            &input,
            "saver-book",
            &data,
            &index,
            ScanOptions {
                storage: StorageMode::Saver,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(again.id, book.id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saver_reingests_migrated_rows_with_zero_start_sec() {
        skip_unless_ffmpeg!();
        let dir = scratch("saver-migrated");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = synth(&dir, true); // 3 chapters at 0s / 10s / 20s
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        // Run a normal full-mode ingest first: files land on disk, and the
        // scan records real offsets.
        let book = scan_book_as(
            &input,
            "mig",
            &data,
            &index,
            ScanOptions::default(),
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert!(
            eps.iter().any(|e| e.start_sec > 0.0),
            "full ingest records real chapter offsets"
        );

        // Simulate a pre-5.1 -> 5.1 migration: back-fill `start_sec` = 0
        // everywhere.
        for e in &eps {
            let mut zeroed = e.clone();
            zeroed.start_sec = 0.0;
            index.upsert_episode(&zeroed).unwrap();
        }

        // Re-scan at the SAME mtime in saver mode. The zeroed non-first chapters
        // must force a one-time re-split, not an idempotent skip.
        let book2 = scan_book_as(
            &input,
            "mig",
            &data,
            &index,
            ScanOptions {
                storage: StorageMode::Saver,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(book2.id, book.id);
        let eps2 = index.episodes_for_book(&book.id).unwrap();
        assert!(
            (eps2[1].start_sec - 10.0).abs() < 0.5 && (eps2[2].start_sec - 20.0).abs() < 0.5,
            "real start offsets are restored by the forced re-split: {:?}",
            eps2.iter().map(|e| e.start_sec).collect::<Vec<_>>()
        );
        assert!(
            eps2.iter().all(|e| !Path::new(&e.file_path).exists()),
            "saver re-split leaves no files on disk"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthesize an audio file with a specific encoder. Return `None` if the
    /// ffmpeg build lacks that encoder (the test then skips).
    fn synth_encoded(dir: &Path, name: &str, enc: &[&str], dur: u32) -> Option<PathBuf> {
        std::fs::create_dir_all(dir).unwrap();
        let out = dir.join(name);
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=300:duration={dur}"),
            ])
            .args(enc)
            .arg(&out)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(out)
    }

    #[test]
    fn output_ext_by_codec() {
        assert_eq!(output_ext(Some("aac")), "m4a");
        assert_eq!(output_ext(Some("alac")), "m4a");
        assert_eq!(output_ext(Some("mp3")), "mp3");
        assert_eq!(output_ext(Some("flac")), "flac");
        assert_eq!(output_ext(Some("vorbis")), "ogg");
        assert_eq!(output_ext(Some("opus")), "opus");
        assert_eq!(output_ext(None), "m4a");
    }

    // ---- opt-in transcoding (Task 5.2) ----

    #[test]
    fn transcoding_only_touches_non_podcast_safe_codecs() {
        use TranscodeMode::{Aac, Mp3, Off};
        // `Off` is the default: the scan never re-encodes anything.
        for codec in ["mp3", "aac", "flac", "vorbis", "opus", "alac"] {
            assert_eq!(encoding_for(Some(codec), Off), Encoding::Copy, "{codec}");
        }
        // On: the scan still stream-copies MP3/AAC sources; they already play
        // everywhere.
        assert_eq!(encoding_for(Some("mp3"), Aac), Encoding::Copy);
        assert_eq!(encoding_for(Some("aac"), Aac), Encoding::Copy);
        // On: the scan re-encodes the formats that podcatchers do not play to
        // the chosen target.
        for codec in ["flac", "vorbis", "opus", "alac"] {
            assert_eq!(encoding_for(Some(codec), Aac), Encoding::Aac, "{codec}");
            assert_eq!(encoding_for(Some(codec), Mp3), Encoding::Mp3, "{codec}");
        }
        // The scan leaves an unnamed codec alone; it does not re-encode on a
        // guess.
        assert_eq!(encoding_for(None, Aac), Encoding::Copy);
    }

    #[test]
    fn episode_ext_follows_the_transcode_target() {
        assert_eq!(episode_ext(Some("flac"), Encoding::Copy), "flac");
        assert_eq!(episode_ext(Some("flac"), Encoding::Aac), "m4a");
        assert_eq!(episode_ext(Some("flac"), Encoding::Mp3), "mp3");
        assert_eq!(episode_ext(Some("vorbis"), Encoding::Aac), "m4a");
    }

    #[test]
    fn expected_transcode_guards_a_toggle_without_probing() {
        // The persisted labels, via the canonical methods (all values pinned).
        assert_eq!(TranscodeMode::Off.label(), "off");
        assert_eq!(TranscodeMode::Aac.label(), "aac");
        assert_eq!(TranscodeMode::Mp3.label(), "mp3");
        let flac = Path::new("/lib/book.flac");
        let m4b = Path::new("/lib/book.m4b");
        let mp3 = Path::new("/lib/book.mp3");
        // Flag off: nothing may stay transcoded, independent of the container.
        assert_eq!(
            expected_transcode(flac, TranscodeMode::Off),
            Some(TranscodeMode::Off)
        );
        assert_eq!(
            expected_transcode(m4b, TranscodeMode::Off),
            Some(TranscodeMode::Off)
        );
        // Flag on: the scan always re-encodes a Tier-2 container.
        assert_eq!(
            expected_transcode(flac, TranscodeMode::Aac),
            Some(TranscodeMode::Aac)
        );
        assert_eq!(
            expected_transcode(Path::new("/lib/b.opus"), TranscodeMode::Mp3),
            Some(TranscodeMode::Mp3)
        );
        // Flag on: the scan never re-encodes MP3.
        assert_eq!(
            expected_transcode(mp3, TranscodeMode::Aac),
            Some(TranscodeMode::Off)
        );
        // Flag on: an mp4-family file is unknowable without a probe (AAC or
        // ALAC). The guard abstains; it does not re-ingest the book on every
        // scan.
        assert_eq!(expected_transcode(m4b, TranscodeMode::Aac), None);
    }

    /// Acceptance: a FLAC source produces a playable AAC feed when transcoding
    /// is on. That means the right container, the right codec, and an
    /// `enclosure length` that is the real output size.
    #[test]
    fn flac_with_cue_transcodes_to_aac_when_enabled() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-aac");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 20) else {
            skip!("no flac encoder");
        };
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:10:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &flac,
            "b",
            &data,
            &index,
            ScanOptions {
                transcode: TranscodeMode::Aac,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            book.transcode,
            Some(TranscodeMode::Aac),
            "the effective mode is persisted"
        );

        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 2, "cue defines two chapters");
        for e in &eps {
            assert!(
                e.file_path.ends_with(".m4a"),
                "AAC lands in an mp4 container: {}",
                e.file_path
            );
            let real = std::fs::metadata(&e.file_path).unwrap().len() as i64;
            assert_eq!(
                e.byte_length, real,
                "enclosure length must be the real output size"
            );
            assert_eq!(
                probe(Path::new(&e.file_path))
                    .unwrap()
                    .audio_codec
                    .as_deref(),
                Some("aac"),
                "episode must actually be AAC"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The server cannot serve a chapterless non-podcast-safe file in place
    /// once it is re-encoded: the bytes clients get exist only under
    /// `<data_dir>`.
    #[test]
    fn chapterless_flac_transcodes_into_the_data_dir_instead_of_serving_in_place() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-whole");
        let Some(flac) = synth_encoded(&dir, "whole.flac", &["-c:a", "flac"], 8) else {
            skip!("no flac encoder");
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &flac,
            "w",
            &data,
            &index,
            ScanOptions {
                transcode: TranscodeMode::Aac,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1, "no chapters -> one whole-file episode");
        let e = &eps[0];
        assert!(
            e.source_path.is_empty(),
            "a re-encoded episode is NOT served in place"
        );
        assert!(e.file_path.ends_with(".m4a"), "{}", e.file_path);
        assert!(e.file_path.starts_with(data.to_str().unwrap()));
        assert!(!e.needs_faststart, "the re-encode is already faststart");
        assert_eq!(
            e.byte_length,
            std::fs::metadata(&e.file_path).unwrap().len() as i64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The MP3 fallback target, for clients that still do not play AAC. The
    /// test skips when this ffmpeg has no `libmp3lame`.
    #[test]
    fn flac_transcodes_to_mp3_when_that_target_is_chosen() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-mp3");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 6) else {
            skip!("no flac encoder");
        };
        if synth_encoded(&dir, "probe.mp3", &["-c:a", "libmp3lame"], 1).is_none() {
            skip!("no libmp3lame encoder");
        }
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &flac,
            "m",
            &data,
            &index,
            ScanOptions {
                transcode: TranscodeMode::Mp3,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(book.transcode, Some(TranscodeMode::Mp3));
        let eps = index.episodes_for_book(&book.id).unwrap();
        let e = &eps[0];
        assert!(e.file_path.ends_with(".mp3"), "{}", e.file_path);
        assert_eq!(
            probe(Path::new(&e.file_path))
                .unwrap()
                .audio_codec
                .as_deref(),
            Some("mp3")
        );
        assert_eq!(
            e.byte_length,
            std::fs::metadata(&e.file_path).unwrap().len() as i64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan deliberately overrides `saver` for a transcoded book. A
    /// re-encode is not byte-reproducible, so its episodes must stay on disk
    /// (the serve layer refuses to regenerate or evict them).
    #[test]
    fn a_transcoded_saver_book_keeps_its_episodes_on_disk() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-saver");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            skip!("no flac encoder");
        };
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:06:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &flac,
            "s",
            &data,
            &index,
            ScanOptions {
                storage: StorageMode::Saver,
                transcode: TranscodeMode::Aac,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            book.storage_mode,
            Some(StorageMode::Saver),
            "the requested mode is still recorded"
        );
        assert_eq!(book.transcode, Some(TranscodeMode::Aac));
        for e in index.episodes_for_book(&book.id).unwrap() {
            assert!(
                Path::new(&e.file_path).exists(),
                "a saver book would have deleted {}",
                e.file_path
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A target change rewrites every episode into a new container. The files
    /// in the old one are unreferenced from that moment, and nothing else
    /// reclaims them (the cache eviction only touches regenerable books, and a
    /// transcoded book is never regenerable). So the ingest must sweep them.
    #[test]
    fn only_episode_files_and_their_temporaries_are_swept() {
        let dir = scratch("sweep-predicate");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "001.flac",
            "002.flac",
            "003.part.flac",
            "001.m4a",
            "cover.jpg",
            "notes.txt",
            "NOTES", // no extension at all
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        remove_episode_files_in_other_containers(&dir, "m4a");

        let left = |name: &str| dir.join(name).exists();
        // The sweep removes the previous container's episodes and temporaries.
        assert!(!left("001.flac"));
        assert!(!left("002.flac"));
        assert!(!left("003.part.flac"));
        // The sweep leaves the new container's files for the ingest to
        // overwrite.
        assert!(left("001.m4a"));
        // An extracted cover is not an episode file and must survive.
        assert!(left("cover.jpg"));
        assert!(left("notes.txt"));
        assert!(left("NOTES"), "a file with no extension is not an episode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_the_transcode_target_reclaims_the_previous_container() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-reclaim");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            skip!("no flac encoder");
        };
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:06:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let scan = |mode: TranscodeMode| {
            let book = scan_book_as(
                &flac,
                "r",
                &data,
                &index,
                ScanOptions {
                    transcode: mode,
                    ..Default::default()
                },
                &BookOverrides::default(),
            )
            .unwrap();
            index
                .episodes_for_book(&book.id)
                .unwrap()
                .into_iter()
                .map(|e| e.file_path)
                .collect::<Vec<_>>()
        };

        let copied = scan(TranscodeMode::Off);
        assert!(copied.iter().all(|p| p.ends_with(".flac")), "{copied:?}");
        let aac = scan(TranscodeMode::Aac);
        assert!(aac.iter().all(|p| p.ends_with(".m4a")), "{aac:?}");
        for old in &copied {
            assert!(
                !Path::new(old).exists(),
                "stream-copied episode left behind: {old}"
            );
        }
        let mp3 = scan(TranscodeMode::Mp3);
        // A missing libmp3lame is a skip, not a failure: the sweep is what is
        // under test.
        if mp3.iter().all(|p| p.ends_with(".mp3")) {
            for old in &aac {
                assert!(
                    !Path::new(old).exists(),
                    "AAC episode left behind after switching to MP3: {old}"
                );
            }
        }
        // The cover is not an episode file and must survive every sweep.
        let book_dir = data.join("books").join("r");
        assert!(
            std::fs::read_dir(&book_dir).unwrap().count() > 0,
            "the book dir still holds this run's episodes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep runs after the index update, never before. Until then the
    /// server still serves the old files, and this scan can sit inside a
    /// re-encode for minutes. A failed re-ingest must therefore leave the
    /// previous episodes playable; it must not delete them and then fail to
    /// replace them.
    #[test]
    fn a_failed_reingest_leaves_the_previous_episodes_in_place() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-failed-reingest");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            skip!("no flac encoder");
        };
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:06:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book_as(
            &flac,
            "f",
            &data,
            &index,
            ScanOptions::default(),
            &BookOverrides::default(),
        )
        .unwrap();
        let served: Vec<String> = index
            .episodes_for_book(&book.id)
            .unwrap()
            .into_iter()
            .map(|e| e.file_path)
            .collect();
        assert!(served.iter().all(|p| Path::new(p).exists()));

        // Block the transcode: a directory where the first episode wants to land
        // makes the atomic rename fail, so the ingest errors mid-book.
        std::fs::create_dir_all(data.join("books").join("f").join("001.m4a")).unwrap();
        let err = scan_book_as(
            &flac,
            "f",
            &data,
            &index,
            ScanOptions {
                transcode: TranscodeMode::Aac,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .expect_err("the blocked rename must fail the ingest");
        assert!(matches!(err, ScanError::Split(_)), "{err:?}");

        for p in &served {
            assert!(
                Path::new(p).exists(),
                "a failed re-ingest deleted a still-indexed episode: {p}"
            );
        }
        assert_eq!(
            index
                .episodes_for_book(&book.id)
                .unwrap()
                .into_iter()
                .map(|e| e.file_path)
                .collect::<Vec<_>>(),
            served,
            "the index still points at the episodes that still exist"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A book indexed before Task 5.2 carries `transcode = None`. Such a book
    /// was necessarily stream-copied, and that is what `Off` means. So an
    /// upgrade must NOT re-ingest it, while a genuinely stale mode must force
    /// a re-ingest.
    #[test]
    fn a_pre_transcode_row_is_not_reingested_but_a_stale_mode_is() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-upgrade");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            skip!("no flac encoder");
        };
        // The book is chaptered, so the episodes land under the data dir. (A
        // chapterless book is served in place, and its `file_path` is the
        // library file.)
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:06:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let scan = || {
            scan_book_as(
                &flac,
                "u",
                &data,
                &index,
                ScanOptions::default(),
                &BookOverrides::default(),
            )
            .unwrap()
        };
        let stored_mode = |mode: Option<TranscodeMode>| {
            let book = index.get_book("u").unwrap().unwrap();
            index
                .upsert_book(&BookRow {
                    transcode: mode,
                    ..book
                })
                .unwrap();
        };
        // A sentinel in the episode file: an early return leaves it, and a
        // re-ingest overwrites it. (Nothing else in a `full`-mode scan
        // rewrites the file.)
        let sentinel = |ep: &str| std::fs::write(ep, b"sentinel").unwrap();
        let survived = |ep: &str| std::fs::read(ep).unwrap() == b"sentinel";

        let book = scan();
        assert_eq!(book.transcode, Some(TranscodeMode::Off));
        let ep = index.episodes_for_book(&book.id).unwrap()[0]
            .file_path
            .clone();

        // Pre-5.2 row: `None` means the same thing as `Off`, so there is no
        // re-split.
        stored_mode(None);
        sentinel(&ep);
        scan();
        assert!(
            survived(&ep),
            "an upgraded database must not re-split every stream-copied book"
        );

        // A row that claims a transcode while the flag is off is genuinely
        // stale.
        stored_mode(Some(TranscodeMode::Aac));
        sentinel(&ep);
        let back = scan();
        assert!(
            !survived(&ep),
            "a stale transcode mode must force a re-ingest"
        );
        assert_eq!(back.transcode, Some(TranscodeMode::Off));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flag flip changes the container and every byte length, and touches no
    /// source mtime. So the idempotency guard must re-ingest instead of
    /// serving stale rows. And an unchanged setting must NOT re-ingest.
    #[test]
    fn toggling_transcode_reingests_a_flac_book() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-transcode-toggle");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 8) else {
            skip!("no flac encoder");
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let scan = |opts: ScanOptions| {
            scan_book_as(&flac, "t", &data, &index, opts, &BookOverrides::default()).unwrap()
        };

        // Copy-first by default: a .flac episode.
        let off = scan(ScanOptions::default());
        assert_eq!(off.transcode, Some(TranscodeMode::Off));
        let before = index.episodes_for_book(&off.id).unwrap();
        assert!(before[0].file_path.ends_with(".flac"));

        // Flag on: the scan re-ingests, even though the source is untouched.
        let on = scan(ScanOptions {
            transcode: TranscodeMode::Aac,
            ..Default::default()
        });
        assert_eq!(on.transcode, Some(TranscodeMode::Aac));
        let after = index.episodes_for_book(&on.id).unwrap();
        assert!(
            after[0].file_path.ends_with(".m4a"),
            "expected a re-ingest, got {}",
            after[0].file_path
        );
        // This book is chapterless, so with the flag off it was served in
        // place: `before` pointed at the library file itself, and that file
        // must never be touched.
        assert_eq!(before[0].file_path, before[0].source_path);
        assert!(Path::new(&before[0].source_path).exists());
        assert!(
            after[0].source_path.is_empty(),
            "a transcoded episode is no longer served in place"
        );
        assert_ne!(
            after[0].byte_length, before[0].byte_length,
            "byte lengths are re-recorded from the re-encoded file"
        );
        assert_eq!(
            after[0].guid, before[0].guid,
            "guid is mtime-keyed, so clients don't re-download the whole feed"
        );

        // Flag back off: the scan re-ingests to a stream copy again, in place.
        // It reclaims the transcoded copy under the data dir; the copy is not
        // orphaned.
        let transcoded_copy = after[0].file_path.clone();
        let back = scan(ScanOptions::default());
        assert_eq!(back.transcode, Some(TranscodeMode::Off));
        let now = index.episodes_for_book(&back.id).unwrap();
        assert!(now[0].file_path.ends_with(".flac"));
        assert_eq!(
            now[0].file_path, now[0].source_path,
            "served in place again"
        );
        assert!(
            !Path::new(&transcoded_copy).exists(),
            "the transcoded copy must not be left behind under the data dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_with_cue_splits_by_sidecar_no_reencode() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-cue");
        // FLAC has no titled embedded chapters, so it relies on a .cue
        // (PRD S7).
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 20) else {
            skip!("no flac encoder");
        };
        std::fs::write(
            flac.with_extension("cue"),
            "TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:10:00\n",
        )
        .unwrap();
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&flac, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 2, "cue defines two chapters");
        for e in &eps {
            assert!(
                e.file_path.ends_with(".flac"),
                "flac container: {}",
                e.file_path
            );
            assert!(Path::new(&e.file_path).exists());
            assert!(e.byte_length > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_without_cue_degrades_to_single_episode() {
        skip_unless_ffmpeg!();
        let dir = scratch("flac-plain");
        let Some(flac) = synth_encoded(&dir, "plain.flac", &["-c:a", "flac"], 8) else {
            skip!("no flac encoder");
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let book = scan_book(&flac, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1, "no chapters/cue -> single episode");
        assert!(eps[0].file_path.ends_with(".flac"));
        // A chapterless single file is served in place from the library (the
        // stored path is canonical/absolute); it is not copied.
        let flac_c = flac.canonicalize().unwrap();
        assert_eq!(eps[0].source_path, flac_c.to_string_lossy());
        assert_eq!(eps[0].file_path, flac_c.to_string_lossy());
        assert!(Path::new(&eps[0].source_path).is_absolute());
        assert!(!Path::new(&eps[0].file_path).starts_with(&data));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opus_single_file_served_in_place() {
        skip_unless_ffmpeg!();
        let dir = scratch("opus");
        let flac = synth_encoded(&dir, "b.opus", &["-c:a", "libopus"], 6)
            .or_else(|| synth_encoded(&dir, "b.opus", &["-c:a", "opus", "-strict", "-2"], 6));
        let Some(opus) = flac else {
            skip!("no opus encoder");
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let book = scan_book(&opus, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1);
        assert!(
            eps[0].file_path.ends_with(".opus"),
            "got {}",
            eps[0].file_path
        );
        // The server serves the original `.opus` in place from the library; it
        // is not remuxed into a data-dir container.
        assert_eq!(
            eps[0].source_path,
            opus.canonicalize().unwrap().to_string_lossy()
        );
        assert!(!Path::new(&eps[0].file_path).starts_with(&data));
        let _ = std::fs::remove_dir_all(&dir);
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
        skip_unless_ffmpeg!();
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
    fn assign_slug_disambiguates_collisions() {
        // With an empty index, assignment is pure within-scan deduplication.
        let index = Index::open_in_memory().unwrap();
        let path = Path::new("/library/whatever.m4b");
        let mut seen = HashSet::new();
        let mut next = |base: &str| assign_slug(base, path, &index, &mut seen);
        assert_eq!(next("dracula"), "dracula");
        assert_eq!(next("dracula"), "dracula-2");
        assert_eq!(next("dracula"), "dracula-3");
        assert_eq!(next("other"), "other");
    }

    // ---- recursive library discovery ----

    /// End to end on the layout Audiobookshelf produces: real files, real
    /// ffprobe, real index rows. This is the case that returned `indexed=0`
    /// before this walk existed.
    #[test]
    fn scan_library_indexes_an_author_title_tree() {
        skip_unless_ffmpeg!();
        let root = scratch("scan-nested");
        let mk = |rel: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            synth_encoded(
                path.parent().unwrap(),
                path.file_name().unwrap().to_str().unwrap(),
                &["-c:a", "aac"],
                4,
            )
        };
        if mk("Jules Verne/The Mysterious Island/Jules Verne - The Mysterious Island.m4b").is_none()
        {
            skip!("no aac encoder");
        }
        mk("Jules Verne/Journey to the Centre of the Earth/jttcote.m4b").unwrap();
        mk("Mary Shelley/Frankenstein/frankenstein.m4b").unwrap();
        // A sibling that is not audio, and a book past nothing at all.
        std::fs::write(
            root.join("Jules Verne/The Mysterious Island/metadata.json"),
            b"{}",
        )
        .unwrap();

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());

        assert_eq!(summary.indexed, 3, "every nested book is indexed");
        let mut slugs: Vec<String> = index
            .list_books()
            .unwrap()
            .into_iter()
            .map(|b| b.slug)
            .collect();
        slugs.sort();
        assert_eq!(
            slugs,
            vec![
                "jules-verne-journey-to-the-centre-of-the-earth",
                "jules-verne-the-mysterious-island",
                "mary-shelley-frankenstein"
            ]
        );
        // Titles come from the book folder, not from `Jules Verne -   - The
        // Mysterious Island`.
        let mut titles: Vec<String> = index
            .list_books()
            .unwrap()
            .into_iter()
            .map(|b| b.title)
            .collect();
        titles.sort();
        assert_eq!(
            titles,
            vec![
                "Frankenstein",
                "Journey to the Centre of the Earth",
                "The Mysterious Island"
            ]
        );
        // Each book has episodes, and its source is the file inside the tree.
        // Compare canonically: the scanner stores a canonicalized source path, and
        // `/tmp` is a symlink on macOS (`/private/var/…`) while Windows canonical
        // paths carry a `\\?\` verbatim prefix.
        let root_canonical = root.canonicalize().unwrap();
        for book in index.list_books().unwrap() {
            assert!(
                Path::new(&book.source_path).starts_with(&root_canonical),
                "{}",
                book.source_path
            );
            assert!(!index.episodes_for_book(&book.id).unwrap().is_empty());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_walks_author_title_libraries() {
        // This is the layout that Audiobookshelf, Plex, and Jellyfin all
        // produce, and the one a large library is least likely to rearrange.
        let root = scratch("discover-nested");
        touch(
            &root.join("Jules Verne/The Mysterious Island/Jules Verne - The Mysterious Island.m4b"),
        );
        touch(&root.join("Jules Verne/The Mysterious Island/metadata.json")); // ignored sibling
        touch(&root.join("Jules Verne/Journey to the Centre of the Earth/jttcote.m4b"));
        touch(&root.join("Mary Shelley/Frankenstein/01.mp3"));
        touch(&root.join("Mary Shelley/Frankenstein/02.mp3"));
        // Deeper still: shelf -> author -> series -> title.
        touch(&root.join("Shelf/Homer/The Epic Cycle/The Odyssey/odyssey.m4b"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![
                // Path-sorted: "Journey…" before "The Mysterious Island".
                BookSource::File(
                    root.join("Jules Verne/Journey to the Centre of the Earth/jttcote.m4b")
                ),
                BookSource::File(root.join(
                    "Jules Verne/The Mysterious Island/Jules Verne - The Mysterious Island.m4b"
                )),
                BookSource::Mp3Folder(root.join("Mary Shelley/Frankenstein")),
                BookSource::File(root.join("Shelf/Homer/The Epic Cycle/The Odyssey/odyssey.m4b")),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nested_book_is_named_by_its_path_but_a_shallow_one_is_not() {
        let root = scratch("discover-naming");
        touch(&root.join("Top Book.m4b"));
        touch(&root.join("a-folder-book/inner-name.m4b"));
        touch(&root.join("Jules Verne/The Mysterious Island/ugly - - stem.m4b"));
        touch(&root.join("tracks/01.mp3"));
        touch(&root.join("tracks/02.mp3"));
        touch(&root.join("Mary Shelley/Frankenstein/01.mp3"));
        touch(&root.join("Mary Shelley/Frankenstein/02.mp3"));

        let name = |src: &BookSource| slugify(&src.base_name(&root));
        let found = discover(&root, &root.join("data"));
        let names: Vec<String> = found.iter().map(name).collect();
        // Files and directories are interleaved, path-sorted, capitals first:
        // the order discovery has always produced (see
        // `root_files_and_directories_keep_their_historical_order`).
        assert_eq!(
            names,
            vec![
                // Nested: named by the path, not by an unhelpful file stem.
                "jules-verne-the-mysterious-island",
                "mary-shelley-frankenstein",
                // Unchanged: a root file keeps its stem.
                "top-book",
                // A book directly below the root keeps its FILE stem, not its
                // folder name. A change here would re-key the book and rotate
                // its capability feed id.
                "inner-name",
                // An MP3 folder directly below the root keeps its folder name.
                "tracks",
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Discovery order decides which of two same-named books gets the bare
    /// slug and which gets `-2`. That slug is the book id, and a book's
    /// capability feed id is preserved per id across re-scans. A reordering
    /// would therefore hand each subscriber the *other* audiobook under the
    /// URL they already have. Root files and directories must stay
    /// interleaved by name.
    ///
    /// A newly discovered file must not take the id of a book that already
    /// exists, because `upsert_book` preserves the capability `feed_id` per
    /// id: the URL would keep working and start serving a different
    /// audiobook. A folder of several `.m4b` files opened this case up: its
    /// second file was never discovered before, and its stem can collide with
    /// an existing book.
    ///
    /// A database that already holds two rows for one source (an older build,
    /// a manual edit) must heal to a single row. Otherwise the book stays
    /// under two capability feeds across every future reconcile, because both
    /// rows survive pruning.
    #[test]
    fn duplicate_source_rows_collapse_to_one() {
        let root = scratch("collapse-dups");
        let data = root.join("data");
        let source = root.join("book.m4b");
        touch(&source);
        let source_str = source
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let index = Index::open_in_memory().unwrap();
        let row = |id: &str, feed: &str| BookRow {
            id: id.into(),
            slug: id.into(),
            feed_id: feed.into(),
            title: "Book".into(),
            author: None,
            cover_path: None,
            source_path: source_str.clone(),
            source_mtime: 1,
            storage_mode: None,
            default_cover_url: None,
            force_embedded: false,
            transcode: Some(TranscodeMode::Off),
        };
        // The ESTABLISHED feed is the suffixed `book-2` (indexed first, so it
        // has the earlier `created_at`); the later duplicate is the base
        // `book`. Age must choose the survivor, not the id: an id sort would
        // delete the very feed subscribers use (Greptile). A few ms between
        // inserts makes `created_at` distinct on any real clock.
        index.upsert_book(&row("book-2", "cap-book-2")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        index.upsert_book(&row("book", "cap-book")).unwrap();
        touch(&data.join("books/book-2/001.m4a"));
        touch(&data.join("books/book/001.m4a"));

        collapse_duplicate_source_rows(&index, &data);

        let ids: Vec<String> = index
            .list_books()
            .unwrap()
            .into_iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(
            ids,
            vec!["book-2"],
            "the earliest-created row survives, whatever its id"
        );
        assert!(
            data.join("books/book-2/001.m4a").exists(),
            "survivor output kept"
        );
        assert!(
            !data.join("books/book").exists(),
            "later duplicate output reclaimed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two rows that share a source whose file is GONE are `prune_orphans`'
    /// job, not this one. And an unmounted library (every source missing)
    /// must collapse nothing, so that a mount blip cannot wipe the index.
    #[test]
    fn collapse_leaves_gone_sources_for_pruning() {
        let root = scratch("collapse-gone");
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let missing = root.join("not-here.m4b").to_string_lossy().into_owned();
        for id in ["book", "book-2"] {
            index
                .upsert_book(&BookRow {
                    id: id.into(),
                    slug: id.into(),
                    feed_id: format!("cap-{id}"),
                    title: "Book".into(),
                    author: None,
                    cover_path: None,
                    source_path: missing.clone(),
                    source_mtime: 1,
                    storage_mode: None,
                    default_cover_url: None,
                    force_embedded: false,
                    transcode: Some(TranscodeMode::Off),
                })
                .unwrap();
        }
        collapse_duplicate_source_rows(&index, &data);
        assert_eq!(
            index.list_books().unwrap().len(),
            2,
            "a gone source is not collapsed — prune_orphans handles it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The two-scan hazard: a stale row held the base id, so the newcomer got
    /// a suffix. Once that stale row is pruned, the next scan must not
    /// re-index the newcomer under the freed base id: that is one audiobook
    /// under two feeds. Reuse of an already-indexed source's id closes the
    /// hazard. The test exercises `reconcile` (scan-then-prune), run twice.
    #[test]
    fn a_source_keeps_one_id_across_consecutive_reconciles() {
        skip_unless_ffmpeg!();
        let root = scratch("dup-feed");
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        // A stale row already owns the base slug `foo`; its file does not exist.
        index
            .upsert_book(&BookRow {
                id: "foo".into(),
                slug: "foo".into(),
                feed_id: "cap-foo-stale".into(),
                title: "Stale".into(),
                author: None,
                cover_path: None,
                source_path: root.join("gone.m4b").to_string_lossy().into_owned(),
                source_mtime: 1,
                storage_mode: None,
                default_cover_url: None,
                force_embedded: false,
                transcode: Some(TranscodeMode::Off),
            })
            .unwrap();

        // A real newcomer whose stem slugifies to `foo`.
        if synth_encoded(&root, "Foo.m4b", &["-c:a", "aac"], 4).is_none() {
            skip!("no aac encoder");
        }
        let newcomer = root.join("Foo.m4b").canonicalize().unwrap();

        // Reconcile 1 suffixes the newcomer to `foo-2` and prunes the stale
        // `foo`.
        reconcile(&root, &data, &index, ScanOptions::default());
        // Reconcile 2: `foo` is free now, but the newcomer must keep `foo-2`.
        reconcile(&root, &data, &index, ScanOptions::default());

        let books = index.list_books().unwrap();
        let for_newcomer: Vec<&BookRow> = books
            .iter()
            .filter(|b| {
                Path::new(&b.source_path)
                    .canonicalize()
                    .map(|c| c == newcomer)
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            for_newcomer.len(),
            1,
            "the source must be indexed exactly once, not once per freed id: {:?}",
            books
                .iter()
                .map(|b| (&b.id, &b.source_path))
                .collect::<Vec<_>>()
        );
        assert_eq!(for_newcomer[0].id, "foo-2", "and it keeps its stable id");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_file_cannot_take_a_live_books_slug() {
        let root = scratch("slug-ownership");
        let established = root.join("b.m4b");
        touch(&established);
        let newcomer = root.join("Shelf/b.m4b");
        touch(&newcomer);

        let index = Index::open_in_memory().unwrap();
        index
            .upsert_book(&BookRow {
                id: "b".into(),
                slug: "b".into(),
                feed_id: "cap-b".into(),
                title: "B".into(),
                author: None,
                cover_path: None,
                source_path: established
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                source_mtime: 1,
                storage_mode: None,
                default_cover_url: None,
                force_embedded: false,
                transcode: Some(TranscodeMode::Off),
            })
            .unwrap();

        // `assign_slug` refuses the newcomer even though it runs first.
        let mut seen = HashSet::new();
        assert_eq!(assign_slug("b", &newcomer, &index, &mut seen), "b-2");
        // And the book that owns it still gets it.
        assert_eq!(assign_slug("b", &established, &index, &mut seen), "b");

        // Even when the owner's file is gone (a move), the live index row
        // still holds the id: a newcomer must NOT inherit its capability feed.
        // `prune_orphans` retires the row in the same reconcile, which frees
        // the id for a LATER scan. Within this one, the newcomer stays on
        // `b-2`.
        std::fs::remove_file(&established).unwrap();
        let mut seen = HashSet::new();
        assert_eq!(assign_slug("b", &newcomer, &index, &mut seen), "b-2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn an_mp3_track_that_links_outside_the_library_is_dropped() {
        // The folder is inside the library, but a track within it symlinks
        // out. `source_is_inside` only vetted the folder, so the collection
        // step must catch the track; otherwise it 404s at serve time.
        let outside = scratch("mp3-outside");
        std::fs::write(outside.join("external.mp3"), b"x").unwrap();

        let root = scratch("mp3-escape");
        let folder = root.join("A Book");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.mp3"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("external.mp3"), folder.join("02.mp3")).unwrap();

        let real_root = root.canonicalize().unwrap();
        let tracks = collect_mp3s(&folder.canonicalize().unwrap(), &real_root);
        let names: Vec<String> = tracks
            .iter()
            .map(|t| t.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["01.mp3"], "the escaping track must be dropped");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_link_is_skipped_rather_than_indexed() {
        // `canonicalize` fails on a dangling link. The scan must not treat
        // that as "inside the library", and it must not panic.
        let root = scratch("discover-broken-link");
        touch(&root.join("Author/Real/real.m4b"));
        let dangling_dir = root.join("Author/Dangling");
        std::fs::create_dir_all(&dangling_dir).unwrap();
        std::os::unix::fs::symlink(
            root.join("nothing-here.m4b"),
            dangling_dir.join("dangling.m4b"),
        )
        .unwrap();

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Real/real.m4b"))]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_costs_one_book_not_the_scan() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("discover-unreadable");
        touch(&root.join("Author/Readable/ok.m4b"));
        let locked = root.join("Author/Locked");
        touch(&locked.join("hidden.m4b"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let found = discover(&root, &root.join("data"));
        // Running as root defeats the permission bits; skip rather than fail.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&root);
            skip!("running as root, permissions are not enforced");
        }
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Readable/ok.m4b"))],
            "an unreadable directory must not abort the walk"
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn root_files_and_directories_keep_their_historical_order() {
        let root = scratch("discover-order");
        touch(&root.join("Dracula.m4b"));
        touch(&root.join("Dracula/inner.m4b"));
        touch(&root.join("apple.m4b"));
        touch(&root.join("Beta/b.m4b"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![
                // Path-sorted, files and directories together: "Beta" < "Dracula"
                // < "Dracula.m4b" < "apple.m4b".
                BookSource::File(root.join("Beta/b.m4b")),
                BookSource::File(root.join("Dracula/inner.m4b")),
                BookSource::File(root.join("Dracula.m4b")),
                BookSource::File(root.join("apple.m4b")),
            ],
            "directories must not be pushed behind every root file"
        );

        let index = Index::open_in_memory().unwrap();
        let mut seen = HashSet::new();
        let slugs: Vec<String> = found
            .iter()
            .map(|s| assign_slug(&slugify(&s.base_name(&root)), s.path(), &index, &mut seen))
            .collect();
        // `Dracula/inner.m4b` keeps `inner`; the folder book and the root file do not
        // trade ids.
        assert_eq!(slugs, vec!["b", "inner", "dracula", "apple"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loose_mp3s_at_the_root_stay_separate_books() {
        // The root is not a book folder: several loose `.mp3` there are several
        // single-file books, not one book whose tracks are the whole library.
        let root = scratch("discover-root-mp3");
        touch(&root.join("01 - First Book.mp3"));
        touch(&root.join("02 - Second Book.mp3"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![
                BookSource::File(root.join("01 - First Book.mp3")),
                BookSource::File(root.join("02 - Second Book.mp3")),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn links_that_leave_the_library_are_not_indexed() {
        // The serve layer canonicalizes an episode's source and refuses
        // anything outside the library root, so an index of these would
        // publish feeds whose audio 404s. The walk must refuse both a linked
        // directory and a linked file.
        let outside = scratch("discover-outside");
        touch(&outside.join("Elsewhere/external.m4b"));
        touch(&outside.join("loose-external.m4b"));

        let root = scratch("discover-escape");
        touch(&root.join("Author/Real Book/real.m4b"));
        std::os::unix::fs::symlink(outside.join("Elsewhere"), root.join("linked-dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("loose-external.m4b"),
            root.join("Author/linked-file.m4b"),
        )
        .unwrap();

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Real Book/real.m4b"))],
            "nothing outside the library root may be indexed"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_pointing_within_the_library_is_still_followed() {
        // The guard is containment, not "no symlinks": a link that stays inside
        // the library is legitimate (a shelf of favourites, say).
        let root = scratch("discover-inside-link");
        touch(&root.join("Author/Real Book/real.m4b"));
        std::os::unix::fs::symlink(root.join("Author"), root.join("Shelf")).unwrap();

        let found = discover(&root, &root.join("data"));
        // Once, not twice: the visited-set collapses the two routes to one book.
        assert_eq!(found.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nested_book_is_titled_by_its_folder_not_its_filename() {
        let root = scratch("nested-title");
        let island =
            BookSource::File(root.join(
                "Jules Verne/The Mysterious Island/Jules Verne -   - The Mysterious Island.m4b",
            ));
        assert_eq!(
            nested_title(&island, &root).as_deref(),
            Some("The Mysterious Island")
        );

        // Deeper still: the book's own folder, not the series above it.
        let deep = BookSource::File(root.join("Homer/The Epic Cycle/#1 - The Odyssey/x.m4b"));
        assert_eq!(
            nested_title(&deep, &root).as_deref(),
            Some("#1 - The Odyssey")
        );

        // Unchanged where a book may already be indexed: a root file and a book
        // directly below the root keep the file stem they have always had.
        assert_eq!(
            nested_title(&BookSource::File(root.join("Top.m4b")), &root),
            None
        );
        assert_eq!(
            nested_title(&BookSource::File(root.join("a-folder/inner.m4b")), &root),
            None
        );
        // An MP3-folder book is already named by its folder.
        assert_eq!(
            nested_title(&BookSource::Mp3Folder(root.join("Author/Tracks")), &root),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_title_under_two_authors_does_not_collide() {
        let root = scratch("discover-collision");
        touch(&root.join("Author One/Dracula/dracula.m4b"));
        touch(&root.join("Author Two/Dracula/dracula.m4b"));

        let index = Index::open_in_memory().unwrap();
        let mut seen = HashSet::new();
        let slugs: Vec<String> = discover(&root, &root.join("data"))
            .iter()
            .map(|s| assign_slug(&slugify(&s.base_name(&root)), s.path(), &index, &mut seen))
            .collect();
        // Not `dracula` and `dracula-2`; that assignment would hinge on walk
        // order.
        assert_eq!(slugs, vec!["author-one-dracula", "author-two-dracula"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_never_enters_the_data_dir() {
        // A data dir inside the library is legitimate. Its extracted episodes are
        // audio files in numbered folders, so a naive walk would index podspine's
        // own output as books and re-split it.
        let root = scratch("discover-datadir");
        // Deliberately NOT a dot-directory: those are skipped by name, and
        // that skip would let this test pass without the data-dir guard ever
        // running.
        let data = root.join("podspine-data");
        touch(&root.join("Author/Title/book.m4b"));
        touch(&data.join("books/author-title/001.m4a"));
        touch(&data.join("books/author-title/002.m4a"));

        let found = discover(&root, &data);
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Title/book.m4b"))],
            "extracted episodes must never be discovered as books"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn housekeeping_directories_are_skipped() {
        let root = scratch("discover-ignored");
        touch(&root.join("Author/Title/book.m4b"));
        touch(&root.join("@eaDir/Title/thumb.m4a")); // Synology thumbnail cache
        touch(&root.join(".stfolder/Title/sync.m4b")); // Syncthing marker
        touch(&root.join("lost+found/Title/orphan.m4b"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Title/book.m4b"))]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_stops_at_the_depth_limit() {
        let root = scratch("discover-depth");
        let mut deep = root.to_path_buf();
        for i in 0..(MAX_LIBRARY_DEPTH + 3) {
            deep = deep.join(format!("level{i}"));
        }
        touch(&deep.join("too-deep.m4b"));
        touch(&root.join("Author/Title/reachable.m4b"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Title/reachable.m4b"))],
            "a book past the depth limit is skipped, and the walk still terminates"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_terminates_and_indexes_each_book_once() {
        let root = scratch("discover-loop");
        touch(&root.join("Author/Title/book.m4b"));
        // A classic: a folder that links back to the library root.
        std::os::unix::fs::symlink(&root, root.join("Author/back-to-root")).unwrap();

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Title/book.m4b"))],
            "the loop guard must not let the same book in twice"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_lone_tier2_file_in_its_own_folder_is_a_book() {
        let root = scratch("discover-tier2");
        touch(&root.join("Author/A FLAC Book/book.flac"));
        touch(&root.join("Author/An Opus Book/book.opus"));
        // Several Tier-2 files are NOT an MP3-folder equivalent: that path
        // reads only `.mp3`. A half-ingest would be worse than a skip.
        touch(&root.join("Author/Multi FLAC/01.flac"));
        touch(&root.join("Author/Multi FLAC/02.flac"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![
                BookSource::File(root.join("Author/A FLAC Book/book.flac")),
                BookSource::File(root.join("Author/An Opus Book/book.opus")),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_drm_file_nested_in_the_tree_is_still_refused() {
        let root = scratch("discover-nested-drm");
        touch(&root.join("Author/Title/book.aax"));
        touch(&root.join("Author/Other/book.m4b"));

        let found = discover(&root, &root.join("data"));
        assert_eq!(
            found,
            vec![BookSource::File(root.join("Author/Other/book.m4b"))]
        );
        let _ = std::fs::remove_dir_all(&root);
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

        let found = discover(&root, &root.join("data"));
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
        skip_unless_ffmpeg!();
        // Two books that slugify identically, in separate folders. The folder
        // names must differ by more than case, so that they stay distinct on
        // case-insensitive filesystems (Windows/macOS). `Dracula` and
        // `Dracula!` both slugify to "dracula" but are two real directories.
        let root = scratch("dup-lib");
        let b1 = synth(
            &{
                let d = root.join("Dracula");
                std::fs::create_dir_all(&d).unwrap();
                d
            },
            false,
        );
        std::fs::rename(&b1, root.join("Dracula/Dracula.m4a")).unwrap();
        let b2 = synth(
            &{
                let d = root.join("Dracula!");
                std::fs::create_dir_all(&d).unwrap();
                d
            },
            false,
        );
        std::fs::rename(&b2, root.join("Dracula!/Dracula.m4a")).unwrap();

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());

        assert_eq!(summary.indexed, 2, "both books indexed");
        assert_eq!(summary.skipped, 0);
        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 2, "no clobber: two distinct rows");
        let slugs: HashSet<_> = books.iter().map(|b| b.slug.clone()).collect();
        assert!(
            slugs.contains("dracula") && slugs.contains("dracula-2"),
            "got {slugs:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_scan_skips_bad_books_without_aborting() {
        skip_unless_ffmpeg!();
        let root = scratch("mixed-lib");
        synth(&root, true); // chapters.m4a at the top level (the good book)
        std::fs::write(root.join("broken.m4a"), b"not really audio").unwrap();
        touch(&root.join("mp3-multi/01.mp3"));
        touch(&root.join("mp3-multi/02.mp3"));

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());

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
        skip_unless_ffmpeg!();
        let dir = scratch("chapters");
        let input = synth(&dir, true);
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();

        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 3, "3 chapters -> 3 episodes");
        // Check: idx order, positive sizes, files on disk, strictly increasing
        // pubDates.
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
    fn non_faststart_single_file_is_flagged_and_optionally_remuxed() {
        skip_unless_ffmpeg!();
        let dir = scratch("faststart");
        // ffmpeg's mp4 muxer writes `moov` at the END by default, so the file
        // is non-faststart.
        let input = synth(&dir, false);
        assert!(
            needs_faststart(&input),
            "precondition: synthesized m4a is non-faststart"
        );
        let input_c = input.canonicalize().unwrap();

        // remux OFF (default): the episode is flagged, but still streamed in
        // place.
        {
            let data = dir.join("data-off");
            let index = Index::open_in_memory().unwrap();
            let book = scan_book_as(
                &input,
                "ft",
                &data,
                &index,
                ScanOptions::default(),
                &podspine_config::BookOverrides::default(),
            )
            .unwrap();
            let eps = index.episodes_for_book(&book.id).unwrap();
            assert!(eps[0].needs_faststart, "non-faststart mp4 flagged");
            assert_eq!(
                eps[0].source_path, eps[0].file_path,
                "served in place when remux is off"
            );
            assert_eq!(
                eps[0].byte_length as u64,
                std::fs::metadata(&input).unwrap().len()
            );
        }

        // remux ON: the scan remuxes to a faststart cache file under the data
        // dir. It measures the file, then deletes it at ingest (the http layer
        // regenerates it on demand, like a saver chapter).
        {
            let data = dir.join("data-on");
            let index = Index::open_in_memory().unwrap();
            let book = scan_book_as(
                &input,
                "ft",
                &data,
                &index,
                ScanOptions {
                    remux_non_faststart: true,
                    ..Default::default()
                },
                &podspine_config::BookOverrides::default(),
            )
            .unwrap();
            let eps = index.episodes_for_book(&book.id).unwrap();
            assert!(eps[0].needs_faststart);
            assert_ne!(
                eps[0].source_path, eps[0].file_path,
                "remuxed: file_path is the cache copy, not the source"
            );
            assert_eq!(eps[0].source_path, input_c.to_string_lossy());
            assert!(
                Path::new(&eps[0].file_path).starts_with(&data),
                "cache under data dir"
            );
            assert!(eps[0].byte_length > 0);
            assert!(
                !Path::new(&eps[0].file_path).exists(),
                "measured then deleted for on-demand regen"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn faststart_single_file_is_never_remuxed() {
        skip_unless_ffmpeg!();
        let dir = scratch("faststart-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("fast.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=300:duration=3",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            skip!("no aac encoder");
        }
        assert!(!needs_faststart(&input), "precondition: file is faststart");

        // Even with remux ON, a faststart mp4 is served in place (nothing to fix).
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let book = scan_book_as(
            &input,
            "ok",
            &data,
            &index,
            ScanOptions {
                remux_non_faststart: true,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert!(!eps[0].needs_faststart);
        assert_eq!(
            eps[0].source_path, eps[0].file_path,
            "faststart mp4 is not remuxed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggling_remux_flag_reingests_the_episode() {
        skip_unless_ffmpeg!();
        let dir = scratch("faststart-toggle");
        let input = synth(&dir, false); // non-faststart m4a
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        // remux OFF: served in place.
        scan_book_as(
            &input,
            "t",
            &data,
            &index,
            ScanOptions::default(),
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let e1 = index.episodes_for_book("t").unwrap();
        assert_eq!(e1[0].source_path, e1[0].file_path, "remux off → in place");

        // Same mtime, flag flipped ON: the faststart toggle guard forces a
        // re-ingest instead of the idempotent early return.
        scan_book_as(
            &input,
            "t",
            &data,
            &index,
            ScanOptions {
                remux_non_faststart: true,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let e2 = index.episodes_for_book("t").unwrap();
        assert_ne!(
            e2[0].source_path, e2[0].file_path,
            "remux on → served from the cache copy"
        );

        // Flip back OFF: the scan re-ingests again, back to in place.
        scan_book_as(
            &input,
            "t",
            &data,
            &index,
            ScanOptions::default(),
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let e3 = index.episodes_for_book("t").unwrap();
        assert_eq!(
            e3[0].source_path, e3[0].file_path,
            "remux off again → in place"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_book_toml_overrides_apply_at_ingest() {
        skip_unless_ffmpeg!();
        let root = scratch("perbook");
        let input = synth(&root, false); // flat.m4a (top-level single-file book)
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(
            &side,
            b"title = \"My Override\"\nauthor = \"Someone\"\nstorage_mode = \"saver\"\n",
        )
        .unwrap();
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        // Server is `full`, but the sidecar forces `saver` for this one book.
        scan_library(&root, &data, &index, ScanOptions::default());
        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].title, "My Override");
        assert_eq!(books[0].author.as_deref(), Some("Someone"));
        assert_eq!(
            books[0].storage_mode,
            Some(StorageMode::Saver),
            "per-book storage_mode is persisted for serve/evict"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn editing_sidecar_reingests_without_touching_audio() {
        skip_unless_ffmpeg!();
        let root = scratch("perbook-edit");
        let input = synth(&root, false);
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        // First scan: no sidecar, so the default title and `full` apply.
        scan_library(&root, &data, &index, ScanOptions::default());
        let before = index.list_books().unwrap().remove(0);
        assert_eq!(before.storage_mode, Some(StorageMode::Full));
        let guid_before = index.episodes_for_book(&before.id).unwrap()[0].guid.clone();

        // Edit the sidecar; the AUDIO file is untouched (same mtime). The edit
        // must still take effect on the next scan (Greptile 6.4 P1).
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"title = \"Edited\"\nstorage_mode = \"saver\"\n").unwrap();
        scan_library(&root, &data, &index, ScanOptions::default());

        let after = index.get_book(&before.id).unwrap().unwrap();
        assert_eq!(after.title, "Edited", "sidecar title applied on re-scan");
        assert_eq!(
            after.storage_mode,
            Some(StorageMode::Saver),
            "sidecar storage_mode applied"
        );
        // `source_mtime` is unchanged, so the episode guid is stable: a
        // metadata edit does not make podcast clients re-download.
        let guid_after = index.episodes_for_book(&before.id).unwrap()[0].guid.clone();
        assert_eq!(
            guid_before, guid_after,
            "guid stable across a metadata-only edit"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn toggling_only_force_embedded_reingests() {
        skip_unless_ffmpeg!();
        let root = scratch("perbook-fe");
        let input = synth(&root, false);
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        let id = index.list_books().unwrap().remove(0).id;
        assert!(!index.get_book(&id).unwrap().unwrap().force_embedded);

        // Change ONLY `force_embedded_chapters` (no title/storage/cover change): it
        // alters the chapter source but not the other persisted fields, so the
        // metadata guard must still re-ingest (Greptile P1).
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"force_embedded_chapters = true").unwrap();
        scan_library(&root, &data, &index, ScanOptions::default());
        assert!(
            index.get_book(&id).unwrap().unwrap().force_embedded,
            "a force_embedded-only toggle re-ingested"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The scan survives and logs a database failure mid-reconcile; it never
    /// panics. The fault injection drops the `book` table out from under a
    /// live [`Index`] via a second connection. One reconcile then exercises
    /// every skipped-with-a-warn arm: the duplicate-row collapse, the id-reuse
    /// map, the disabled-book prune (the disabled path never probes, so no
    /// ffmpeg is needed), the orphan prune, and the metrics book count.
    #[test]
    fn scan_survives_a_broken_index() {
        let root = scratch("scan-broken-index");
        let data = root.join("data");
        std::fs::write(root.join("book.m4b"), b"").unwrap();
        std::fs::write(root.join("book.podspine.toml"), b"disabled = true").unwrap();

        let db = root.join("test.db");
        let index = Index::open(&db).unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("DROP TABLE book", [])
            .unwrap();

        let summary = reconcile(&root, &data, &index, ScanOptions::default());
        assert_eq!(
            summary.skipped, 1,
            "disabled book is still skipped when every index call fails"
        );
        assert_eq!(summary.pruned, 0, "failed orphan prune degrades to zero");
    }

    /// The duplicate-row collapse must survive a DELETE that fails while reads
    /// still work. The fault injection holds a write lock on a second
    /// connection (WAL keeps reads serving; the delete gets `SQLITE_BUSY`
    /// immediately).
    #[test]
    fn collapse_survives_a_failing_delete() {
        let root = scratch("collapse-delete-fail");
        let src = root.join("book.m4b");
        std::fs::write(&src, b"").unwrap();
        let canonical = src.canonicalize().unwrap().to_string_lossy().into_owned();

        let db = root.join("test.db");
        let index = Index::open(&db).unwrap();
        for id in ["dup", "dup-2"] {
            index
                .upsert_book(&BookRow {
                    id: id.into(),
                    slug: id.into(),
                    feed_id: podspine_index::capability::generate(),
                    title: "Dup".into(),
                    author: None,
                    cover_path: None,
                    source_path: canonical.clone(),
                    source_mtime: 0,
                    storage_mode: None,
                    default_cover_url: None,
                    force_embedded: false,
                    transcode: None,
                })
                .unwrap();
        }

        let blocker = rusqlite::Connection::open(&db).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        collapse_duplicate_source_rows(&index, &root.join("data"));
        drop(blocker);
        assert_eq!(
            index.list_books().unwrap().len(),
            2,
            "a failed delete leaves both rows (logged, not fatal)"
        );
    }

    /// The walk skips, with a warning, symlinks that leave the library root
    /// and dangling symlinks. It never follows them, and they are never fatal.
    #[cfg(unix)]
    #[test]
    fn walk_skips_escaping_and_dangling_symlinks() {
        let base = scratch("walk-symlinks");
        let library = base.join("library");
        std::fs::create_dir_all(&library).unwrap();
        let outside = base.join("outside.m4b");
        std::fs::write(&outside, b"").unwrap();
        std::os::unix::fs::symlink(&outside, library.join("escape.m4b")).unwrap();
        std::os::unix::fs::symlink(base.join("nope.m4b"), library.join("dangling.m4b")).unwrap();

        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&library, &base.join("data"), &index, ScanOptions::default());
        assert_eq!(summary.indexed, 0, "neither symlink is ingested");
        assert!(index.list_books().unwrap().is_empty());
    }

    /// A DB that cannot open must still fire the ready callback. Otherwise the
    /// server hangs on the "Scanning…" page forever (issue 159's failure
    /// mode).
    #[test]
    fn watch_loop_fires_ready_even_when_the_db_cannot_open() {
        let root = scratch("watch-bad-db");
        let db_as_dir = root.join("db-as-dir");
        std::fs::create_dir_all(&db_as_dir).unwrap(); // a directory can't open as SQLite
        let fired = std::cell::Cell::new(false);
        let res = watch_loop(
            &root,
            &root.join("data"),
            &db_as_dir,
            ScanOptions::default(),
            || fired.set(true),
        );
        assert!(res.is_err(), "the DB failure is surfaced");
        assert!(fired.get(), "readiness flips even on DB failure");
    }

    /// An unwatchable library degrades to no-auto-refresh: the initial scan
    /// still runs, ready still fires, and the loop exits cleanly.
    #[test]
    fn watch_loop_degrades_when_the_library_cannot_be_watched() {
        let root = scratch("watch-no-library");
        let missing = root.join("no-such-library");
        let fired = std::cell::Cell::new(false);
        let res = watch_loop(
            &missing,
            &root.join("data"),
            &root.join("test.db"),
            ScanOptions::default(),
            || fired.set(true),
        );
        assert!(res.is_ok(), "watch failure is degradation, not an error");
        assert!(fired.get(), "readiness flips without a watch");
    }

    #[test]
    fn per_book_disabled_skips_and_prunes() {
        skip_unless_ffmpeg!();
        let root = scratch("perbook-disabled");
        let input = synth(&root, false);
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 1, "indexed first");

        // A disabling sidecar removes the book from the index on the next
        // scan.
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"disabled = true").unwrap();
        scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(
            index.list_books().unwrap().len(),
            0,
            "disabled book is pruned"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn per_book_full_override_beats_global_saver() {
        skip_unless_ffmpeg!();
        let root = scratch("perbook-full");
        let input = synth(&root, true); // chaptered, so storage_mode matters
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"storage_mode = \"full\"").unwrap();
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        // Global saver, but the sidecar forces `full` for this book.
        scan_library(
            &root,
            &data,
            &index,
            ScanOptions {
                storage: StorageMode::Saver,
                ..Default::default()
            },
        );
        let b = index.list_books().unwrap().remove(0);
        assert_eq!(
            b.storage_mode,
            Some(StorageMode::Full),
            "sidecar full overrides global saver"
        );
        let eps = index.episodes_for_book(&b.id).unwrap();
        assert!(
            Path::new(&eps[0].file_path).exists(),
            "full mode keeps the split on disk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sidecar_global_key_is_ignored_and_a_typo_is_non_fatal() {
        skip_unless_ffmpeg!();
        // The scan ignores a server-global key in a sidecar and warns; the
        // per-book key still applies.
        let root = scratch("perbook-global");
        let input = synth(&root, false);
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"bind = \"0.0.0.0:9\"\ntitle = \"Kept\"\n").unwrap();
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        assert_eq!(
            scan_library(&root, &data, &index, ScanOptions::default()).indexed,
            1
        );
        assert_eq!(index.list_books().unwrap().remove(0).title, "Kept");
        let _ = std::fs::remove_dir_all(&root);

        // A typo (an unknown key, here a stray plural) is a per-book warning,
        // not fatal. The scan still indexes the book.
        let root2 = scratch("perbook-typo");
        let input2 = synth(&root2, false);
        std::fs::write(
            input2
                .canonicalize()
                .unwrap()
                .with_extension("podspine.toml"),
            b"storage_modes = \"saver\"",
        )
        .unwrap();
        let data2 = root2.join("data");
        let index2 = Index::open_in_memory().unwrap();
        assert_eq!(
            scan_library(&root2, &data2, &index2, ScanOptions::default()).indexed,
            1
        );
        assert_eq!(
            index2.list_books().unwrap().remove(0).storage_mode,
            Some(StorageMode::Full)
        );
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn mp3_folder_with_no_probeable_tracks_is_skipped() {
        skip_unless_ffmpeg!();
        let root = scratch("mp3-unprobeable");
        let book = root.join("Broken");
        std::fs::create_dir_all(&book).unwrap();
        // Several `.mp3` files that are not real audio give a folder book
        // whose tracks all fail to probe. That is `EmptyFolder`: skipped, not
        // fatal.
        std::fs::write(book.join("01.mp3"), b"not audio at all").unwrap();
        std::fs::write(book.join("02.mp3"), b"also not audio").unwrap();
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 0);
        assert_eq!(summary.skipped, 1, "unprobeable MP3 folder skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_watcher_indexes_a_book_added_after_startup() {
        skip_unless_ffmpeg!();
        // `keep()`: the watcher below must outlive this test body (see the
        // teardown note at the bottom), so the test defuses the drop-cleanup
        // guard.
        let root = scratch("watcher-lib").keep();
        let data = scratch("watcher-data").keep();
        let db_path = data.join("podspine.db");
        // Create the schema so the watcher and this test share the WAL db.
        drop(Index::open(&db_path).unwrap());

        spawn_library_watcher(
            root.clone(),
            data.clone(),
            db_path.clone(),
            ScanOptions::default(),
            || {},
        );
        // Let the watcher establish its filesystem watch before the test adds
        // a file.
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Add a book. The watcher should notice it, reconcile, and index it.
        let _input = synth(&root, false);

        // Poll: debounce, reconcile, and the ffmpeg split take a moment. The
        // cap is generous.
        let mut indexed = false;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(idx) = Index::open(&db_path)
                && idx.list_books().map(|b| !b.is_empty()).unwrap_or(false)
            {
                indexed = true;
                break;
            }
        }
        assert!(indexed, "the watcher indexed the book added after startup");

        // Deliberately do NOT tear down `root`/`data` here.
        // `spawn_library_watcher` is a detached, process-lifetime daemon with
        // no shutdown hook (by design). A delete of its watched dir and WAL db
        // out from under the live thread would make it churn on removed paths,
        // and it could race later tests. `scratch()` already wipes these
        // unique paths at the START of the next run, so nothing leaks across
        // runs. The parked thread sees no further events and dies with the
        // process.
    }

    #[test]
    fn watch_filter_ignores_output_and_junk_keeps_real_changes() {
        let lib = Path::new("/lib");
        let data = Path::new("/lib/.podspine"); // a nested --data-dir
        // Our own split output under the data dir is ignored (otherwise it
        // self-triggers).
        assert!(!watch_path_is_relevant(
            Path::new("/lib/.podspine/BookX/001.m4a"),
            lib,
            data,
            None
        ));
        // NAS junk and thumbnail caches are ignored anywhere below the root.
        assert!(!watch_path_is_relevant(
            Path::new("/lib/Author/@eaDir/thumb.jpg"),
            lib,
            data,
            None
        ));
        assert!(!watch_path_is_relevant(
            Path::new("/lib/.stfolder/x"),
            lib,
            data,
            None
        ));
        assert!(!watch_path_is_relevant(
            Path::new("/lib/lost+found/y"),
            lib,
            data,
            None
        ));
        // A real book file is relevant.
        assert!(watch_path_is_relevant(
            Path::new("/lib/Author/Title/book.m4b"),
            lib,
            data,
            None
        ));
        // A library that itself lives under a dot-path is NOT wholly ignored.
        let dotlib = Path::new("/home/u/.local/books");
        assert!(watch_path_is_relevant(
            Path::new("/home/u/.local/books/Author/Title/book.m4b"),
            dotlib,
            Path::new("/data"),
            None
        ));
    }

    #[test]
    fn watch_filter_drops_reads_but_keeps_writes() {
        use notify::EventKind;
        use notify::event::{AccessKind, ModifyKind};
        let lib = Path::new("/lib");
        let data = Path::new("/data");
        let paths = || vec![PathBuf::from("/lib/Author/Title/book.m4b")];
        // A read (another app streams the file) is dropped.
        let read = notify::Event {
            kind: EventKind::Access(AccessKind::Any),
            paths: paths(),
            attrs: Default::default(),
        };
        assert!(!watch_event_is_relevant(&read, lib, data, None));
        // A write to a real book file is relevant.
        let write = notify::Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: paths(),
            attrs: Default::default(),
        };
        assert!(watch_event_is_relevant(&write, lib, data, None));
        // A pathless rescan hint errs toward reconciling.
        let hint = notify::Event {
            kind: EventKind::Other,
            paths: vec![],
            attrs: Default::default(),
        };
        assert!(watch_event_is_relevant(&hint, lib, data, None));
    }

    #[test]
    fn scan_generates_a_thumbnail_and_reconcile_backfills_a_missing_one() {
        skip_unless_ffmpeg!();
        let dir = scratch("thumb-gen");
        let data = dir.join("data");
        let input = synth_with_cover(&dir);
        let index = Index::open_in_memory().unwrap();
        let scan = || {
            scan_book_as(
                &input,
                "coverbook",
                &data,
                &index,
                ScanOptions::default(),
                &BookOverrides::default(),
            )
        };

        // A scan generates the thumbnail alongside the cover.
        let book = scan().unwrap();
        let cover = PathBuf::from(book.cover_path.expect("cover extracted"));
        let thumb = cover.parent().unwrap().join("cover_thumb.jpg");
        assert!(thumb.exists(), "scan generates the thumbnail");

        // Delete it and re-scan (idempotent, unchanged): the reconcile backfills a
        // missing thumbnail without re-splitting the book.
        std::fs::remove_file(&thumb).unwrap();
        scan().unwrap();
        assert!(thumb.exists(), "reconcile backfills a missing thumbnail");

        // A re-ingest (mtime changed) deletes the old thumbnail and regenerates it,
        // so nothing stale from the previous cover survives.
        let before = std::fs::metadata(&thumb).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A clearly-different source mtime forces a re-ingest (not the early-return).
        std::fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            )
            .unwrap();
        scan().unwrap();
        let after = std::fs::metadata(&thumb).unwrap().modified().unwrap();
        assert!(after > before, "a re-ingest regenerates the thumbnail");
    }

    #[test]
    fn a_failed_cover_re_extraction_keeps_the_previous_cover() {
        skip_unless_ffmpeg!();
        let dir = scratch("cover-keep");
        let data = dir.join("data");
        let input = synth_with_cover(&dir);
        let index = Index::open_in_memory().unwrap();
        let scan = || {
            scan_book_as(
                &input,
                "coverbook",
                &data,
                &index,
                ScanOptions::default(),
                &BookOverrides::default(),
            )
        };

        // A first scan extracts the cover.
        let book = scan().unwrap();
        let cover = PathBuf::from(book.cover_path.expect("cover extracted"));
        assert!(cover.exists(), "cover on disk");
        let book_out = cover.parent().unwrap().to_path_buf();

        // Block the atomic publish of any *replacement* cover: a directory at
        // the `.part` target makes ffmpeg's write fail. `extract_cover` then
        // errors while the previously published `cover.jpg` stays intact on
        // disk. That is the exact case the fallback below must survive.
        std::fs::create_dir_all(book_out.join("cover.part.jpg")).unwrap();

        // Force a re-ingest (a distinct source mtime, not the idempotent early return).
        std::fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            )
            .unwrap();

        // The re-ingest keeps the still-present cover; it does not orphan it
        // to `None`. Otherwise both cover routes 404, and the thumbnail
        // backfill cannot recover it.
        let reingested = scan().unwrap();
        assert_eq!(
            reingested.cover_path.as_deref(),
            cover.to_str(),
            "a failed cover re-extraction keeps the previous cover, not None"
        );
        assert!(cover.exists(), "the previous cover survives on disk");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_filter_ignores_output_under_the_canonical_data_dir() {
        // The configured data dir can be spelled differently from where it
        // resolves (a symlinked mount), so the filter also checks the
        // canonical path. A write under the CANONICAL data dir is still our
        // own split output, and the filter must ignore it even when it does
        // not match the raw spelling.
        let lib = Path::new("/lib");
        let raw = Path::new("/lib/.podspine"); // as configured
        let canon = Path::new("/mnt/real/podspine"); // where it resolves to
        assert!(!watch_path_is_relevant(
            Path::new("/mnt/real/podspine/BookX/001.m4a"),
            lib,
            raw,
            Some(canon),
        ));
        // A real library file (under neither data dir) stays relevant.
        assert!(watch_path_is_relevant(
            Path::new("/lib/Author/Title/book.m4b"),
            lib,
            raw,
            Some(canon),
        ));
    }

    #[test]
    fn chapterless_file_becomes_a_single_episode() {
        skip_unless_ffmpeg!();
        let dir = scratch("flat");
        let input = synth(&dir, false);
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        let book = scan_book(&input, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1, "chapter-less -> single episode");
        assert!(Path::new(&eps[0].file_path).exists());
        // Served in place from the library — the episode IS the source file
        // (stored as a canonical/absolute path), and nothing was copied under the
        // data dir.
        let input_c = input.canonicalize().unwrap();
        assert_eq!(eps[0].source_path, input_c.to_string_lossy());
        assert_eq!(eps[0].file_path, input_c.to_string_lossy());
        assert!(Path::new(&eps[0].source_path).is_absolute());
        assert!(!Path::new(&eps[0].file_path).starts_with(&data));
        assert_eq!(
            eps[0].byte_length as u64,
            std::fs::metadata(&input).unwrap().len(),
            "enclosure length = real source size"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mp3_folder_rescan_is_idempotent_and_stays_in_place() {
        skip_unless_ffmpeg!();
        let root = scratch("mp3-idem");
        let book = root.join("Idem Book");
        let a = synth_mp3(&book, "01.mp3", Some(1), 2);
        let b = synth_mp3(&book, "02.mp3", Some(2), 2);
        if a.is_none() || b.is_none() {
            skip!("ffmpeg has no libmp3lame encoder");
        }
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        let id = index.list_books().unwrap()[0].id.clone();
        let first = index.episodes_for_book(&id).unwrap();
        assert!(first.iter().all(|e| !e.source_path.is_empty()));

        // Re-scan the unchanged folder: the `source_path` idempotency guard
        // takes the early return. Episodes are unchanged and still served in
        // place.
        scan_library(&root, &data, &index, ScanOptions::default());
        let second = index.episodes_for_book(&id).unwrap();
        assert_eq!(first.len(), second.len());
        for (x, y) in first.iter().zip(&second) {
            assert_eq!(x.guid, y.guid, "guid stable across re-scan");
            assert_eq!(x.source_path, y.source_path, "still served in place");
            assert_eq!(x.file_path, y.file_path);
        }
        // Still nothing copied under the data dir.
        assert!(!data.join("books").join(&id).join("001.mp3").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_stale_episode_copies_reclaims_numbered_files_but_keeps_cover() {
        let dir = scratch("stale-copies");
        std::fs::create_dir_all(&dir).unwrap();
        // Numbered files are per-episode copies from a pre-6.2 ingest.
        std::fs::write(dir.join("001.mp3"), b"x").unwrap();
        std::fs::write(dir.join("002.m4a"), b"y").unwrap();
        // The extracted cover and any non-numbered file must survive.
        std::fs::write(dir.join("cover.jpg"), b"img").unwrap();

        remove_stale_episode_copies(&dir);

        assert!(!dir.join("001.mp3").exists(), "stale copy removed");
        assert!(!dir.join("002.m4a").exists(), "stale copy removed");
        assert!(dir.join("cover.jpg").exists(), "cover preserved");

        // A missing dir is a no-op, not a panic.
        remove_stale_episode_copies(&dir.join("nope"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Two distinctly named top-level books in `root`. `data` is kept OUTSIDE
    // the library, so that an emptied `root` is genuinely empty (for the guard
    // test). `tag` keeps each test's scratch dirs distinct, so parallel runs
    // do not collide.
    fn two_book_library(tag: &str) -> (ScratchDir, ScratchDir, Index) {
        let root = scratch(&format!("{tag}-lib"));
        let data = scratch(&format!("{tag}-data"));
        let a = synth(&root, false);
        std::fs::rename(&a, root.join("alpha.m4a")).unwrap();
        let b = synth(&root, false);
        std::fs::rename(&b, root.join("beta.m4a")).unwrap();
        let index = Index::open_in_memory().unwrap();
        (root, data, index)
    }

    #[test]
    fn prune_orphans_removes_a_deleted_source_and_its_split_output() {
        skip_unless_ffmpeg!();
        // Chaptered books (not whole-file), so each materializes a per-chapter
        // split dir under `<data>`. That dir is the "split output" the prune
        // must remove.
        let root = scratch("prune-removes-lib");
        let data = scratch("prune-removes-data");
        let a = synth(&root, true);
        std::fs::rename(&a, root.join("alpha.m4a")).unwrap();
        let b = synth(&root, true);
        std::fs::rename(&b, root.join("beta.m4a")).unwrap();
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 2);
        let beta_out = data.join("books").join("beta");
        assert!(beta_out.exists(), "beta was split");

        // Delete beta's source; alpha remains, so the root is non-empty.
        std::fs::remove_file(root.join("beta.m4a")).unwrap();
        let pruned = prune_orphans(&root, &data, &index).unwrap();

        assert_eq!(pruned, 1);
        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].slug, "alpha");
        assert!(!beta_out.exists(), "beta's split output was removed");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn prune_orphans_empty_root_guard_preserves_the_index() {
        skip_unless_ffmpeg!();
        let (root, data, index) = two_book_library("prune-guard");
        scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 2);

        // Simulate an unmount: every source vanishes and the root goes empty.
        std::fs::remove_file(root.join("alpha.m4a")).unwrap();
        std::fs::remove_file(root.join("beta.m4a")).unwrap();
        let pruned = prune_orphans(&root, &data, &index).unwrap();

        assert_eq!(pruned, 0, "empty/unreadable root must not prune anything");
        assert_eq!(
            index.list_books().unwrap().len(),
            2,
            "books preserved despite missing sources (unmount guard)"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn reconcile_indexes_new_books_and_prunes_deleted_ones() {
        skip_unless_ffmpeg!();
        let (root, data, index) = two_book_library("reconcile");

        // First pass indexes both, prunes none.
        let s = reconcile(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 2);
        assert_eq!(s.pruned, 0);

        // Remove one source and reconcile again: the book is pruned.
        std::fs::remove_file(root.join("beta.m4a")).unwrap();
        let s = reconcile(&root, &data, &index, ScanOptions::default());
        assert_eq!(s.pruned, 1);
        let books = index.list_books().unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].slug, "alpha");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }
}
