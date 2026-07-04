//! `splitter` — `ffmpeg` wrapper that cuts one audiobook file into per-chapter
//! episode files by **stream copy** (no re-encode).
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
//!   the offset and yields a ~2× file — so we never emit `-to`. [`build_ffmpeg_args`]
//!   encodes this and is unit-tested without invoking ffmpeg.
//! - `byte_length` is read from the **actual output file** (`fs::metadata().len()`),
//!   never prorated from a bitrate.
//! - The source file is only ever read; every output lands in `out_dir`.
//!
//! The split is synchronous (`std::process::Command`); bounding concurrent jobs
//! with a semaphore and a per-child timeout/kill is a later, server-side concern
//! (scanner / Task 3.5), not needed for the CLI POC.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Path to the written `.m4a`.
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
    /// The output file is missing or empty after a "successful" ffmpeg run.
    #[error("chapter {idx} produced no output at {path:?}")]
    OutputMissing {
        /// Zero-based chapter position.
        idx: usize,
        /// Where the output was expected.
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

    let output = Command::new("ffmpeg")
        .args(&args)
        .output()
        .map_err(CoverError::Spawn)?;
    if !output.status.success() {
        return Err(CoverError::Ffmpeg {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
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

/// Split every chapter of `input` into `out_dir`, returning one [`SplitEpisode`]
/// per chapter (fails fast on the first error). Creates `out_dir` if needed and
/// never modifies `input`.
pub fn split_book(
    input: &Path,
    out_dir: &Path,
    chapters: &[ChapterCut],
) -> Result<Vec<SplitEpisode>, SplitError> {
    fs::create_dir_all(out_dir).map_err(|source| SplitError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;

    let mut episodes = Vec::with_capacity(chapters.len());
    for ch in chapters {
        episodes.push(split_chapter(input, out_dir, ch)?);
    }
    Ok(episodes)
}

/// Cut a single chapter. Output is `out_dir/{idx+1:03}.m4a`.
pub fn split_chapter(
    input: &Path,
    out_dir: &Path,
    ch: &ChapterCut,
) -> Result<SplitEpisode, SplitError> {
    let duration_sec = ch.end_sec - ch.start_sec;
    if duration_sec <= 0.0 {
        return Err(SplitError::EmptyChapter { idx: ch.idx });
    }

    let out_path = out_dir.join(format!("{:03}.m4a", ch.idx + 1));
    let args = build_ffmpeg_args(input, &out_path, ch.start_sec, ch.end_sec);

    let output = Command::new("ffmpeg")
        .args(&args)
        .output()
        .map_err(SplitError::Spawn)?;

    if !output.status.success() {
        return Err(SplitError::Ffmpeg {
            idx: ch.idx,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
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
            idx: ch.idx,
            path: out_path,
        });
    }

    Ok(SplitEpisode {
        idx: ch.idx,
        path: out_path,
        byte_length,
        duration_sec,
    })
}

/// Build the exact ffmpeg argv for a stream-copy chapter cut.
///
/// Factored out so the ordering invariants (`-ss` before `-i`, `-t` not `-to`,
/// `-c copy`, `+faststart`) can be asserted in a unit test without ffmpeg.
fn build_ffmpeg_args(input: &Path, output: &Path, start_sec: f64, end_sec: f64) -> Vec<OsString> {
    let duration = (end_sec - start_sec).max(0.0);
    vec![
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        // -ss BEFORE -i: fast seek via the index.
        "-ss".into(),
        fmt_secs(start_sec).into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
        // -t <duration>, NEVER -to (which with a pre-input -ss makes a 2x file).
        "-t".into(),
        fmt_secs(duration).into(),
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

/// Format seconds for ffmpeg (fixed decimal, no scientific notation).
fn fmt_secs(v: f64) -> String {
    format!("{v:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_as_strings(start: f64, end: f64) -> Vec<String> {
        build_ffmpeg_args(Path::new("in.m4b"), Path::new("out.m4a"), start, end)
            .iter()
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
    fn zero_length_chapter_errors_without_spawning_ffmpeg() {
        // end == start -> caught before any ffmpeg spawn, so the (missing) input
        // path is irrelevant.
        let ch = ChapterCut {
            idx: 3,
            start_sec: 5.0,
            end_sec: 5.0,
        };
        let err = split_chapter(Path::new("does-not-exist.m4b"), Path::new("/tmp"), &ch)
            .expect_err("zero-length chapter must error");
        assert!(matches!(err, SplitError::EmptyChapter { idx: 3 }));
    }
}
