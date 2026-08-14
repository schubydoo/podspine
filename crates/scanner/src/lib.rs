//! `scanner` — orchestrate `prober -> splitter -> index`.
//!
//! [`scan_book`] probes one audio file, resolves its chapter source (a sibling
//! `.cue`/`.ffmeta` sidecar wins over embedded markers unless `force_embedded`,
//! Task 3.8), splits those chapters into `<data>/books/<id>/`, and persists a
//! book + episodes to the index. It:
//! - falls back to a single episode for a chapter-less file,
//! - is **idempotent**: an unchanged source that is already fully indexed is not
//!   re-split (guids/pubDates are stable),
//! - **skips DRM-protected input** (AAX/AAXC/`.aa`/`.odm`) with a typed error —
//!   Podspine ships no circumvention (PRD W5).
//!
//! [`scan_library`] walks a library root of many audiobooks (Task 3.1): each
//! top-level audio file and each per-book subfolder becomes one independent
//! book. It distinguishes single-file books (`.m4b`/`.m4a`, or a lone `.mp3`),
//! split by chapters, from multi-track **MP3 folders** (Task 3.3) — a folder of
//! per-chapter MP3s ingested as one episode per file with **no splitting and no
//! re-encode** (ordered by track number, falling back to filename order). It
//! assigns collision-free slugs deterministically and never lets one bad book
//! abort the whole scan. Tier-2 inputs (Ogg Vorbis/Opus/FLAC) are stream-copied
//! into a matching container (Task 3.9); DRM inputs (AAX/AAXC/`.aa`/`.odm`) are
//! skipped with a logged notice (PRD W5).
//!
//! **Whole-file episodes are served in place (Sprint 6.2):** when an episode IS
//! a whole source file — every MP3-folder track, or a chapterless single file —
//! it is streamed directly from the read-only library and its `source_path` is
//! recorded; nothing is copied under `<data_dir>`. Only chaptered books, whose
//! episodes are sub-ranges of a container, are extracted (`full`/`saver`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// Re-exported: both are part of this crate's public surface — `BookOverrides` is a
// parameter of the scan API and `TranscodeMode` a field of [`ScanOptions`].
pub use podspine_config::{BookOverrides, TranscodeMode};
use podspine_config::{StorageMode, book_overrides};
use podspine_feed::{episode_guid, pubdate_epoch};
use podspine_index::{BookRow, EpisodeRow, Index, IndexError};
use podspine_prober::{ProbeError, needs_faststart, probe};
use podspine_splitter::{
    ChapterCut, Encoding, SplitEpisode, SplitError, extract_cover, remux_faststart,
    split_book_encoded, split_chapter_encoded, transcode_whole,
};

/// Extensions we refuse to ingest (DRM). Matched case-insensitively.
const DRM_EXTENSIONS: &[&str] = &["aax", "aaxc", "aa", "odm"];

/// The server-global ingest knobs, threaded through the scan API as one value.
///
/// Per-book `.podspine.toml` overrides refine these per book (Sprint 6.4), so a
/// field here is the *default* for a book, not the last word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Ignore any `.cue`/`.ffmeta` sidecar and use embedded chapters (Task 3.8).
    pub force_embedded: bool,
    /// `saver` storage for chaptered books: split at ingest to record each real
    /// `byte_length`, then delete and regenerate on demand (Sprint 5.1).
    pub saver: bool,
    /// Remux a non-faststart whole-file mp4 to a faststart cache copy instead of
    /// serving it in place (Sprint 6.3).
    pub remux_non_faststart: bool,
    /// Re-encode sources no podcatcher can be relied on to play (FLAC/Vorbis/
    /// Opus/ALAC) to AAC or MP3 at ingest (Task 5.2). Off by default: Podspine is
    /// copy-first, and MP3/AAC sources are never re-encoded whatever this says.
    pub transcode: TranscodeMode,
}

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
/// name. Convenience wrapper over [`scan_book_as`] for single-book callers.
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

