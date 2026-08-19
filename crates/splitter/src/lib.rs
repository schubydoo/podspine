//! `splitter` — `ffmpeg` wrapper that cuts one audiobook file into per-chapter
//! episode files by **stream copy** (no re-encode), or — opt-in, for sources no
//! podcatcher can play — by **re-encoding** to AAC/MP3 (Task 5.2).
//!
//! Per chapter it runs, as an **argv vector** (never a shell string — chapter
//! metadata is untrusted):
//!
//! ```text
//! ffmpeg -nostdin -y -loglevel error \
//!   -ss <start> -i <in> -t <end-start> \
//!   -map 0:a:0 -map_chapters -1 -c copy -movflags +faststart <out>.m4a
//! ```
//!
//! ## Invariants (the reason this crate exists)
//! - `-ss` goes **before** `-i` (fast index seek) and duration is `-t <end-start>`.
//!   Using `-to` after `-i` together with `-ss` before `-i` does **not** subtract
//!   the offset and yields a ~2× file — so we never emit `-to`. `build_encode_args`
//!   encodes this and is unit-tested without invoking ffmpeg.
//! - `byte_length` is read from the **actual output file** (`fs::metadata().len()`),
//!   never prorated from a bitrate.
//! - The source file is only ever read; every output lands in `out_dir`.
//!
//! ## Hardening (Task 3.5)
//! Every ffmpeg spawn goes through [`run_ffmpeg`], which (a) acquires a permit
//! from a process-wide counting semaphore sized to the CPU count, so concurrent
//! ffmpeg jobs are bounded, and (b) enforces a per-child wall-clock timeout,
//! killing a hung child. The splitter is **synchronous** (`std::process`), so
//! this uses a `std`-built semaphore + the `wait-timeout` crate rather than the
//! `tokio` primitives the TAD sketched — same guarantees, no async ripple.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Per-child ffmpeg wall-clock timeout. A stream-copy of one chapter is seconds;
/// splitting a whole 10h book is ≤2min (NFR-P1), so any single child running
/// past this is hung, not slow.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-child ffmpeg wall-clock timeout for a **re-encode** ([`Encoding::Aac`] /
/// [`Encoding::Mp3`], Task 5.2). A re-encode is bounded by CPU, not by the index:
/// a whole chapterless 10h FLAC is a single child that runs for tens of minutes
/// on a Raspberry Pi, so [`FFMPEG_TIMEOUT`] would kill honest work. Still a hard
/// bound — a child past this is hung, not slow.
const TRANSCODE_TIMEOUT: Duration = Duration::from_secs(7200);

/// How an episode's audio is produced from the source.
///
/// [`Encoding::Copy`] is the default and the only mode used for podcast-safe
/// sources (MP3/AAC): the bytes are copied out of the container untouched. The
/// re-encode modes exist for Tier-2 sources (FLAC/Vorbis/Opus/ALAC) that most
/// podcatchers refuse to play, and are opt-in per server (`PODSPINE_TRANSCODE`).
///
/// A re-encode is **not** byte-reproducible across ffmpeg builds, so a transcoded
/// book is always materialized once at ingest and never regenerated on demand —
/// that is what keeps `enclosure length` equal to the file actually served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// Stream copy (`-c copy`) — no re-encode, the default everywhere.
    #[default]
    Copy,
    /// Re-encode to AAC 128 kbps (ffmpeg's built-in `aac` encoder, `.m4a`).
    Aac,
    /// Re-encode to MP3 128 kbps (`libmp3lame`, `.mp3`) — the fallback target for
    /// clients that still choke on AAC.
    Mp3,
}

impl Encoding {
    /// The per-child timeout this encoding runs under.
    fn timeout(self) -> Duration {
        match self {
            Encoding::Copy => FFMPEG_TIMEOUT,
            Encoding::Aac | Encoding::Mp3 => TRANSCODE_TIMEOUT,
        }
    }

    /// The codec arguments, appended after the stream maps.
    ///
    /// `-write_xing 1` on the MP3 target keeps a Xing/LAME header in the output so
    /// clients can read the duration of a VBR-capable file (TAD §5.4).
    fn codec_args(self) -> Vec<OsString> {
        match self {
            Encoding::Copy => vec!["-c".into(), "copy".into()],
            Encoding::Aac => vec!["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()],
            Encoding::Mp3 => vec![
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                "128k".into(),
                "-write_xing".into(),
                "1".into(),
            ],
        }
    }
}

/// A chapter to cut: its position and its `[start, end)` in seconds.
///
/// Deliberately independent of where the chapters came from (embedded ffprobe
/// markers, a `.cue` sidecar, …) so the splitter doesn't depend on the prober.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterCut {
    /// Zero-based chapter position (episode N in the feed is `idx + 1`).
    pub idx: usize,
    /// Start offset in seconds.
    pub start_sec: f64,
    /// End offset in seconds.
    pub end_sec: f64,
}

/// One produced episode file.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitEpisode {
    /// Zero-based chapter position this came from.
    pub idx: usize,
    /// Path to the written episode file (container matches `out_ext`).
    pub path: PathBuf,
    /// Real output size in bytes (`fs::metadata().len()`) — for `enclosure length`.
    pub byte_length: u64,
    /// Requested chapter duration in seconds (`end - start`).
    pub duration_sec: f64,
}

/// Failure modes of a split. None of these panic a caller.
#[derive(Debug, thiserror::Error)]
pub enum SplitError {
    /// `ffmpeg` could not be launched (not on PATH, permissions, …).
    #[error("failed to launch ffmpeg (is it installed and on PATH?): {0}")]
    Spawn(#[source] std::io::Error),
    /// A chapter had `end <= start`, so there is nothing to cut.
    #[error("chapter {idx} has a non-positive duration")]
    EmptyChapter {
        /// Zero-based chapter position.
        idx: usize,
    },
    /// `ffmpeg` ran but exited non-zero. `stderr` is captured for logs (never
    /// surface it to HTTP clients — that leak is the http layer's guard).
    #[error("ffmpeg failed on chapter {idx} (exit {code:?}): {stderr}")]
    Ffmpeg {
        /// Zero-based chapter position.
        idx: usize,
        /// Process exit code, if not killed by a signal.
        code: Option<i32>,
        /// Trimmed ffmpeg stderr.
        stderr: String,
    },
    /// `ffmpeg` exceeded the per-child timeout and was killed.
    #[error("ffmpeg timed out on chapter {idx} and was killed")]
    TimedOut {
        /// Zero-based chapter position.
        idx: usize,
    },
    /// The output file is missing or empty after a "successful" ffmpeg run.
    #[error("chapter {idx} produced no output at {path:?}")]
    OutputMissing {
        /// Zero-based chapter position.
        idx: usize,
        /// Where the output was expected.
        path: PathBuf,
    },
    /// The finished output could not be moved into place (see [`part_path`]).
    #[error("could not publish chapter {idx} to {path:?}: {source}")]
    Publish {
        /// Zero-based chapter position.
        idx: usize,
        /// The path the finished file was being moved to.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Could not create the output directory.
    #[error("could not create output directory {path:?}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Could not stat the produced output file.
    #[error("could not read output metadata for {path:?}: {source}")]
    Metadata {
        /// The output file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Failure modes of a cover extraction. A book with no cover is *not* an error —
/// the caller checks `has_cover` first; these only cover a genuine ffmpeg failure.
#[derive(Debug, thiserror::Error)]
pub enum CoverError {
    /// `ffmpeg` could not be launched.
    #[error("failed to launch ffmpeg (is it installed and on PATH?): {0}")]
    Spawn(#[source] std::io::Error),
    /// `ffmpeg` ran but exited non-zero. `stderr` is for logs only (never HTTP).
    #[error("ffmpeg cover extraction failed (exit {code:?}): {stderr}")]
    Ffmpeg {
        /// Process exit code, if not killed by a signal.
        code: Option<i32>,
        /// Trimmed ffmpeg stderr.
        stderr: String,
    },
    /// `ffmpeg` exceeded the per-child timeout and was killed.
    #[error("ffmpeg timed out extracting the cover and was killed")]
    TimedOut,
    /// The cover file is missing or empty after a "successful" ffmpeg run.
    #[error("cover extraction produced no output at {path:?}")]
    OutputMissing {
        /// Where the cover was expected.
        path: PathBuf,
    },
    /// Could not create the output directory.
    #[error("could not create output directory {path:?}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A minimal `std`-only counting semaphore used to bound how many ffmpeg
/// children run at once (the splitter is sync, so `tokio::sync::Semaphore` would
/// not fit). A dropped [`Permit`] releases and wakes one waiter.
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

struct Permit<'a>(&'a Semaphore);

impl Semaphore {
    fn acquire(&self) -> Permit<'_> {
        let mut n = self.permits.lock().unwrap();
        while *n == 0 {
            n = self.cv.wait(n).unwrap();
        }
        *n -= 1;
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.permits.lock().unwrap() += 1;
        self.0.cv.notify_one();
    }
}

/// How many ffmpeg jobs may run at once: the CPU count (fallback 4 when the OS
/// won't say). The single source of truth for both the process-wide [`ffmpeg_gate`]
/// and the per-book split worker pool ([`split_book_encoded`]), so they agree.
fn ffmpeg_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// The process-wide ffmpeg concurrency gate, sized to the CPU count (min 1).
fn ffmpeg_gate() -> &'static Semaphore {
    static GATE: OnceLock<Semaphore> = OnceLock::new();
    GATE.get_or_init(|| Semaphore {
        permits: Mutex::new(ffmpeg_parallelism()),
        cv: Condvar::new(),
    })
}

/// Outcome of a single guarded, time-bounded ffmpeg run.
enum RunError {
    /// Could not launch the process.
    Spawn(std::io::Error),
    /// Exited non-zero; stderr captured (for logs only).
    Failed {
        /// Exit code, if not signalled.
        code: Option<i32>,
        /// Trimmed stderr.
        stderr: String,
    },
    /// Exceeded the run's timeout and was killed.
    TimedOut,
}

/// Run one ffmpeg invocation under the concurrency gate with a per-child
/// timeout+kill. stdout is discarded; stderr is captured (small under
/// `-loglevel error`) so a failure can be logged without leaking it to clients.
fn run_ffmpeg(args: &[OsString]) -> Result<(), RunError> {
    run_ffmpeg_within(args, FFMPEG_TIMEOUT)
}

/// As [`run_ffmpeg`], with an explicit per-child timeout — a re-encode needs a
/// far longer bound than a stream copy ([`Encoding::timeout`]).
fn run_ffmpeg_within(args: &[OsString], timeout: Duration) -> Result<(), RunError> {
    let _permit = ffmpeg_gate().acquire();

    let mut child = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Spawn)?;

    // Drain stderr on a side thread so a chatty child can't fill the pipe buffer
    // and stall before exit (which would masquerade as a timeout).
    let stderr_drain = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = pipe.read_to_string(&mut s);
            s
        })
    });
    let stderr = || {
        stderr_drain
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    match child.wait_timeout(timeout).map_err(RunError::Spawn)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(RunError::Failed {
            code: status.code(),
            stderr: stderr(),
        }),
        None => {
            // Hung child: kill and reap so we don't leak a zombie; the drain
            // thread then hits EOF and its handle is dropped.
            let _ = child.kill();
            let _ = child.wait();
            Err(RunError::TimedOut)
        }
    }
}

/// Extract the embedded cover image of `input` into `out_dir/cover.<ext>` by
/// **stream copy** (no re-encode), returning the written path. `ext` should match
/// the cover codec (`"jpg"` for mjpeg, `"png"` for png). The source is only read.
///
/// Maps only the first video (attached-picture) stream, so no audio is written.
pub fn extract_cover(input: &Path, out_dir: &Path, ext: &str) -> Result<PathBuf, CoverError> {
    fs::create_dir_all(out_dir).map_err(|source| CoverError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;

    let out_path = out_dir.join(format!("cover.{ext}"));
    let args = build_cover_args(input, &out_path);

    match run_ffmpeg(&args) {
        Ok(()) => {}
        Err(RunError::Spawn(e)) => return Err(CoverError::Spawn(e)),
        Err(RunError::Failed { code, stderr }) => {
            return Err(CoverError::Ffmpeg { code, stderr });
        }
        Err(RunError::TimedOut) => return Err(CoverError::TimedOut),
    }

    let ok = fs::metadata(&out_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !ok {
        return Err(CoverError::OutputMissing { path: out_path });
    }
    Ok(out_path)
}

/// Long-edge cap (px) for the cover thumbnail. 400 covers every browse-UI use —
/// the grid (~150px), the detail cover (~200px) and the subscribe subcover (~96px)
/// — at 2× density, while the full-res `/cover` is what the RSS feed advertises for
/// podcatcher artwork.
const COVER_THUMB_PX: u32 = 400;

/// Downscale an already-extracted cover into a small `cover_thumb.jpg` for the
/// browse UI (the RSS feed keeps the full-res `/cover`). Re-encodes to JPEG, bounded
/// to [`COVER_THUMB_PX`] on the long edge and never upscaled; the source cover file
/// is only read. A failure is non-fatal to ingest — the serve layer falls back to
/// the full cover.
pub fn extract_cover_thumb(cover: &Path, out_dir: &Path) -> Result<PathBuf, CoverError> {
    fs::create_dir_all(out_dir).map_err(|source| CoverError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;

    let out_path = out_dir.join("cover_thumb.jpg");
    let args = build_cover_thumb_args(cover, &out_path);

    match run_ffmpeg(&args) {
        Ok(()) => {}
        Err(RunError::Spawn(e)) => return Err(CoverError::Spawn(e)),
        Err(RunError::Failed { code, stderr }) => {
            return Err(CoverError::Ffmpeg { code, stderr });
        }
        Err(RunError::TimedOut) => return Err(CoverError::TimedOut),
    }

    let ok = fs::metadata(&out_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !ok {
        return Err(CoverError::OutputMissing { path: out_path });
    }
    Ok(out_path)
}

/// Build the ffmpeg argv for a stream-copy cover extraction (argv vector, never a
/// shell string). Factored out for a hermetic unit test.
fn build_cover_args(input: &Path, output: &Path) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
        // First (attached-picture) video stream only — drops audio, one frame.
        "-map".into(),
        "0:v:0".into(),
        "-frames:v".into(),
        "1".into(),
        "-c".into(),
        "copy".into(),
        output.as_os_str().to_os_string(),
    ]
}

/// Build the ffmpeg argv for the cover thumbnail: read the cover image, one frame,
/// scale so the long edge is at most [`COVER_THUMB_PX`] (aspect preserved, never
/// upscaled — `min(px, iw/ih)`), re-encode as JPEG. Argv vector, never a shell
/// string. Factored out for a hermetic unit test.
fn build_cover_thumb_args(input: &Path, output: &Path) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!(
            "scale=w='min({px},iw)':h='min({px},ih)':force_original_aspect_ratio=decrease",
            px = COVER_THUMB_PX
        )
        .into(),
        "-c:v".into(),
        "mjpeg".into(),
        "-q:v".into(),
        "4".into(),
        output.as_os_str().to_os_string(),
    ]
}