/// Scan one audiobook `input` into `index` under the explicit `id` (also used as
/// the slug), writing split episodes under `<data_dir>/books/<id>/`. Returns the
/// persisted [`BookRow`]. The library scanner uses this to assign collision-free
/// slugs; single-book callers should use [`scan_book`].
///
/// `force_embedded` skips sidecar (`.cue`/`.ffmeta`) chapter resolution and uses
/// the embedded chapters even when a sidecar exists (Task 3.8).
///
/// `saver` is the on-demand storage mode (Sprint 5.1): each chapter is still
/// split once so its real `byte_length` (the `enclosure length`) is recorded,
/// but the file is deleted immediately afterwards — the http layer regenerates
/// it on demand. Peak extra disk is one chapter, not a full second copy of the
/// book. `false` is the default (pre-split, files kept).
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
    // Persist an ABSOLUTE, symlink-resolved source path. In-place serving (and
    // saver regeneration) resolves this later from the server's cwd, so a
    // relative `--library` stored verbatim would 404 after a restart from a
    // different directory (systemd/Docker). `is_file` above proved it exists, so
    // canonicalize succeeds; the fallback only guards a race.
    let input_canonical = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());
    let input = input_canonical.as_path();

    // Per-book `.podspine.toml` overrides refine the global flags for this book
    // (Sprint 6.4); `disabled` is handled by the caller before we're reached.
    let force_embedded = overrides
        .force_embedded_chapters
        .unwrap_or(opts.force_embedded);
    let remux_non_faststart = overrides
        .remux_non_faststart
        .unwrap_or(opts.remux_non_faststart);
    let saver = match overrides.storage_mode {
        Some(StorageMode::Saver) => true,
        Some(StorageMode::Full) => false,
        None => opts.saver,
    };
    let force_reingest = overrides.force_reingest == Some(true);

    // Effective per-book metadata (override → default). Computed up here so the
    // idempotency check can spot a `.podspine.toml` edit (which doesn't change the
    // audio mtime) and re-ingest, and so the BookRow build below reuses it.
    let eff_title = overrides.title.clone().unwrap_or_else(|| file_stem(input));
    let eff_author = overrides.author.clone();
    let eff_storage_mode = if saver { "saver" } else { "full" }.to_string();
    let eff_cover = overrides.default_cover_url.clone();

    let id = id.to_string();
    let source_mtime = mtime_epoch(input)?;
    let book_out = data_dir.join("books").join(&id);

    // Idempotency: already indexed at this mtime with all files present -> done,
    // no re-probe / re-split.
    if let Some(existing) = index.get_book(&id)?
        && existing.source_mtime == source_mtime
    {
        let eps = index.episodes_for_book(&id)?;
        // In `saver` mode the split files are intentionally absent (regenerated
        // on demand), so don't require them on disk — the index entry is enough.
        //
        // BUT guard against a migrated database: `Index::migrate` back-fills
        // `start_sec = 0` for pre-5.1 rows, and a non-first chapter with
        // `start_sec == 0` can't drive correct on-demand regeneration (it would
        // ffmpeg `-ss 0` and serve the book's opening seconds). Force a one-time
        // re-split (skip this early return) so the real offsets are recorded
        // before any eviction can serve the wrong segment. Chapter 0 legitimately
        // starts at 0, so only non-first chapters are checked.
        let start_secs_recorded =
            !saver || eps.iter().filter(|e| e.idx > 0).all(|e| e.start_sec > 0.0);
        // Faststart re-ingest guard (Sprint 6.3): if PODSPINE_REMUX_NON_FASTSTART
        // was toggled since the last scan, a `needs_faststart` whole-file episode's
        // recorded serve mode (in place ⇒ `file_path == source_path`; remuxed ⇒
        // `file_path != source_path`) no longer matches the flag — re-ingest so
        // `byte_length`/`file_path` are re-recorded for the current mode.
        let faststart_consistent = eps.iter().all(|e| {
            !e.needs_faststart
                || e.source_path.is_empty()
                || (e.file_path != e.source_path) == remux_non_faststart
        });
        // An episode's file may legitimately be absent when it's regenerated on
        // demand: a saver chapter, or a remuxed whole-file cache copy. Everything
        // else (full chapters, in-place whole files) must be present on disk.
        let files_present = eps.iter().all(|e| {
            let regenerable = (saver && e.source_path.is_empty())
                || (!e.source_path.is_empty() && e.file_path != e.source_path);
            regenerable || Path::new(&e.file_path).exists()
        });
        // A `.podspine.toml` edit doesn't change the audio mtime, so also re-ingest
        // when the persisted metadata no longer matches the current overrides
        // (Greptile 6.4 P1) — otherwise a changed title/author/storage_mode/cover
        // would stay stale in the index. `source_mtime` is unchanged, so episode
        // guids stay stable (no spurious client re-downloads).
        let metadata_consistent = existing.title == eff_title
            && existing.author == eff_author
            && existing.storage_mode == eff_storage_mode
            && existing.default_cover_url == eff_cover
            // `force_embedded_chapters` changes the chapter SOURCE (embedded vs a
            // `.cue`/`.ffmeta` sidecar) without touching the fields above, so a
            // toggle must also re-ingest (Greptile 6.4 P1).
            && existing.force_embedded == force_embedded;
        // Transcode toggle guard (Task 5.2): flipping `PODSPINE_TRANSCODE` changes
        // the episode container AND every recorded `byte_length`, and touches no
        // source mtime — so re-ingest when the persisted mode no longer matches
        // what this setting would produce. A pre-5.2 row (`""`) was produced by a
        // stream copy, which is exactly what `"off"` means, so it is not a
        // mismatch and doesn't re-split anyone's library on upgrade.
        let stored_transcode = if existing.transcode.is_empty() {
            "off"
        } else {
            existing.transcode.as_str()
        };
        let transcode_consistent = expected_transcode(input, opts.transcode)
            .is_none_or(|expected| stored_transcode == expected);
        // `force_reingest` (a troubleshooting knob) always skips the early return
        // so the book is re-processed on every scan while set.
        if !force_reingest
            && metadata_consistent
            && !eps.is_empty()
            && start_secs_recorded
            && faststart_consistent
            && transcode_consistent
            && files_present
        {
            return Ok(existing);
        }
    }

    let probed = probe(input)?;

    // Resolve the chapter source: a sibling `.cue`/`.ffmeta` sidecar wins over
    // embedded markers unless overridden (Task 3.8).
    let resolved =
        podspine_chapters::resolve(input, &probed.chapters, probed.duration_sec, force_embedded);
    if resolved.source != podspine_chapters::ChapterSource::Embedded {
        tracing::info!(id = %id, source = ?resolved.source, "using sidecar chapters");
    }

    // Transcoding (Task 5.2): a source no podcatcher can be relied on to play
    // (FLAC/Vorbis/Opus/ALAC) is re-encoded at ingest when the operator opts in.
    // MP3/AAC sources are always stream-copied, whatever the flag says.
    let enc = encoding_for(probed.audio_codec.as_deref(), opts.transcode);
    let transcoding = enc != Encoding::Copy;
    if transcoding {
        tracing::info!(
            id = %id,
            codec = probed.audio_codec.as_deref().unwrap_or("unknown"),
            target = mode_label(opts.transcode),
            "re-encoding a non-podcast-safe source (transcoding is on)"
        );
    }

    // A chapterless file is ONE whole-file episode → streamed in place from the
    // library (no split, no copy under <data_dir>). A chaptered book is extracted
    // per chapter (full/saver). See TAD §5.3. A transcoded book is never served in
    // place: the bytes clients get are the re-encoded ones, which only exist under
    // <data_dir>.
    let chapterless = resolved.chapters.is_empty();
    let serve_in_place = chapterless && !transcoding;
    // Chapters -> (cut, title). Chapter-less -> a single episode over the file.
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
    // Stream-copy into a container matching the source codec (Task 3.9), or into
    // the transcode target's container when re-encoding (Task 5.2).
    let out_ext = episode_ext(probed.audio_codec.as_deref(), enc);
    // Set for the single whole-file episode below; chaptered episodes never need
    // faststart (`split_chapter` already writes `moov`-first).
    let mut needs_ft = false;
    // A re-encode is not byte-reproducible across ffmpeg builds, so a transcoded
    // book is always materialized here and never regenerated on demand — that is
    // what keeps the published `enclosure length` equal to the bytes served. The
    // serve/evict layers read `book.transcode` and skip regeneration + eviction to
    // match, so an explicit `saver` request is deliberately overridden per book.
    if transcoding && saver {
        tracing::info!(
            id = %id,
            "transcoded book: storing chapters in full (a re-encode can't be regenerated byte-for-byte)"
        );
    }
    let episodes = if chapterless && transcoding {
        // One whole-file episode, re-encoded into <data_dir> (no -ss/-t: the
        // episode is the entire file, so a short probed duration can't clip it).
        vec![transcode_whole(
            input,
            &book_out,
            0,
            out_ext,
            probed.duration_sec,
            enc,
        )?]
    } else if serve_in_place {
        // Whole source file — nothing to extract. (Any per-episode copy a previous
        // ingest left is reclaimed after the index is updated, below.)
        // Faststart (Sprint 6.3): a non-faststart whole-file mp4 (`moov` after
        // `mdat`) seeks slowly when streamed in place. Detect it ffmpeg-free.
        needs_ft = needs_faststart(input);
        if needs_ft && remux_non_faststart {
            // Opt-in remux: write a faststart cache copy (byte-deterministic
            // `-c copy`), measure it, then delete — the http layer regenerates it
            // on demand and evicts it under the cache cap. The source is untouched.
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
            // Serve in place from the read-only library — no ffmpeg, no copy; the
            // enclosure length is the real source size. A non-faststart mp4 still
            // plays, so just log a one-line callout naming the (opt-in) fix.
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
        // Split each chapter to record its real byte size, then delete it — the
        // http layer regenerates on demand (deterministic stream-copy, so the
        // regenerated bytes match the recorded length). Peak disk = one chapter.
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
        feed_id: podspine_index::capability::generate(),
        // Per-book overrides (Sprint 6.4), computed above and re-checked by the
        // idempotency guard so a sidecar edit re-persists them.
        title: eff_title,
        author: eff_author,
        cover_path,
        source_path: input.to_string_lossy().into_owned(),
        source_mtime,
        status: "ready".to_string(),
        // Persist the effective mode so serve/evict honor it without the sidecar.
        storage_mode: eff_storage_mode,
        default_cover_url: eff_cover,
        force_embedded,
        // What actually happened to this book's audio (Task 5.2): `"off"` when it
        // was stream-copied. Read by the serve/evict layers (a transcoded book is
        // never regenerated) and by the toggle guard above.
        transcode: if transcoding {
            mode_label(opts.transcode).to_string()
        } else {
            "off".to_string()
        },
    };
    index.upsert_book(&book)?;

    for (ep, (cut, title)) in episodes.iter().zip(&specs) {
        index.upsert_episode(&EpisodeRow {
            guid: episode_guid(&id, ep.idx, source_mtime),
            book_id: id.clone(),
            idx: ep.idx as i64,
            title: title.clone(),
            file_path: ep.path.to_string_lossy().into_owned(),
            // Non-empty for a whole-file episode (source path); empty for an
            // extracted chapter under <data_dir>. `file_path == source_path` ⇒
            // in place, `file_path != source_path` ⇒ remuxed to the faststart cache.
            source_path: if serve_in_place {
                input.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            // Only ever true for the single whole-file episode; drives the http
            // remux-vs-in-place decision + the toggle guard above.
            needs_faststart: needs_ft,
            byte_length: ep.byte_length as i64,
            duration_sec: ep.duration_sec,
            start_sec: cut.start_sec,
            pubdate_epoch: pubdate_epoch(source_mtime, ep.idx, n),
        })?;
    }

    // Sweep leftovers only now, AFTER the index points at this ingest's episodes.
    // Until that upsert lands, the old files are the ones being served, and this
    // function can sit inside a re-encode for minutes: deleting them up front
    // would 404 every request for the whole window, and would leave the book with
    // no playable episodes at all if the encode then failed. Once the rows are
    // written, whatever is left in another container is unreferenced.
    if serve_in_place {
        // Episodes stream from the library now, so any per-episode copy a pre-6.2
        // ingest — or a previous transcode — left under <data_dir> is dead weight.
        remove_stale_episode_copies(&book_out);
    } else {
        remove_episode_files_in_other_containers(&book_out, out_ext);
    }

    Ok(book)
}

/// Remove episode files under `book_out` that this ingest will **not** overwrite:
/// leftovers in a container the book no longer uses.
///
/// Switching a book between stream-copy and a transcode target (Task 5.2), or
/// between the AAC and MP3 targets, changes the episode extension — `001.flac`
/// becomes `001.m4a`. The index points at the new path, so the old files are
/// unreferenced from that moment on, and nothing else reclaims them: the cache
/// eviction only touches regenerable (`saver`, stream-copied) books, and a
/// transcoded book is never regenerable. Left alone they cost a full extra copy of
/// the audiobook per mode change.
///
/// Only numbered episode files (`NNN.<ext>`) and their `NNN.part.<ext>`
/// temporaries are considered, so an extracted `cover.*` is never touched. Files
/// already in `keep_ext` are left for the ingest to overwrite. Best-effort — a
/// missing dir or a failed unlink is logged, never fatal.
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

/// Remove per-episode audio copies a previous (pre-6.2) ingest wrote under
/// `<data_dir>/books/<id>/` now that this book's episodes stream in place from
/// the library. Only numbered episode files (`NNN.<ext>`) are removed; an
/// extracted `cover.*` is left in place. Best-effort — a missing dir or a failed
/// unlink is logged, never fatal (the book still serves from the library).
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

/// Whether `path` is a produced episode file rather than something else in the
/// book directory: a numbered `NNN.<ext>`, or the `NNN.part.<ext>` temporary an
/// interrupted encode can leave (see `podspine_splitter::part_path`). An extracted
/// `cover.*` matches neither, so no sweep ever removes it.
fn is_episode_stem(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".part").unwrap_or(s))
        .is_some_and(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
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
/// file, **no splitting, no re-encode, and no copy** — each track is served in
/// place from the library (Sprint 6.2). Files are ordered by track number when
/// every track is present and distinct, otherwise by filename with a warning.
/// Idempotent on an unchanged folder.
fn scan_mp3_folder(
    dir: &Path,
    id: &str,
    data_dir: &Path,
    index: &Index,
    overrides: &BookOverrides,
) -> Result<BookRow, ScanError> {
    // Canonicalize the folder so every track path stored below is absolute and
    // symlink-resolved — in-place serving must not depend on the server's cwd.
    let dir_canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let dir = dir_canonical.as_path();
    let files = collect_mp3s(dir);
    if files.is_empty() {
        return Err(ScanError::EmptyFolder(dir.to_path_buf()));
    }

    // Book mtime = newest track mtime: stable while unchanged, bumps on replace.
    let source_mtime = files
        .iter()
        .map(|f| mtime_epoch(f))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let book_out = data_dir.join("books").join(id);

    // Effective per-book metadata (override → default). `storage_mode`/`remux`/
    // `force_embedded` are no-ops for MP3 folders, so only title/author/cover
    // apply. Computed here to detect a `.podspine.toml` edit and reused below.
    let eff_title = overrides.title.clone().unwrap_or_else(|| dir_name(dir));
    let eff_author = overrides.author.clone();
    let eff_cover = overrides.default_cover_url.clone();

    // Idempotency: unchanged and already served in place -> no re-probe.
    // The `source_path` guard forces a one-time re-ingest of a pre-6.2 book
    // (tracks copied under <data_dir>, empty `source_path`) so it flips to
    // in-place serving and its copies get reclaimed. The metadata checks re-ingest
    // on a `.podspine.toml` edit that didn't change the folder mtime (Greptile P1).
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

    // Probe each track for duration/track/title; a corrupt file is skipped, not
    // fatal to the book.
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
        // Per-book overrides (Sprint 6.4), computed above and re-checked by the
        // idempotency guard so a sidecar edit re-persists them. storage_mode/remux/
        // force_embedded are no-ops for MP3 folders (tracks are served in place),
        // so persist storage_mode as `""` (follow global).
        title: eff_title,
        author: eff_author,
        cover_path: None,
        source_path: dir.to_string_lossy().into_owned(),
        source_mtime,
        status: "ready".to_string(),
        storage_mode: String::new(),
        default_cover_url: eff_cover,
        // No chapters in an MP3 folder, so force_embedded never applies.
        force_embedded: false,
        // MP3 is podcast-safe: an MP3 folder is never re-encoded (Task 5.2).
        transcode: "off".to_string(),
    };
    index.upsert_book(&book)?;

    let n = tracks.len();
    // Each track is a whole file → served in place from the library, no copy.
    // Reclaim any verbatim copies a pre-6.2 ingest wrote under <data_dir>.
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
            // A folder track IS a whole source file — stream it in place.
            source_path: t.path.to_string_lossy().into_owned(),
            // MP3 has no `moov` atom, so faststart never applies.
            needs_faststart: false,
            byte_length: byte_length as i64,
            duration_sec: t.duration_sec,
            // Whole files (not sub-ranges of a container), so each starts at 0.
            start_sec: 0.0,
            pubdate_epoch: pubdate_epoch(source_mtime, idx, n),
        })?;
    }

    Ok(book)
}