/// Split every chapter of `input` into `out_dir`, returning one [`SplitEpisode`]
/// per chapter (fails fast on the first error). Creates `out_dir` if needed and
/// never modifies `input`. `out_ext` selects the stream-copy output container to
/// match the source codec (e.g. `"m4a"`, `"mp3"`, `"flac"`, `"ogg"`, `"opus"`).
pub fn split_book(
    input: &Path,
    out_dir: &Path,
    chapters: &[ChapterCut],
    out_ext: &str,
) -> Result<Vec<SplitEpisode>, SplitError> {
    split_book_encoded(input, out_dir, chapters, out_ext, Encoding::Copy)
}

/// As [`split_book`], with an explicit [`Encoding`] — [`Encoding::Copy`] is the
/// stream-copy default; the re-encode modes serve Task 5.2's opt-in transcoding
/// of sources podcatchers can't play.
pub fn split_book_encoded(
    input: &Path,
    out_dir: &Path,
    chapters: &[ChapterCut],
    out_ext: &str,
    enc: Encoding,
) -> Result<Vec<SplitEpisode>, SplitError> {
    fs::create_dir_all(out_dir).map_err(|source| SplitError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;

    let n = chapters.len();
    // One or zero chapters: no worker pool, no overhead.
    if n <= 1 {
        return chapters
            .iter()
            .map(|ch| split_chapter_encoded(input, out_dir, ch, out_ext, enc))
            .collect();
    }

    // Two phases so a multi-chapter split is all-or-nothing.
    //
    // Phase 1 — produce every chapter's `.part` in PARALLEL. Each is an independent
    // ffmpeg stream-copy (or re-encode) to its own `{idx+1:03}.part.<ext>`, so the
    // only shared state is the per-index result slot each worker writes exactly
    // once. Actual ffmpeg concurrency is bounded by the process-wide `ffmpeg_gate`;
    // the pool is capped at the same CPU count so extra workers wouldn't just park.
    // (This is the first-scan speed-up: splitting was serial while the gate sat
    // unused.) Nothing is published yet, so the previously-served episodes are
    // untouched no matter what happens here.
    let out_path_for = |ch: &ChapterCut| out_dir.join(format!("{:03}.{out_ext}", ch.idx + 1));
    let slots: Vec<Mutex<Option<Result<ProducedPart, SplitError>>>> =
        (0..n).map(|_| Mutex::new(None)).collect();
    let cursor = AtomicUsize::new(0);
    let workers = ffmpeg_parallelism().min(n);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let ch = &chapters[i];
                    let result = if ch.end_sec - ch.start_sec <= 0.0 {
                        Err(SplitError::EmptyChapter { idx: ch.idx })
                    } else {
                        produce_part(&out_path_for(ch), ch.idx, enc, |part| {
                            build_encode_args(input, part, Some((ch.start_sec, ch.end_sec)), enc)
                        })
                    };
                    *slots[i].lock().expect("split slot mutex poisoned") = Some(result);
                }
            });
        }
    });
    let produced: Vec<Result<ProducedPart, SplitError>> = slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .expect("split slot mutex poisoned")
                .expect("every chapter slot was filled by a worker")
        })
        .collect();

    // If ANY chapter failed, publish NONE: delete every produced part (so the old
    // episode files stay in place) and return the lowest-index error, matching the
    // old serial `?`. This is what keeps a failed re-ingest from leaving a chapter's
    // new bytes over the index's old enclosure length.
    if produced.iter().any(Result::is_err) {
        for p in produced.iter().flatten() {
            let _ = fs::remove_file(&p.part);
        }
        // Surface the lowest-index error (matches the old serial `?`).
        return Err(produced
            .into_iter()
            .filter_map(Result::err)
            .next()
            .expect("a failed result exists in this branch"));
    }

    // Phase 2 — every chapter produced: publish each `.part` into place.
    let parts: Vec<ProducedPart> = produced
        .into_iter()
        .map(|r| r.expect("all parts are Ok in this branch"))
        .collect();
    let part_paths: Vec<PathBuf> = parts.iter().map(|p| p.part.clone()).collect();

    // A same-dir rename is atomic and, in practice, only fails when a directory sits
    // at the target (the "blocked rename" case). Check EVERY target before renaming
    // any, so a late block can't leave earlier chapters published over the index's
    // now-stale enclosure lengths. If any is blocked, publish nothing and delete all
    // the produced parts — the previously-served episodes stay intact.
    for ch in chapters {
        let out_path = out_path_for(ch);
        if out_path.is_dir() {
            for leftover in &part_paths {
                let _ = fs::remove_file(leftover);
            }
            return Err(SplitError::Publish {
                idx: ch.idx,
                path: out_path,
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "a directory is at the episode path",
                ),
            });
        }
    }

    // Targets are clear: publish each part in chapter order. A rename that still
    // fails here is a catastrophic I/O error (e.g. the disk filled between producing
    // the parts and this rename); delete the parts not yet published so nothing is
    // left half-done, then error. Renames that already succeeded can't be rolled
    // back — POSIX has no cross-file rename transaction — but the next scan re-splits.
    let mut episodes = Vec::with_capacity(n);
    for (i, (ch, part)) in chapters.iter().zip(parts).enumerate() {
        match publish_part(part, out_path_for(ch), ch.idx, ch.end_sec - ch.start_sec) {
            Ok(ep) => episodes.push(ep),
            Err(err) => {
                for leftover in &part_paths[i..] {
                    let _ = fs::remove_file(leftover);
                }
                return Err(err);
            }
        }
    }
    Ok(episodes)
}

/// Cut a single chapter. Output is `out_dir/{idx+1:03}.{out_ext}`.
pub fn split_chapter(
    input: &Path,
    out_dir: &Path,
    ch: &ChapterCut,
    out_ext: &str,
) -> Result<SplitEpisode, SplitError> {
    split_chapter_encoded(input, out_dir, ch, out_ext, Encoding::Copy)
}

/// As [`split_chapter`], with an explicit [`Encoding`]. A re-encode runs under
/// the longer [`TRANSCODE_TIMEOUT`]; everything else (argv-only invocation, the
/// concurrency gate, `byte_length` read from the real output) is identical.
pub fn split_chapter_encoded(
    input: &Path,
    out_dir: &Path,
    ch: &ChapterCut,
    out_ext: &str,
    enc: Encoding,
) -> Result<SplitEpisode, SplitError> {
    let duration_sec = ch.end_sec - ch.start_sec;
    if duration_sec <= 0.0 {
        return Err(SplitError::EmptyChapter { idx: ch.idx });
    }

    let out_path = out_dir.join(format!("{:03}.{out_ext}", ch.idx + 1));
    produce_episode(out_path, ch.idx, duration_sec, enc, |part| {
        build_encode_args(input, part, Some((ch.start_sec, ch.end_sec)), enc)
    })
}