/// Order tracks by track number when every one is present and the numbers are
/// distinct; otherwise fall back to a case-insensitive filename sort (warning).
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
fn collect_mp3s(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && ext_lower(p).as_deref() == Some("mp3"))
        .collect()
}

/// A directory's own name (fallback `"book"`), used as an MP3-folder book title.
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string())
}

/// Outcome of a library scan (counts only — a library of thousands of books is
/// never held in memory; each is indexed and dropped in turn, NFR-P4).
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

/// Audio extensions we recognize at the library level: Tier-1 (M4B/M4A/MP3) and
/// Tier-2 (Ogg Vorbis/Opus/FLAC, Task 3.9). DRM inputs (AAX/AAXC/`.aa`/`.odm`)
/// are deliberately absent and logged as skipped during discovery (PRD W5).
const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3", "ogg", "oga", "opus", "flac"];

/// Resolve + parse a book's `.podspine.toml` (Sprint 6.4). A missing sidecar
/// yields the empty default; a bad sidecar or a server-global key that doesn't
/// apply per book is logged and dropped — never fatal to the scan. `source` is
/// canonicalized so the folder-vs-library-root check compares like-for-like.
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

/// Scan a library root of many audiobooks into `index`, writing each book's
/// episodes under `<data_dir>/books/<slug>/`. One independent book per top-level
/// audio file or per-book subfolder. Slugs are collision-free and deterministic
/// across re-scans; a single failing book is logged and skipped, never fatal.
pub fn scan_library(
    library: &Path,
    data_dir: &Path,
    index: &Index,
    opts: ScanOptions,
) -> ScanSummary {
    let sources = discover(library);
    // Canonical library root, for resolving per-book `.podspine.toml` sidecars
    // (Sprint 6.4) — matched against each book's canonical source path.
    let library_root = library
        .canonicalize()
        .unwrap_or_else(|_| library.to_path_buf());

    let mut seen = HashSet::new();
    let mut summary = ScanSummary::default();
    for source in sources {
        // Reserve a slug for every candidate in deterministic order so a book's
        // slug is stable across re-scans regardless of siblings' outcomes.
        let slug = unique_slug(&slugify(&source.base_name()), &mut seen);
        let source_path = match &source {
            BookSource::File(p) => p.as_path(),
            BookSource::Mp3Folder(d) => d.as_path(),
        };
        let overrides = resolve_book_overrides(source_path, &library_root);
        // `disabled` (a `.podspine.toml` troubleshooting knob): drop the book from
        // every surface — prune it if it was previously indexed, and skip.
        if overrides.disabled == Some(true) {
            if matches!(index.get_book(&slug), Ok(Some(_))) {
                let _ = index.delete_book(&slug);
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
                match scan_mp3_folder(&dir, &slug, data_dir, index, &overrides) {
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

/// Remove indexed books whose source file/folder no longer exists, along with
/// their split output under `<data_dir>/books/<id>/`. Returns the count pruned.
///
/// **Empty-root guard:** if the library root is missing, unreadable, or empty,
/// nothing is pruned. A transiently-unmounted library looks like "every source
/// vanished"; without this guard an unmount would wipe the whole index. The cost
/// is that genuinely deleting your *last* book leaves it indexed until another
/// book is present — a safe trade.
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

/// Reconcile the index with the library: [`scan_library`] (add/update) then
/// [`prune_orphans`] (remove sources that disappeared). This is what the
/// auto-watch runs after each debounced batch of changes, and what the server
/// runs at startup so a book deleted while it was down is cleaned up.
pub fn reconcile(library: &Path, data_dir: &Path, index: &Index, opts: ScanOptions) -> ScanSummary {
    let mut summary = scan_library(library, data_dir, index, opts);
    summary.pruned = prune_orphans(library, data_dir, index).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "orphan prune failed");
        0
    });
    // Set from the index rather than accumulated from this run's adds, so a
    // prune (or a book that failed to ingest) is reflected just as accurately.
    // Reconcile is startup + debounced-watch only, so the extra read is cheap.
    match index.list_books() {
        Ok(books) => podspine_metrics::set_books_indexed(books.len() as u64),
        Err(err) => tracing::warn!(error = %err, "could not count books for metrics"),
    }
    summary
}

/// Debounce window: a burst of filesystem events (e.g. one big file copy lands
/// as many) is coalesced into a single reconcile once things go quiet.
const WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Spawn a background thread that watches `library` and [`reconcile`]s the index
/// whenever it changes (debounced). The thread opens its **own** index
/// connection on `db_path` — with WAL enabled, its rescans (including a long
/// split of a newly-added book) don't block the server's feed/audio reads.
///
/// Returns immediately; the watcher runs for the process lifetime. A setup
/// failure (or the watch ending) is logged and simply disables auto-refresh —
/// the server keeps serving what's already indexed. (Task 4.3 / PRD C2.)
pub fn spawn_library_watcher(
    library: PathBuf,
    data_dir: PathBuf,
    db_path: PathBuf,
    opts: ScanOptions,
) {
    std::thread::spawn(move || {
        if let Err(err) = watch_loop(&library, &data_dir, &db_path, opts) {
            tracing::error!(error = %err, "library watcher stopped — auto-refresh disabled");
        }
    });
}

fn watch_loop(
    library: &Path,
    data_dir: &Path,
    db_path: &Path,
    opts: ScanOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    use notify::{RecursiveMode, Watcher};

    let index = Index::open(db_path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(library, RecursiveMode::Recursive)?;
    tracing::info!(library = %library.display(), "watching library for changes");

    // Block for an event, then drain the burst until it's quiet for the debounce
    // window, then reconcile once. `watcher` stays alive in scope, so `rx` never
    // disconnects and the loop runs for the process lifetime.
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
        if path.is_file() {
            if is_drm(&path) {
                tracing::warn!(
                    path = %path.display(),
                    "skipping DRM-protected file (Podspine ships no circumvention)"
                );
            } else if is_audio(&path) {
                sources.push(BookSource::File(path));
            }
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

/// Stream-copy output container extension for an audio codec. Each Tier-2 codec
/// needs its own container (mp4 can't hold FLAC/Vorbis); unknown codecs default
/// to `m4a` (the Tier-1 case). No re-encode — this only names the muxer.
fn output_ext(codec: Option<&str>) -> &'static str {
    match codec {
        Some("mp3") => "mp3",
        Some("flac") => "flac",
        Some("vorbis") => "ogg",
        Some("opus") => "opus",
        _ => "m4a", // aac/alac and any unknown codec
    }
}

/// Whether a probed codec is one podcatchers can be relied on to play.
///
/// MP3 and AAC are the two every client handles. An unknown codec (`None` — a
/// probe that named no audio codec) counts as safe: guessing wrong there would
/// burn a whole re-encode on a file that probably plays fine.
fn is_podcast_safe(codec: Option<&str>) -> bool {
    matches!(codec, Some("mp3" | "aac") | None)
}

/// The [`Encoding`] this book's episodes are produced with (Task 5.2): a
/// re-encode only when transcoding is on *and* the source codec isn't
/// podcast-safe. Everything else is a stream copy.
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

/// The container extension for one produced episode: the transcode target's when
/// re-encoding, else one matching the source codec (Task 3.9).
fn episode_ext(codec: Option<&str>, enc: Encoding) -> &'static str {
    match enc {
        Encoding::Copy => output_ext(codec),
        Encoding::Aac => "m4a",
        Encoding::Mp3 => "mp3",
    }
}

/// The persisted label for a transcode mode (`book.transcode`).
fn mode_label(mode: TranscodeMode) -> &'static str {
    match mode {
        TranscodeMode::Off => "off",
        TranscodeMode::Aac => "aac",
        TranscodeMode::Mp3 => "mp3",
    }
}

/// The `book.transcode` value an already-indexed book *should* carry under the
/// current setting, judged from its file extension alone — the idempotency guard
/// runs before the probe and must not force one.
///
/// `None` means "can't be known without probing, so accept whatever is stored":
/// an `.m4a`/`.m4b` may hold AAC (podcast-safe, copied) or ALAC (re-encoded), and
/// guessing either way would make the guard either miss a real toggle or re-ingest
/// the book on every single scan. An ALAC book therefore keeps its existing
/// episodes until its source changes; `force_reingest = true` in its
/// `.podspine.toml` picks up the new setting immediately.
fn expected_transcode(input: &Path, mode: TranscodeMode) -> Option<&'static str> {
    if !mode.is_on() {
        // Nothing may stay transcoded once the flag is off.
        return Some("off");
    }
    match ext_lower(input).as_deref() {
        // Tier-2 containers: never podcast-safe, so they get the target codec.
        Some("flac" | "ogg" | "oga" | "opus") => Some(mode_label(mode)),
        // MP3 is podcast-safe and never re-encoded.
        Some("mp3") => Some("off"),
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

    #[test]
    fn an_mp3_folder_with_no_tracks_is_a_typed_error() {
        // A folder of cover art and metadata with no audio is a user mistake, not
        // a crash and not a silently empty book: it must surface as EmptyFolder.
        let dir = scratch("empty-mp3-folder");
        std::fs::write(dir.join("cover.jpg"), b"img").unwrap();
        std::fs::write(dir.join("notes.txt"), b"no audio here").unwrap();
        let index = Index::open_in_memory().unwrap();
        let data = dir.join("data");

        let err = scan_mp3_folder(&dir, "book-id", &data, &index, &BookOverrides::default())
            .expect_err("a folder with no mp3s must not scan");

        assert!(matches!(err, ScanError::EmptyFolder(_)), "got {err:?}");
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

    /// Synthesize a real MP3 with an optional `track` tag. Returns `None` if the
    /// ffmpeg build has no MP3 encoder (test then skips).
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("mp3-order");
        let book = root.join("A Folder Book");
        // Filenames are deliberately NOT in track order; durations tag each track.
        let a = synth_mp3(&book, "z-first.mp3", Some(1), 2);
        let b = synth_mp3(&book, "a-second.mp3", Some(2), 4);
        let c = synth_mp3(&book, "m-third.mp3", Some(3), 2);
        if a.is_none() || b.is_none() || c.is_none() {
            eprintln!("skipping: ffmpeg has no libmp3lame encoder");
            return;
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

        // Track order (1,2,3) => durations ~2,4,2. Filename order would be ~4,2,2.
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

        // No per-track copies were written under <data>/books/<id>/.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("mp3-fallback");
        let book = root.join("Mixed Book");
        // One track tagged, one missing -> mixed -> filename sort (01 before 02).
        let a = synth_mp3(&book, "01-intro.mp3", None, 2);
        let b = synth_mp3(&book, "02-body.mp3", Some(5), 4);
        if a.is_none() || b.is_none() {
            eprintln!("skipping: ffmpeg has no libmp3lame encoder");
            return;
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        // synth() embeds THREE 10s chapters. A sibling .cue defines only TWO
        // chapters (0–5s, 5–30s), which must win.
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

        // force_embedded ignores the sidecar -> back to 3 embedded chapters.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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
                saver: true,
                ..Default::default()
            },
            &podspine_config::BookOverrides::default(),
        )
        .unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 3);
        for (i, e) in eps.iter().enumerate() {
            // The real enclosure length is recorded even though the file is gone.
            assert!(e.byte_length > 0, "byte_length recorded for chapter {i}");
            // The chapter start offset is persisted so http can regenerate it.
            assert!(
                (e.start_sec - (i as f64) * 10.0).abs() < 0.5,
                "start_sec ~= {}s, got {}",
                i * 10,
                e.start_sec
            );
            // saver mode deleted the split file (peak disk = one chapter).
            assert!(
                !Path::new(&e.file_path).exists(),
                "saver deletes the split file: {}",
                e.file_path
            );
        }

        // Idempotent: re-scanning at the same mtime is a no-op despite the files
        // being absent (the index entry alone satisfies the check in saver mode).
        let again = scan_book_as(
            &input,
            "saver-book",
            &data,
            &index,
            ScanOptions {
                saver: true,
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("saver-migrated");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = synth(&dir, true); // 3 chapters at 0s / 10s / 20s
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        // A normal full-mode ingest first: files on disk, real offsets recorded.
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

        // Simulate a pre-5.1 -> 5.1 migration: back-fill start_sec = 0 everywhere.
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
                saver: true,
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

    /// Synthesize an audio file with a specific encoder. `None` if the ffmpeg
    /// build lacks that encoder (test then skips).
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
        // Off is the default: nothing is ever re-encoded.
        for codec in ["mp3", "aac", "flac", "vorbis", "opus", "alac"] {
            assert_eq!(encoding_for(Some(codec), Off), Encoding::Copy, "{codec}");
        }
        // On: MP3/AAC sources are still stream-copied — they already play everywhere.
        assert_eq!(encoding_for(Some("mp3"), Aac), Encoding::Copy);
        assert_eq!(encoding_for(Some("aac"), Aac), Encoding::Copy);
        // On: the formats podcatchers choke on are re-encoded to the chosen target.
        for codec in ["flac", "vorbis", "opus", "alac"] {
            assert_eq!(encoding_for(Some(codec), Aac), Encoding::Aac, "{codec}");
            assert_eq!(encoding_for(Some(codec), Mp3), Encoding::Mp3, "{codec}");
        }
        // An unnamed codec is left alone rather than re-encoded on a guess.
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
        assert_eq!(mode_label(TranscodeMode::Off), "off");
        assert_eq!(mode_label(TranscodeMode::Aac), "aac");
        assert_eq!(mode_label(TranscodeMode::Mp3), "mp3");
        let flac = Path::new("/lib/book.flac");
        let m4b = Path::new("/lib/book.m4b");
        let mp3 = Path::new("/lib/book.mp3");
        // Flag off: nothing may stay transcoded, whatever the container.
        assert_eq!(expected_transcode(flac, TranscodeMode::Off), Some("off"));
        assert_eq!(expected_transcode(m4b, TranscodeMode::Off), Some("off"));
        // Flag on: a Tier-2 container is definitely re-encoded...
        assert_eq!(expected_transcode(flac, TranscodeMode::Aac), Some("aac"));
        assert_eq!(
            expected_transcode(Path::new("/lib/b.opus"), TranscodeMode::Mp3),
            Some("mp3")
        );
        // ...MP3 definitely isn't...
        assert_eq!(expected_transcode(mp3, TranscodeMode::Aac), Some("off"));
        // ...and an mp4-family file is unknowable without a probe (AAC or ALAC), so
        // the guard abstains rather than re-ingesting the book on every scan.
        assert_eq!(expected_transcode(m4b, TranscodeMode::Aac), None);
    }

    /// Acceptance: a FLAC source produces a playable AAC feed when transcoding is
    /// on — right container, right codec, and an `enclosure length` that is the
    /// real output size.
    #[test]
    fn flac_with_cue_transcodes_to_aac_when_enabled() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-aac");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 20) else {
            eprintln!("skipping: no flac encoder");
            return;
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
        assert_eq!(book.transcode, "aac", "the effective mode is persisted");

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

    /// A chapterless non-podcast-safe file can't be served in place once it's
    /// re-encoded: the bytes clients get only exist under `<data_dir>`.
    #[test]
    fn chapterless_flac_transcodes_into_the_data_dir_instead_of_serving_in_place() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-whole");
        let Some(flac) = synth_encoded(&dir, "whole.flac", &["-c:a", "flac"], 8) else {
            eprintln!("skipping: no flac encoder");
            return;
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

    /// The MP3 fallback target, for clients that still choke on AAC. Skipped when
    /// this ffmpeg has no `libmp3lame`.
    #[test]
    fn flac_transcodes_to_mp3_when_that_target_is_chosen() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-mp3");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 6) else {
            eprintln!("skipping: no flac encoder");
            return;
        };
        if synth_encoded(&dir, "probe.mp3", &["-c:a", "libmp3lame"], 1).is_none() {
            eprintln!("skipping: no libmp3lame encoder");
            let _ = std::fs::remove_dir_all(&dir);
            return;
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
        assert_eq!(book.transcode, "mp3");
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

    /// `saver` is deliberately overridden for a transcoded book: a re-encode is not
    /// byte-reproducible, so its episodes must stay on disk (the serve layer refuses
    /// to regenerate or evict them).
    #[test]
    fn a_transcoded_saver_book_keeps_its_episodes_on_disk() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-saver");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            eprintln!("skipping: no flac encoder");
            return;
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
                saver: true,
                transcode: TranscodeMode::Aac,
                ..Default::default()
            },
            &BookOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            book.storage_mode, "saver",
            "the requested mode is still recorded"
        );
        assert_eq!(book.transcode, "aac");
        for e in index.episodes_for_book(&book.id).unwrap() {
            assert!(
                Path::new(&e.file_path).exists(),
                "a saver book would have deleted {}",
                e.file_path
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Changing the target rewrites every episode into a new container. The files
    /// in the old one are unreferenced from that moment, and nothing else reclaims
    /// them — the cache eviction only touches regenerable books, and a transcoded
    /// book is never regenerable — so the ingest must sweep them.
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
        // The previous container's episodes and temporaries go.
        assert!(!left("001.flac"));
        assert!(!left("002.flac"));
        assert!(!left("003.part.flac"));
        // The new container's files are left for the ingest to overwrite.
        assert!(left("001.m4a"));
        // An extracted cover is not an episode file and must survive.
        assert!(left("cover.jpg"));
        assert!(left("notes.txt"));
        assert!(left("NOTES"), "a file with no extension is not an episode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_the_transcode_target_reclaims_the_previous_container() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-reclaim");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            eprintln!("skipping: no flac encoder");
            return;
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
        // No libmp3lame is a skip, not a failure: the sweep is what's under test.
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

    /// The sweep runs after the index is updated, never before. Until then the old
    /// files are the ones being served — and this scan can sit inside a re-encode
    /// for minutes. A failed re-ingest must therefore leave the previous episodes
    /// playable rather than deleting them and then failing to replace them.
    #[test]
    fn a_failed_reingest_leaves_the_previous_episodes_in_place() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-failed-reingest");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            eprintln!("skipping: no flac encoder");
            return;
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

    /// A book indexed before Task 5.2 carries `transcode = ""`. It was necessarily
    /// stream-copied, which is what `"off"` means — so it must NOT be re-ingested
    /// on upgrade, while a genuinely stale mode must be.
    #[test]
    fn a_pre_transcode_row_is_not_reingested_but_a_stale_mode_is() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-upgrade");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 12) else {
            eprintln!("skipping: no flac encoder");
            return;
        };
        // Chaptered, so the episodes are extracted under the data dir — a
        // chapterless book is served in place and `file_path` is the library file.
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
        let stored_mode = |mode: &str| {
            let book = index.get_book("u").unwrap().unwrap();
            index
                .upsert_book(&BookRow {
                    transcode: mode.to_string(),
                    ..book
                })
                .unwrap();
        };
        // A sentinel in the episode file: an early return leaves it, a re-ingest
        // overwrites it. (Nothing else in a `full`-mode scan rewrites the file.)
        let sentinel = |ep: &str| std::fs::write(ep, b"sentinel").unwrap();
        let survived = |ep: &str| std::fs::read(ep).unwrap() == b"sentinel";

        let book = scan();
        assert_eq!(book.transcode, "off");
        let ep = index.episodes_for_book(&book.id).unwrap()[0]
            .file_path
            .clone();

        // Pre-5.2 row: `""` means the same thing as `"off"` — no re-split.
        stored_mode("");
        sentinel(&ep);
        scan();
        assert!(
            survived(&ep),
            "an upgraded database must not re-split every stream-copied book"
        );

        // A row claiming a transcode while the flag is off is genuinely stale.
        stored_mode("aac");
        sentinel(&ep);
        let back = scan();
        assert!(
            !survived(&ep),
            "a stale transcode mode must force a re-ingest"
        );
        assert_eq!(back.transcode, "off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Flipping the flag changes the container and every byte length, and touches no
    /// source mtime — so the idempotency guard must re-ingest instead of serving
    /// stale rows. And an unchanged setting must NOT re-ingest.
    #[test]
    fn toggling_transcode_reingests_a_flac_book() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-transcode-toggle");
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 8) else {
            eprintln!("skipping: no flac encoder");
            return;
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let scan = |opts: ScanOptions| {
            scan_book_as(&flac, "t", &data, &index, opts, &BookOverrides::default()).unwrap()
        };

        // Copy-first by default: a .flac episode.
        let off = scan(ScanOptions::default());
        assert_eq!(off.transcode, "off");
        let before = index.episodes_for_book(&off.id).unwrap();
        assert!(before[0].file_path.ends_with(".flac"));

        // Flag on -> re-ingest, even though the source is untouched.
        let on = scan(ScanOptions {
            transcode: TranscodeMode::Aac,
            ..Default::default()
        });
        assert_eq!(on.transcode, "aac");
        let after = index.episodes_for_book(&on.id).unwrap();
        assert!(
            after[0].file_path.ends_with(".m4a"),
            "expected a re-ingest, got {}",
            after[0].file_path
        );
        // This book is chapterless, so with the flag off it was served in place:
        // `before` pointed at the library file itself, which must never be touched.
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

        // And back off -> re-ingest to a stream copy again, in place, with the
        // transcoded copy under the data dir reclaimed rather than orphaned.
        let transcoded_copy = after[0].file_path.clone();
        let back = scan(ScanOptions::default());
        assert_eq!(back.transcode, "off");
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-cue");
        // FLAC has no titled embedded chapters, so it leans on a .cue (PRD S7).
        let Some(flac) = synth_encoded(&dir, "book.flac", &["-c:a", "flac"], 20) else {
            eprintln!("skipping: no flac encoder");
            return;
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("flac-plain");
        let Some(flac) = synth_encoded(&dir, "plain.flac", &["-c:a", "flac"], 8) else {
            eprintln!("skipping: no flac encoder");
            return;
        };
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();
        let book = scan_book(&flac, &data, &index).unwrap();
        let eps = index.episodes_for_book(&book.id).unwrap();
        assert_eq!(eps.len(), 1, "no chapters/cue -> single episode");
        assert!(eps[0].file_path.ends_with(".flac"));
        // Chapterless single file → served in place from the library (the stored
        // path is canonical/absolute), not copied.
        let flac_c = flac.canonicalize().unwrap();
        assert_eq!(eps[0].source_path, flac_c.to_string_lossy());
        assert_eq!(eps[0].file_path, flac_c.to_string_lossy());
        assert!(Path::new(&eps[0].source_path).is_absolute());
        assert!(!Path::new(&eps[0].file_path).starts_with(&data));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opus_single_file_served_in_place() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("opus");
        let flac = synth_encoded(&dir, "b.opus", &["-c:a", "libopus"], 6)
            .or_else(|| synth_encoded(&dir, "b.opus", &["-c:a", "opus", "-strict", "-2"], 6));
        let Some(opus) = flac else {
            eprintln!("skipping: no opus encoder");
            return;
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
        // Served in place from the library (the original `.opus`), not remuxed
        // into a data-dir container.
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
        // Two books that slugify identically, in separate folders. The folder
        // names must differ by more than case so they stay distinct on
        // case-insensitive filesystems (Windows/macOS) — `Dune` and `Dune!`
        // both slugify to "dune" but are two real directories everywhere.
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
                let d = root.join("Dune!");
                std::fs::create_dir_all(&d).unwrap();
                d
            },
            false,
        );
        std::fs::rename(&b2, root.join("Dune!/Dune.m4a")).unwrap();

        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();
        let summary = scan_library(&root, &data, &index, ScanOptions::default());

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
    fn non_faststart_single_file_is_flagged_and_optionally_remuxed() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("faststart");
        // ffmpeg's mp4 muxer writes `moov` at the END by default → non-faststart.
        let input = synth(&dir, false);
        assert!(
            needs_faststart(&input),
            "precondition: synthesized m4a is non-faststart"
        );
        let input_c = input.canonicalize().unwrap();

        // remux OFF (default): flagged, but still streamed in place.
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

        // remux ON: remuxed to a faststart cache file under the data dir; measured
        // then deleted at ingest (regenerated on demand, like a saver chapter).
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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
            eprintln!("skipping: no aac encoder");
            return;
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = scratch("faststart-toggle");
        let input = synth(&dir, false); // non-faststart m4a
        let data = dir.join("data");
        let index = Index::open_in_memory().unwrap();

        // remux OFF → in place.
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

        // Flip back OFF: re-ingest again, back to in place.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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
            books[0].storage_mode, "saver",
            "per-book storage_mode is persisted for serve/evict"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn editing_sidecar_reingests_without_touching_audio() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("perbook-edit");
        let input = synth(&root, false);
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        // First scan: no sidecar → default title, `full`.
        scan_library(&root, &data, &index, ScanOptions::default());
        let before = index.list_books().unwrap().remove(0);
        assert_eq!(before.storage_mode, "full");
        let guid_before = index.episodes_for_book(&before.id).unwrap()[0].guid.clone();

        // Edit the sidecar — the AUDIO file is untouched (same mtime). The edit
        // must still take effect on the next scan (Greptile 6.4 P1).
        let side = input
            .canonicalize()
            .unwrap()
            .with_extension("podspine.toml");
        std::fs::write(&side, b"title = \"Edited\"\nstorage_mode = \"saver\"\n").unwrap();
        scan_library(&root, &data, &index, ScanOptions::default());

        let after = index.get_book(&before.id).unwrap().unwrap();
        assert_eq!(after.title, "Edited", "sidecar title applied on re-scan");
        assert_eq!(after.storage_mode, "saver", "sidecar storage_mode applied");
        // source_mtime is unchanged, so the episode guid is stable — a metadata
        // edit doesn't make podcast clients re-download.
        let guid_after = index.episodes_for_book(&before.id).unwrap()[0].guid.clone();
        assert_eq!(
            guid_before, guid_after,
            "guid stable across a metadata-only edit"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn toggling_only_force_embedded_reingests() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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

    #[test]
    fn per_book_disabled_skips_and_prunes() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("perbook-disabled");
        let input = synth(&root, false);
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 1, "indexed first");

        // A disabling sidecar removes it from the index on the next scan.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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
                saver: true,
                ..Default::default()
            },
        );
        let b = index.list_books().unwrap().remove(0);
        assert_eq!(
            b.storage_mode, "full",
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        // A server-global key in a sidecar is ignored (warned); the per-book key
        // still applies.
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

        // A typo (unknown key) is a per-book warning, not fatal — still indexes.
        let root2 = scratch("perbook-typo");
        let input2 = synth(&root2, false);
        std::fs::write(
            input2
                .canonicalize()
                .unwrap()
                .with_extension("podspine.toml"),
            b"stroage_mode = \"saver\"",
        )
        .unwrap();
        let data2 = root2.join("data");
        let index2 = Index::open_in_memory().unwrap();
        assert_eq!(
            scan_library(&root2, &data2, &index2, ScanOptions::default()).indexed,
            1
        );
        assert_eq!(index2.list_books().unwrap().remove(0).storage_mode, "full");
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn mp3_folder_with_no_probeable_tracks_is_skipped() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("mp3-unprobeable");
        let book = root.join("Broken");
        std::fs::create_dir_all(&book).unwrap();
        // Several `.mp3` files that aren't real audio → a folder book whose tracks
        // all fail to probe → EmptyFolder → skipped, not fatal.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("watcher-lib");
        let data = scratch("watcher-data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let db_path = data.join("podspine.db");
        // Create the schema so the watcher and this test share the WAL db.
        drop(Index::open(&db_path).unwrap());

        spawn_library_watcher(
            root.clone(),
            data.clone(),
            db_path.clone(),
            ScanOptions::default(),
        );
        // Let the watcher establish its filesystem watch before we add a file.
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Add a book — the watcher should notice, reconcile, and index it.
        let _input = synth(&root, false);

        // Poll (debounce + reconcile + ffmpeg split take a moment); generous cap.
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

        // Deliberately do NOT tear down `root`/`data` here. `spawn_library_watcher`
        // is a detached, process-lifetime daemon with no shutdown hook (by design),
        // so deleting its watched dir + WAL db out from under the live thread would
        // make it churn on removed paths and could race later tests. `scratch()`
        // already wipes these unique paths at the START of the next run, so nothing
        // leaks across runs; the parked thread sees no further events and dies with
        // the process.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let root = scratch("mp3-idem");
        let book = root.join("Idem Book");
        let a = synth_mp3(&book, "01.mp3", Some(1), 2);
        let b = synth_mp3(&book, "02.mp3", Some(2), 2);
        if a.is_none() || b.is_none() {
            eprintln!("skipping: ffmpeg has no libmp3lame encoder");
            return;
        }
        let data = root.join("data");
        let index = Index::open_in_memory().unwrap();

        scan_library(&root, &data, &index, ScanOptions::default());
        let id = index.list_books().unwrap()[0].id.clone();
        let first = index.episodes_for_book(&id).unwrap();
        assert!(first.iter().all(|e| !e.source_path.is_empty()));

        // Re-scan the unchanged folder: the `source_path` idempotency guard takes
        // the early return — episodes are unchanged and still served in place.
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

    // Two distinctly-named top-level books in `root`; `data` is kept OUTSIDE the
    // library so emptying `root` genuinely empties it (for the guard test). `tag`
    // keeps each test's scratch dirs distinct so parallel runs don't collide.
    fn two_book_library(tag: &str) -> (PathBuf, PathBuf, Index) {
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        // Chaptered books (not whole-file) so each materializes a per-chapter
        // split dir under <data> — that's the "split output" prune must remove.
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
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
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let (root, data, index) = two_book_library("reconcile");

        // First pass indexes both, prunes none.
        let s = reconcile(&root, &data, &index, ScanOptions::default());
        assert_eq!(index.list_books().unwrap().len(), 2);
        assert_eq!(s.pruned, 0);

        // Remove one source, reconcile again -> it is pruned.
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