/// Produce one episode file, atomically: run ffmpeg into the [`part_path`]
/// sibling of `out_path`, validate what it wrote, then rename it into place.
///
/// `args_for` builds the argv against the temporary — callers never name the
/// final path themselves, so nothing can write directly over a published episode.
/// Every failure removes the temporary and returns without touching `out_path`,
/// so a re-ingest that dies mid-encode leaves the previously served file, and its
/// recorded `byte_length`, exactly as they were.
fn produce_episode(
    out_path: PathBuf,
    idx: usize,
    duration_sec: f64,
    enc: Encoding,
    args_for: impl FnOnce(&Path) -> Vec<OsString>,
) -> Result<SplitEpisode, SplitError> {
    let produced = produce_part(&out_path, idx, enc, args_for)?;
    publish_part(produced, out_path, idx, duration_sec)
}

/// One episode's audio, produced into its `.part` file and validated but **not yet
/// published** (renamed into place). Splitting produce from publish lets
/// [`split_book_encoded`] make a multi-chapter split all-or-nothing: it produces
/// every chapter's part first, and only publishes them if all succeeded — so a
/// failing chapter never leaves a sibling's new bytes over the old index length.
struct ProducedPart {
    /// The validated `.part` file (awaiting a rename to its final path).
    part: PathBuf,
    /// Real output size in bytes (`fs::metadata().len()`) — the enclosure length.
    byte_length: u64,
    /// ffmpeg wall-clock, recorded into the split histogram only on publish.
    elapsed: Duration,
}

/// Run ffmpeg for one episode into its `.part` file and validate the output, but do
/// **not** rename it into place. On any failure the `.part` is removed and the
/// already-published episode (if any) is left untouched.
fn produce_part(
    out_path: &Path,
    idx: usize,
    enc: Encoding,
    args_for: impl FnOnce(&Path) -> Vec<OsString>,
) -> Result<ProducedPart, SplitError> {
    let part = part_path(out_path);
    let args = args_for(&part);

    // Timed around the ffmpeg call only. The observation is recorded on publish,
    // once the output has been validated: a failed or timed-out split would
    // otherwise pollute the latency distribution with a duration that reflects
    // the failure rather than the work.
    let started = std::time::Instant::now();
    let run = run_ffmpeg_within(&args, enc.timeout());
    let elapsed = started.elapsed();
    match run {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::remove_file(&part);
            return Err(match err {
                RunError::Spawn(e) => SplitError::Spawn(e),
                RunError::Failed { code, stderr } => SplitError::Ffmpeg { idx, code, stderr },
                RunError::TimedOut => SplitError::TimedOut { idx },
            });
        }
    }

    // enclosure length MUST come from the real file, never prorated.
    let byte_length = fs::metadata(&part)
        .map_err(|source| SplitError::Metadata {
            path: part.clone(),
            source,
        })?
        .len();
    if byte_length == 0 {
        let _ = fs::remove_file(&part);
        return Err(SplitError::OutputMissing {
            idx,
            path: out_path.to_path_buf(),
        });
    }

    Ok(ProducedPart {
        part,
        byte_length,
        elapsed,
    })
}

/// Publish a produced `.part` into its final path — a same-directory atomic replace
/// (readers see the old file or the new one, never a mix) — and record the split
/// metric. An ffmpeg that "succeeds" into a missing or zero-byte file was already
/// rejected by [`produce_part`], so reaching here means real, published work: a
/// re-encode is observed here too (the same unit of work, and the slowest ingest an
/// operator can have — hiding it would make the histogram lie about the tail).
fn publish_part(
    produced: ProducedPart,
    out_path: PathBuf,
    idx: usize,
    duration_sec: f64,
) -> Result<SplitEpisode, SplitError> {
    fs::rename(&produced.part, &out_path).map_err(|source| SplitError::Publish {
        idx,
        path: out_path.clone(),
        source,
    })?;
    podspine_metrics::split_observed(produced.elapsed);
    Ok(SplitEpisode {
        idx,
        path: out_path,
        byte_length: produced.byte_length,
        duration_sec,
    })
}

/// Remux a whole (chapterless) MP4 to **faststart** — stream-copied, `moov`
/// relocated to the front — so podcast clients seek immediately. Serves a
/// non-faststart whole-file episode from the cache when `PODSPINE_REMUX_NON_FASTSTART`
/// is on (Sprint 6.3); the source is never touched. Like [`split_chapter`] it is a
/// deterministic `-c copy`, so the output size is a stable `enclosure length`.
/// `idx`/`out_ext` name the cache file (`NNN.<ext>`); `duration_sec` is the whole
/// file's duration (the enclosure duration, carried through unchanged).
pub fn remux_faststart(
    input: &Path,
    out_dir: &Path,
    idx: usize,
    out_ext: &str,
    duration_sec: f64,
) -> Result<SplitEpisode, SplitError> {
    let out_path = out_dir.join(format!("{:03}.{out_ext}", idx + 1));
    let args = build_remux_args(input, &out_path);

    match run_ffmpeg(&args) {
        Ok(()) => {}
        Err(RunError::Spawn(e)) => return Err(SplitError::Spawn(e)),
        Err(RunError::Failed { code, stderr }) => {
            return Err(SplitError::Ffmpeg { idx, code, stderr });
        }
        Err(RunError::TimedOut) => return Err(SplitError::TimedOut { idx }),
    }

    // enclosure length MUST come from the real file, never prorated.
    let byte_length = fs::metadata(&out_path)
        .map_err(|source| SplitError::Metadata {
            path: out_path.clone(),
            source,
        })?
        .len();
    if byte_length == 0 {
        return Err(SplitError::OutputMissing {
            idx,
            path: out_path,
        });
    }

    Ok(SplitEpisode {
        idx,
        path: out_path,
        byte_length,
        duration_sec,
    })
}

/// Re-encode a whole (chapterless) file into one episode under `out_dir` —
/// Task 5.2's transcode path for a source no podcatcher will play (a FLAC with no
/// `.cue`, say). Unlike [`split_chapter_encoded`] this emits **no `-ss`/`-t`**: the
/// episode is the entire file, so a probed duration that is a hair short must not
/// clip the ending. `duration_sec` is carried through to the enclosure unchanged
/// (the ffprobe duration), while `byte_length` is read from the real output.
///
/// `enc` must be a re-encode mode; [`Encoding::Copy`] here would just be a
/// container rewrite, which is [`remux_faststart`]'s job.
pub fn transcode_whole(
    input: &Path,
    out_dir: &Path,
    idx: usize,
    out_ext: &str,
    duration_sec: f64,
    enc: Encoding,
) -> Result<SplitEpisode, SplitError> {
    fs::create_dir_all(out_dir).map_err(|source| SplitError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;

    let out_path = out_dir.join(format!("{:03}.{out_ext}", idx + 1));
    produce_episode(out_path, idx, duration_sec, enc, |part| {
        build_encode_args(input, part, None, enc)
    })
}

/// argv for a whole-file faststart remux: keep audio only, drop chapters, copy
/// codecs (no re-encode), relocate `moov`. No `-ss`/`-t` — the whole file. An
/// argument vector, never a shell string (untrusted paths).
fn build_remux_args(input: &Path, output: &Path) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
        "-map".into(),
        "0:a:0".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-c".into(),
        "copy".into(),
        "-movflags".into(),
        "+faststart".into(),
        output.as_os_str().to_os_string(),
    ]
}

/// Build the exact ffmpeg argv for one produced episode.
///
/// Factored out so the ordering invariants (`-ss` before `-i`, `-t` not `-to`,
/// the codec args, `+faststart`) can be asserted in a unit test without ffmpeg.
/// `+faststart` is an mp4-family muxer option, so it is emitted only for
/// `.m4a`/`.m4b`/`.mp4` outputs (Tier-2 `.flac`/`.ogg`/`.opus` reject it).
///
/// `range` is `Some((start, end))` for a chapter cut and `None` for a whole file
/// (no `-ss`/`-t` at all — see [`transcode_whole`]). `enc` supplies the codec
/// arguments; every ordering invariant holds for a re-encode too.
fn build_encode_args(
    input: &Path,
    output: &Path,
    range: Option<(f64, f64)>,
    enc: Encoding,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    if let Some((start_sec, _)) = range {
        // -ss BEFORE -i: fast seek via the index.
        args.push("-ss".into());
        args.push(fmt_secs(start_sec).into());
    }
    args.push("-i".into());
    args.push(input.as_os_str().to_os_string());
    if let Some((start_sec, end_sec)) = range {
        // -t <duration>, NEVER -to (which with a pre-input -ss makes a 2x file).
        args.push("-t".into());
        args.push(fmt_secs((end_sec - start_sec).max(0.0)).into());
    }
    args.push("-map".into());
    args.push("0:a:0".into());
    args.push("-map_chapters".into());
    args.push("-1".into());
    args.extend(enc.codec_args());
    if is_mp4_family(output) {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push(output.as_os_str().to_os_string());
    args
}

/// The temporary path an episode is encoded into before it is renamed over
/// `out_path`.
///
/// Every episode is written **out of place and then renamed**, because a rename
/// within one directory is atomic: a request that arrives mid-encode is served the
/// previous complete file (or 404s), never a half-written one, and an ffmpeg that
/// fails or is killed leaves the already-published episode untouched. That matters
/// most for a re-encode, which holds the output open for minutes rather than
/// milliseconds — but it costs nothing to do for a stream copy too.
///
/// The extension is **preserved** (`001.m4a` → `001.part.m4a`): ffmpeg picks its
/// muxer from it, and [`is_mp4_family`] reads it to decide `+faststart`. The stem
/// is no longer a bare `NNN`, so the cache-eviction and stale-copy sweeps — which
/// both match a three-digit stem — skip a temporary.
fn part_path(out_path: &Path) -> PathBuf {
    let stem = out_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("episode");
    let name = match out_path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{stem}.part.{ext}"),
        _ => format!("{stem}.part"),
    };
    out_path.with_file_name(name)
}

/// Whether an output path uses an mp4-family container (where `+faststart`
/// applies). Tier-2 containers (flac/ogg/opus) do not.
fn is_mp4_family(output: &Path) -> bool {
    matches!(
        output
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("m4a" | "m4b" | "mp4")
    )
}

/// Format seconds for ffmpeg (fixed decimal, no scientific notation).
fn fmt_secs(v: f64) -> String {
    format!("{v:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip the rest of a test that this machine cannot run — no ffmpeg, or no
    /// encoder for the format the test needs.
    ///
    /// The `eprintln!` and the `return` live here, once, instead of at every call
    /// site. Wherever ffmpeg IS installed — CI, most dev machines — they cannot
    /// execute, and 62 copies of them dominated the uncovered-line count (35 of the
    /// 56 lines in PR 152's diff).
    ///
    /// Measured, not assumed: llvm-cov attributes a macro body to each **expansion**,
    /// not to the definition, so a call site still reports one unreachable line —
    /// one instead of two, which took 63 lines off the report (scanner 158 → 105
    /// misses, splitter 45 → 35). One line per skip is the floor for deciding at
    /// runtime; something has to stand for "this didn't run".
    macro_rules! skip {
        ($($why:tt)*) => {{
            eprintln!("skipping: {}", format_args!($($why)*));
            return;
        }};
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn args_as_strings(start: f64, end: f64) -> Vec<String> {
        strings(&build_encode_args(
            Path::new("in.m4b"),
            Path::new("out.m4a"),
            Some((start, end)),
            Encoding::Copy,
        ))
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ss_comes_before_i_and_uses_t_not_to() {
        let args = args_as_strings(10.0, 40.0);
        let pos = |s: &str| args.iter().position(|x| x == s);

        let ss = pos("-ss").expect("-ss present");
        let i = pos("-i").expect("-i present");
        assert!(ss < i, "-ss must come before -i (fast seek)");

        assert!(pos("-t").is_some(), "-t must be present");
        assert!(pos("-to").is_none(), "-to must NEVER be used (2x-file bug)");
    }

    #[test]
    fn duration_is_end_minus_start() {
        let args = args_as_strings(10.0, 40.0);
        let t = args.iter().position(|x| x == "-t").unwrap();
        assert_eq!(args[t + 1], "30.000000");
    }

    #[test]
    fn carries_copy_faststart_and_single_audio_map() {
        let args = args_as_strings(0.0, 5.0);
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("-c", "copy"), "must stream-copy");
        assert!(
            pair("-map", "0:a:0"),
            "must map only the first audio stream"
        );
        assert!(
            pair("-map_chapters", "-1"),
            "must drop chapters from output"
        );
        assert!(
            pair("-movflags", "+faststart"),
            "must move moov atom to head"
        );
        assert_eq!(args.last().unwrap(), "out.m4a", "output path is last");
    }

    #[test]
    fn aac_transcode_replaces_copy_and_keeps_cut_invariants() {
        let args = strings(&build_encode_args(
            Path::new("in.flac"),
            Path::new("out/001.m4a"),
            Some((10.0, 40.0)),
            Encoding::Aac,
        ));
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("-c:a", "aac"), "must select the aac encoder");
        assert!(pair("-b:a", "128k"), "must target 128 kbps");
        assert!(
            !args.iter().any(|a| a == "copy"),
            "a transcode must NOT stream-copy"
        );
        // The cut invariants are the same ones a stream copy obeys.
        let pos = |s: &str| args.iter().position(|x| x == s);
        assert!(pos("-ss") < pos("-i"), "-ss must still precede -i");
        assert!(pos("-to").is_none(), "-to must NEVER be used (2x-file bug)");
        let t = pos("-t").expect("-t present");
        assert_eq!(args[t + 1], "30.000000");
        assert!(pair("-map", "0:a:0"), "still one audio stream only");
        assert!(pair("-map_chapters", "-1"), "still drops chapters");
        // .m4a output ⇒ the mp4-family faststart flag still applies.
        assert!(pair("-movflags", "+faststart"));
        assert_eq!(args.last().unwrap(), "out/001.m4a");
    }

    #[test]
    fn mp3_transcode_keeps_a_xing_header() {
        let args = strings(&build_encode_args(
            Path::new("in.flac"),
            Path::new("out/001.mp3"),
            Some((0.0, 5.0)),
            Encoding::Mp3,
        ));
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("-c:a", "libmp3lame"));
        assert!(pair("-b:a", "128k"));
        // Without a Xing/LAME header a client can't read the duration (TAD §5.4).
        assert!(pair("-write_xing", "1"));
        assert!(
            !args.iter().any(|a| a == "-movflags"),
            "no mp4-only flag on an .mp3 output"
        );
    }

    #[test]
    fn whole_file_transcode_emits_no_seek_or_duration() {
        // The episode IS the whole file: a probed duration that is a hair short
        // must not clip the ending, so neither -ss nor -t may appear.
        let args = strings(&build_encode_args(
            Path::new("in.flac"),
            Path::new("out/001.m4a"),
            None,
            Encoding::Aac,
        ));
        assert!(!args.iter().any(|a| a == "-ss"), "no -ss for a whole file");
        assert!(!args.iter().any(|a| a == "-t"), "no -t for a whole file");
        assert!(!args.iter().any(|a| a == "-to"), "and never -to");
        assert!(
            args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"),
            "still re-encodes"
        );
    }

    #[test]
    fn a_reencode_gets_the_longer_timeout() {
        // A stream copy of one chapter is seconds; a whole-book re-encode is tens
        // of minutes, so it must not run under the stream-copy bound.
        assert_eq!(Encoding::Copy.timeout(), FFMPEG_TIMEOUT);
        assert_eq!(Encoding::Aac.timeout(), TRANSCODE_TIMEOUT);
        assert_eq!(Encoding::Mp3.timeout(), TRANSCODE_TIMEOUT);
        assert!(TRANSCODE_TIMEOUT > FFMPEG_TIMEOUT);
    }

    #[test]
    fn tier2_output_omits_mp4_only_faststart() {
        // A .flac output must not carry the mp4-only -movflags option.
        let args: Vec<String> = strings(&build_encode_args(
            Path::new("in.flac"),
            Path::new("out/001.flac"),
            Some((0.0, 5.0)),
            Encoding::Copy,
        ));
        assert!(args.iter().any(|a| a == "copy"), "still stream-copies");
        assert!(
            !args.iter().any(|a| a == "-movflags"),
            "no -movflags for flac"
        );
        assert_eq!(args.last().unwrap(), "out/001.flac", "output path is last");
        // Positive control: mp4 family keeps it.
        assert!(is_mp4_family(Path::new("x.m4a")));
        assert!(!is_mp4_family(Path::new("x.ogg")));
    }

    #[test]
    fn remux_args_are_whole_file_copy_faststart() {
        let args: Vec<String> = build_remux_args(Path::new("in.m4b"), Path::new("out/001.m4b"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("-c", "copy"), "stream copy, no re-encode");
        assert!(
            pair("-movflags", "+faststart"),
            "relocates moov to the front"
        );
        assert!(pair("-map", "0:a:0"), "audio only");
        assert!(pair("-map_chapters", "-1"), "drops chapters");
        assert!(
            !args.iter().any(|a| a == "-ss" || a == "-t"),
            "whole file — no seek/duration cut"
        );
        assert_eq!(args.last().unwrap(), "out/001.m4b", "output path is last");
    }

    #[test]
    fn cover_args_copy_first_video_stream_to_named_output() {
        let args: Vec<String> = build_cover_args(Path::new("in.m4b"), Path::new("out/cover.jpg"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(
            pair("-map", "0:v:0"),
            "must map the attached-picture stream"
        );
        assert!(
            pair("-c", "copy"),
            "must stream-copy the cover (no re-encode)"
        );
        assert!(pair("-frames:v", "1"), "one frame only");
        assert!(!args.iter().any(|a| a == "-to"));
        assert_eq!(args.last().unwrap(), "out/cover.jpg", "output path is last");
    }

    #[test]
    fn cover_thumb_args_scale_and_reencode_to_jpeg() {
        let args: Vec<String> =
            build_cover_thumb_args(Path::new("cover.png"), Path::new("out/cover_thumb.jpg"))
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("-c:v", "mjpeg"), "re-encode to JPEG");
        assert!(pair("-frames:v", "1"), "one frame only");
        // The scale filter caps the long edge and never upscales.
        assert!(
            args.iter()
                .any(|a| a.contains("scale=") && a.contains("min(400,iw)")),
            "must cap the long edge at 400px: {args:?}"
        );
        assert_eq!(args.last().unwrap(), "out/cover_thumb.jpg");
    }

    #[test]
    fn cover_thumb_downscales_a_large_cover() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        let dir = std::env::temp_dir().join("podspine-cover-thumb");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A large cover to downscale.
        let cover = dir.join("cover.png");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=800x800:d=0.1",
                "-frames:v",
                "1",
            ])
            .arg(&cover)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg cover synth failed");

        let thumb = extract_cover_thumb(&cover, &dir).expect("thumbnail");
        assert_eq!(thumb, dir.join("cover_thumb.jpg"));
        assert!(std::fs::metadata(&thumb).unwrap().len() > 0);

        // Downscaled: the thumbnail's width is capped at 400 (the source is 800).
        let width = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width",
                "-of",
                "csv=p=0",
            ])
            .arg(&thumb)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .expect("ffprobe the thumbnail width");
        assert!(
            width <= 400 && width > 0,
            "thumbnail width {width} must be <= 400"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semaphore_bounds_and_releases_permits() {
        let sem = Semaphore {
            permits: Mutex::new(1),
            cv: Condvar::new(),
        };
        {
            let _p = sem.acquire();
            assert_eq!(*sem.permits.lock().unwrap(), 0, "permit taken");
        }
        assert_eq!(*sem.permits.lock().unwrap(), 1, "permit released on drop");
        // The gate is sized to at least one CPU. Assert the sizing source, not the
        // global gate's live free-permit count — since split_book_encoded now runs
        // ffmpeg through that same process-wide gate in parallel, a concurrent test
        // can legitimately hold every permit, so reading the live count here races.
        assert!(ffmpeg_parallelism() >= 1);
    }

    #[test]
    fn hung_ffmpeg_is_killed_by_the_timeout() {
        fn ffmpeg_available() -> bool {
            Command::new("ffmpeg")
                .arg("-version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        if !ffmpeg_available() {
            skip!("ffmpeg not available");
        }
        // A real-time, unbounded encode that never terminates on its own; the
        // per-child timeout must kill it. Uses a deliberately tiny timeout via a
        // direct argv, bypassing the 5-min production constant.
        let args: Vec<OsString> = [
            "-nostdin",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-f",
            "null",
            "-",
        ]
        .iter()
        .map(Into::into)
        .collect();

        let _permit = ffmpeg_gate().acquire();
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        let waited = child
            .wait_timeout(Duration::from_millis(300))
            .expect("wait_timeout");
        assert!(waited.is_none(), "unbounded encode must still be running");
        child.kill().expect("kill");
        assert!(child.wait().is_ok(), "reaped after kill");
    }

    #[test]
    fn zero_length_chapter_errors_without_spawning_ffmpeg() {
        // end == start -> caught before any ffmpeg spawn, so the (missing) input
        // path is irrelevant.
        let ch = ChapterCut {
            idx: 3,
            start_sec: 5.0,
            end_sec: 5.0,
        };
        let err = split_chapter(
            Path::new("does-not-exist.m4b"),
            Path::new("/tmp"),
            &ch,
            "m4a",
        )
        .expect_err("zero-length chapter must error");
        assert!(matches!(err, SplitError::EmptyChapter { idx: 3 }));
    }

    #[test]
    fn split_maps_a_nonzero_ffmpeg_exit_to_a_split_error() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // A positive-duration cut on a non-audio input: ffmpeg fails to read it
        // and exits non-zero, exercising run_ffmpeg's failure path + the mapping.
        let dir = std::env::temp_dir().join("podspine-split-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("notaudio.m4a");
        std::fs::write(&bad, b"definitely not an audio stream").unwrap();
        let ch = ChapterCut {
            idx: 0,
            start_sec: 0.0,
            end_sec: 5.0,
        };
        let err = split_book(&bad, &dir.join("out"), std::slice::from_ref(&ch), "m4a")
            .expect_err("bad input must fail");
        assert!(matches!(err, SplitError::Ffmpeg { idx: 0, .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_split_returns_every_chapter_in_index_order() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // Many chapters (> the worker pool on most CPUs) so the parallel path runs
        // and we can prove the results come back in chapter order regardless of
        // which worker finished first.
        let dir = std::env::temp_dir().join("podspine-parallel-split");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("book.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=24",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg synth failed");

        let cuts: Vec<ChapterCut> = (0..12)
            .map(|i| ChapterCut {
                idx: i,
                start_sec: i as f64 * 2.0,
                end_sec: i as f64 * 2.0 + 2.0,
            })
            .collect();
        let out = dir.join("out");
        let eps = split_book(&input, &out, &cuts, "m4a").expect("split");

        assert_eq!(eps.len(), 12);
        // Episodes are in chapter order, and each lands at its own numbered file.
        for (i, ep) in eps.iter().enumerate() {
            assert_eq!(ep.idx, i);
            assert_eq!(ep.path, out.join(format!("{:03}.m4a", i + 1)));
            assert!(ep.path.exists() && ep.byte_length > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_chapter_publishes_none_of_a_multi_chapter_split() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // All-or-nothing: if one chapter of a re-ingest fails, the chapters that DID
        // succeed must not be published over the already-served files — otherwise the
        // index's old enclosure length would point at new bytes.
        let dir = std::env::temp_dir().join("podspine-atomic-book");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = dir.join("book.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=10",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg synth failed");

        // Stand-ins for previously-published episodes.
        let published: Vec<PathBuf> = (1..=3).map(|n| out.join(format!("{n:03}.m4a"))).collect();
        for (n, p) in published.iter().enumerate() {
            std::fs::write(p, format!("old episode {n}")).unwrap();
        }
        let before: Vec<Vec<u8>> = published
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();

        // Chapters 0 and 1 are valid; chapter 2 is empty (end == start) → fails
        // before ffmpeg, so 0/1 produce real parts but nothing is published.
        let cuts = [
            ChapterCut {
                idx: 0,
                start_sec: 0.0,
                end_sec: 3.0,
            },
            ChapterCut {
                idx: 1,
                start_sec: 3.0,
                end_sec: 6.0,
            },
            ChapterCut {
                idx: 2,
                start_sec: 6.0,
                end_sec: 6.0,
            },
        ];
        let err = split_book(&input, &out, &cuts, "m4a")
            .expect_err("the empty chapter must fail the split");
        assert!(
            matches!(err, SplitError::EmptyChapter { idx: 2 }),
            "{err:?}"
        );

        // Every previously-published episode is byte-for-byte untouched...
        for (p, was) in published.iter().zip(&before) {
            assert_eq!(
                &std::fs::read(p).unwrap(),
                was,
                "{p:?} must not be overwritten"
            );
        }
        // ...and no half-produced `.part` files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "produced parts must be cleaned up: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_publish_target_publishes_none_of_a_multi_chapter_split() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // Both chapters produce fine, but a directory sits where chapter 2's episode
        // must land, so its publish (rename) can't happen. The pre-check must catch
        // that before publishing chapter 1 — nothing half-published, no parts left.
        let dir = std::env::temp_dir().join("podspine-atomic-block");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = dir.join("book.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=10",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg synth failed");

        // A directory blocks chapter 2's target; chapter 1 has an old published file.
        std::fs::create_dir_all(out.join("002.m4a")).unwrap();
        let published = out.join("001.m4a");
        std::fs::write(&published, b"old episode 1").unwrap();
        let before = std::fs::read(&published).unwrap();

        let cuts = [
            ChapterCut {
                idx: 0,
                start_sec: 0.0,
                end_sec: 3.0,
            },
            ChapterCut {
                idx: 1,
                start_sec: 3.0,
                end_sec: 6.0,
            },
        ];
        let err = split_book(&input, &out, &cuts, "m4a").expect_err("blocked target must fail");
        assert!(matches!(err, SplitError::Publish { idx: 1, .. }), "{err:?}");

        // Chapter 1's old file is untouched (nothing was published)...
        assert_eq!(std::fs::read(&published).unwrap(), before);
        // ...the blocker is left exactly as it was...
        assert!(out.join("002.m4a").is_dir());
        // ...and no `.part` files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "parts must be cleaned up: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_temporary_keeps_the_extension_and_hides_from_the_sweeps() {
        let p = part_path(Path::new("/data/books/b/001.m4a"));
        // ffmpeg picks its muxer from the extension, and `is_mp4_family` reads it
        // to decide `+faststart` — so the extension has to survive.
        assert_eq!(p, Path::new("/data/books/b/001.part.m4a"));
        assert!(is_mp4_family(&p), "a temporary must still look mp4-family");
        // Both the cache eviction and the stale-copy sweep match a 3-digit stem.
        let stem = p.file_stem().unwrap().to_str().unwrap();
        assert_eq!(stem, "001.part");
        assert!(
            !(stem.len() == 3 && stem.bytes().all(|b| b.is_ascii_digit())),
            "a temporary must not look like an episode file"
        );
        assert_eq!(
            part_path(Path::new("/data/001")),
            Path::new("/data/001.part"),
            "an extensionless output still gets a distinct temporary"
        );
    }

    #[test]
    fn an_unusable_output_directory_is_an_error_not_a_panic() {
        // No ffmpeg needed: `create_dir_all` fails before anything is spawned
        // (the "directory" is a regular file's child).
        let dir = std::env::temp_dir().join("podspine-transcode-baddir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();

        let err = transcode_whole(
            Path::new("in.flac"),
            &blocker.join("books"),
            0,
            "m4a",
            10.0,
            Encoding::Aac,
        )
        .expect_err("an unusable out_dir must fail");
        assert!(matches!(err, SplitError::CreateDir { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_rename_is_reported_and_never_leaves_a_half_published_file() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // A directory sitting where the episode must land blocks the atomic
        // rename. The encode itself succeeds, so this exercises the publish step.
        let dir = std::env::temp_dir().join("podspine-blocked-rename");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(out.join("001.m4a")).unwrap();
        let input = dir.join("in.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg synth failed");

        let err = transcode_whole(&input, &out, 0, "m4a", 3.0, Encoding::Aac)
            .expect_err("the blocked rename must surface");
        assert!(matches!(err, SplitError::Publish { idx: 0, .. }), "{err:?}");
        assert!(
            out.join("001.m4a").is_dir(),
            "the blocker is left exactly as it was"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_encode_leaves_the_published_episode_untouched() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // The re-ingest hazard: an episode is already published and being served
        // when a new encode of it fails. The old bytes — and the byte_length the
        // feed advertises for them — must survive, and no temporary may be left.
        let dir = std::env::temp_dir().join("podspine-atomic-fail");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let published = out.join("001.m4a");
        std::fs::write(&published, b"the previously published episode").unwrap();
        let before = std::fs::read(&published).unwrap();

        let bad = dir.join("notaudio.flac");
        std::fs::write(&bad, b"definitely not an audio stream").unwrap();
        let err = transcode_whole(&bad, &out, 0, "m4a", 12.5, Encoding::Aac)
            .expect_err("bad input must fail");
        assert!(matches!(err, SplitError::Ffmpeg { idx: 0, .. }), "{err:?}");

        assert_eq!(
            std::fs::read(&published).unwrap(),
            before,
            "a failed encode must not touch the file being served"
        );
        assert!(
            !part_path(&published).exists(),
            "a failed encode must clean up its temporary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_encode_only_appears_at_the_final_path() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        let dir = std::env::temp_dir().join("podspine-atomic-ok");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = dir.join("in.m4a");
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg synth failed");

        let ep = transcode_whole(&input, &out, 0, "m4a", 4.0, Encoding::Aac).unwrap();
        assert_eq!(ep.path, out.join("001.m4a"));
        assert_eq!(
            ep.byte_length,
            std::fs::metadata(&ep.path).unwrap().len(),
            "byte_length is measured on the file that got published"
        );
        assert!(
            !part_path(&ep.path).exists(),
            "the temporary is renamed away, not left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_chapter_maps_a_nonzero_ffmpeg_exit_to_an_error() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // `split_chapter` (the saver/regen entry point) on a non-audio input:
        // ffmpeg exits non-zero → the error arm, carrying the chapter index.
        let dir = std::env::temp_dir().join("podspine-splitchap-fail");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let bad = dir.join("notaudio.m4a");
        std::fs::write(&bad, b"definitely not an audio stream").unwrap();
        let ch = ChapterCut {
            idx: 2,
            start_sec: 0.0,
            end_sec: 5.0,
        };
        let err = split_chapter(&bad, &out, &ch, "m4a").expect_err("bad input must fail");
        assert!(matches!(err, SplitError::Ffmpeg { idx: 2, .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remux_faststart_produces_a_deterministic_faststart_file() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        let dir = std::env::temp_dir().join("podspine-remux-ft");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // ffmpeg's mp4 muxer writes `moov` at the END unless +faststart is asked,
        // so a plain encode gives us a non-faststart source.
        let src = dir.join("src.m4a");
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
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            skip!("no aac encoder");
        }

        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let ep = remux_faststart(&src, &out, 0, "m4a", 3.0).unwrap();
        assert_eq!(ep.idx, 0);
        assert_eq!(ep.duration_sec, 3.0);
        assert!(ep.byte_length > 0);
        assert_eq!(ep.path, out.join("001.m4a"));

        // The output is faststart: `moov` now precedes `mdat` in the byte stream.
        let find = |hay: &[u8], n: &[u8]| hay.windows(n.len()).position(|w| w == n);
        let bytes = std::fs::read(&ep.path).unwrap();
        let (moov, mdat) = (find(&bytes, b"moov"), find(&bytes, b"mdat"));
        assert!(
            moov.is_some() && mdat.is_some() && moov < mdat,
            "moov must precede mdat (faststart)"
        );

        // Byte-deterministic: a second remux is identical, so the recorded
        // enclosure length stays valid across cache eviction + regeneration.
        let out2 = dir.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        let ep2 = remux_faststart(&src, &out2, 0, "m4a", 3.0).unwrap();
        assert_eq!(ep.byte_length, ep2.byte_length);
        assert_eq!(
            std::fs::read(&ep.path).unwrap(),
            std::fs::read(&ep2.path).unwrap(),
            "remux is byte-identical run-to-run"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remux_maps_a_nonzero_ffmpeg_exit_to_a_split_error() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // A non-audio input makes ffmpeg exit non-zero → the remux error arm.
        let dir = std::env::temp_dir().join("podspine-remux-fail");
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let bad = dir.join("notaudio.m4a");
        std::fs::write(&bad, b"definitely not an audio stream").unwrap();
        let err = remux_faststart(&bad, &out, 0, "m4a", 3.0).expect_err("bad input must fail");
        assert!(matches!(err, SplitError::Ffmpeg { idx: 0, .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semaphore_blocks_a_second_acquirer_until_release() {
        use std::sync::Arc;
        // One permit: a second acquirer must wait on the condvar (the wait path)
        // until the first permit drops.
        let sem = Arc::new(Semaphore {
            permits: Mutex::new(1),
            cv: Condvar::new(),
        });
        let p1 = sem.acquire();
        assert_eq!(*sem.permits.lock().unwrap(), 0);
        let sem2 = Arc::clone(&sem);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done2 = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            let _p2 = sem2.acquire(); // blocks on cv.wait until p1 is released
            done2.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "second acquirer is still blocked while the permit is held"
        );
        drop(p1); // release → wakes the waiter
        handle.join().unwrap();
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn extract_cover_maps_a_nonzero_ffmpeg_exit_to_a_cover_error() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // No video stream to map -> ffmpeg exits non-zero -> CoverError::Ffmpeg.
        let dir = std::env::temp_dir().join("podspine-cover-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("notaudio.m4a");
        std::fs::write(&bad, b"no video here").unwrap();
        let err =
            extract_cover(&bad, &dir.join("out"), "jpg").expect_err("no cover stream must fail");
        assert!(matches!(err, CoverError::Ffmpeg { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_cover_thumb_maps_a_bad_input_to_a_cover_error() {
        if !have_ffmpeg() {
            skip!("ffmpeg not available");
        }
        // A non-image input -> ffmpeg can't decode it -> CoverError (Ffmpeg or the
        // empty-output guard), never a panic.
        let dir = std::env::temp_dir().join("podspine-thumb-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("notanimage.png");
        std::fs::write(&bad, b"definitely not an image").unwrap();
        let err = extract_cover_thumb(&bad, &dir.join("out")).expect_err("bad input must fail");
        assert!(
            matches!(
                err,
                CoverError::Ffmpeg { .. } | CoverError::OutputMissing { .. }
            ),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_cover_reports_create_dir_when_out_dir_is_a_file() {
        // out_dir is an existing regular file -> create_dir_all fails BEFORE any
        // ffmpeg spawn -> CoverError::CreateDir. ffmpeg-free, so it runs everywhere.
        let dir = std::env::temp_dir().join("podspine-cover-createdir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_as_dir = dir.join("iam-a-file");
        std::fs::write(&file_as_dir, b"x").unwrap();
        let err = extract_cover(Path::new("irrelevant.m4b"), &file_as_dir, "jpg")
            .expect_err("out_dir being a file must fail");
        assert!(matches!(err, CoverError::CreateDir { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_book_reports_create_dir_when_out_dir_is_a_file() {
        // Same guard on the chaptered path: out_dir is a file -> SplitError::CreateDir
        // before any chapter is cut (ffmpeg-free).
        let dir = std::env::temp_dir().join("podspine-split-createdir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_as_dir = dir.join("iam-a-file");
        std::fs::write(&file_as_dir, b"x").unwrap();
        let ch = ChapterCut {
            idx: 0,
            start_sec: 0.0,
            end_sec: 10.0,
        };
        let err = split_book(Path::new("irrelevant.m4b"), &file_as_dir, &[ch], "m4a")
            .expect_err("out_dir being a file must fail");
        assert!(matches!(err, SplitError::CreateDir { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
