//! `prober` — thin `ffprobe` wrapper.
//!
//! Runs `ffprobe -v error -print_format json -show_format -show_streams
//! -show_chapters <file>` and parses the result into a [`ProbedBook`]:
//! embedded chapters (from decimal `start_time`/`end_time` strings, **not** raw
//! `start` + `time_base`), the format duration, and whether a cover-art stream is
//! present. See TAD §4.
//!
//! ## Design notes
//! - The subprocess call passes every argument as a separate argv element (no
//!   shell string) — a habit kept consistent with the splitter, where chapter
//!   titles are untrusted (command-injection surface).
//! - Parsing is split out into [`parse_probe_json`] so the parser can be
//!   unit-tested hermetically, without ffprobe or a real audiobook on disk.
//! - **Chapter-less is not an error.** A readable file with zero chapter markers
//!   returns `Ok` with an empty [`ProbedBook::chapters`]; the single-episode
//!   fallback is decided downstream (Task 1.7). Only an unreadable/corrupt file,
//!   a non-zero ffprobe exit, unparseable JSON, or a file with no audio stream
//!   yields a typed [`ProbeError`].

use std::path::Path;
use std::process::Command;

/// One embedded chapter, with times already resolved to seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// Zero-based position in the book (chapter order as reported by ffprobe).
    pub idx: usize,
    /// Start offset in seconds (from `start_time`).
    pub start_sec: f64,
    /// End offset in seconds (from `end_time`).
    pub end_sec: f64,
    /// Chapter title (`tags.title`), if present.
    pub title: Option<String>,
}

impl Chapter {
    /// Chapter length in seconds (`end - start`, floored at 0).
    pub fn duration_sec(&self) -> f64 {
        (self.end_sec - self.start_sec).max(0.0)
    }
}

/// The result of probing one audiobook file.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbedBook {
    /// Total duration in seconds.
    pub duration_sec: f64,
    /// Codec of the first audio stream (e.g. `"aac"`, `"mp3"`, `"flac"`,
    /// `"vorbis"`, `"opus"`). Picks the stream-copy output container (Task 3.9).
    pub audio_codec: Option<String>,
    /// Whether the file carries an embedded cover-art stream.
    pub has_cover: bool,
    /// Codec of the embedded cover stream (e.g. `"mjpeg"`, `"png"`), if any.
    /// Lets the extractor stream-copy the cover to the right file extension.
    pub cover_codec: Option<String>,
    /// Container-level track number (`format.tags.track`, leading integer of
    /// e.g. `"3"` or `"3/12"`). Used to order per-chapter MP3 folders (Task 3.3).
    pub track: Option<u32>,
    /// Container-level title tag (`format.tags.title`), if non-empty. A nicer
    /// per-episode title than the filename for MP3-folder tracks.
    pub title: Option<String>,
    /// Embedded chapters in file order. Empty for a chapter-less file.
    pub chapters: Vec<Chapter>,
}

/// Everything that can go wrong probing a file. Chapter-less input is *not* here
/// — it is a valid [`ProbedBook`] with no chapters.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// `ffprobe` could not be launched (not on PATH, permissions, …).
    #[error("failed to launch ffprobe (is it installed and on PATH?): {0}")]
    Spawn(#[source] std::io::Error),
    /// `ffprobe` ran but exited non-zero (corrupt/unsupported input).
    #[error("ffprobe exited with {}", match .code { Some(c) => c.to_string(), None => "signal".into() })]
    Ffprobe {
        /// Process exit code, if the process was not killed by a signal.
        code: Option<i32>,
    },
    /// `ffprobe` output was not the expected JSON.
    #[error("could not parse ffprobe JSON output: {0}")]
    Json(#[source] serde_json::Error),
    /// A chapter carried a `start_time`/`end_time` that was not a decimal number.
    #[error("chapter {idx} has an unparseable {field} ({value:?})")]
    InvalidChapterTime {
        /// Zero-based chapter position.
        idx: usize,
        /// Which field failed (`start_time` or `end_time`).
        field: &'static str,
        /// The raw string ffprobe reported.
        value: String,
    },
    /// The file has no audio stream — it is not a playable audiobook.
    #[error("no audio stream found in file")]
    NoAudioStream,
    /// Duration could not be determined from format, chapters, or streams.
    #[error("could not determine a duration for the file")]
    DurationUnavailable,
}

/// Probe `path` with `ffprobe` and return its chapters, duration, and cover flag.
pub fn probe(path: &Path) -> Result<ProbedBook, ProbeError> {
    // argv vector — never a shell string.
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(path)
        .output()
        .map_err(ProbeError::Spawn)?;

    if !output.status.success() {
        return Err(ProbeError::Ffprobe {
            code: output.status.code(),
        });
    }

    let json = String::from_utf8_lossy(&output.stdout);
    parse_probe_json(&json)
}

// ---- JSON model (only the fields we consume) ------------------------------

mod raw {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Probe {
        #[serde(default)]
        pub streams: Vec<Stream>,
        #[serde(default)]
        pub chapters: Vec<Chapter>,
        pub format: Option<Format>,
    }

    #[derive(Deserialize)]
    pub struct Stream {
        pub codec_type: Option<String>,
        pub codec_name: Option<String>,
        pub duration: Option<String>,
        #[serde(default)]
        pub disposition: Disposition,
    }

    #[derive(Deserialize, Default)]
    pub struct Disposition {
        #[serde(default)]
        pub attached_pic: i32,
    }

    #[derive(Deserialize)]
    pub struct Chapter {
        pub start_time: Option<String>,
        pub end_time: Option<String>,
        #[serde(default)]
        pub tags: Tags,
    }

    #[derive(Deserialize, Default)]
    pub struct Tags {
        pub title: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct Format {
        pub duration: Option<String>,
        #[serde(default)]
        pub tags: FormatTags,
    }

    #[derive(Deserialize, Default)]
    pub struct FormatTags {
        pub track: Option<String>,
        pub title: Option<String>,
    }
}

/// Parse the JSON emitted by our `ffprobe` invocation into a [`ProbedBook`].
///
/// Separated from [`probe`] so it can be tested without ffprobe or a real file.
pub fn parse_probe_json(json: &str) -> Result<ProbedBook, ProbeError> {
    let probe: raw::Probe = serde_json::from_str(json).map_err(ProbeError::Json)?;

    let audio_stream = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));
    let Some(audio_stream) = audio_stream else {
        return Err(ProbeError::NoAudioStream);
    };
    let audio_codec = audio_stream.codec_name.clone();

    let cover_stream = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic == 1);
    let has_cover = cover_stream.is_some();
    let cover_codec = cover_stream.and_then(|s| s.codec_name.clone());

    let mut chapters = Vec::with_capacity(probe.chapters.len());
    for (idx, c) in probe.chapters.iter().enumerate() {
        let start_sec = parse_time(c.start_time.as_deref(), idx, "start_time")?;
        let end_sec = parse_time(c.end_time.as_deref(), idx, "end_time")?;
        chapters.push(Chapter {
            idx,
            start_sec,
            end_sec,
            title: c.tags.title.clone().filter(|t| !t.is_empty()),
        });
    }

    // Duration: prefer format.duration, then the audio stream, then the last
    // chapter's end. One of these is essentially always present.
    let duration_sec = probe
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            audio_stream
                .duration
                .as_deref()
                .and_then(|d| d.parse::<f64>().ok())
        })
        .or_else(|| chapters.last().map(|c| c.end_sec))
        .ok_or(ProbeError::DurationUnavailable)?;

    let (track, title) = probe
        .format
        .as_ref()
        .map(|f| {
            (
                parse_track(f.tags.track.as_deref()),
                f.tags.title.clone().filter(|t| !t.is_empty()),
            )
        })
        .unwrap_or((None, None));

    Ok(ProbedBook {
        duration_sec,
        audio_codec,
        has_cover,
        cover_codec,
        track,
        title,
        chapters,
    })
}

/// Parse a track tag (`"3"`, `"03"`, `"3/12"`) into its leading integer.
fn parse_track(raw: Option<&str>) -> Option<u32> {
    let s = raw?.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

/// Parse a decimal-seconds string (ffprobe `start_time`/`end_time`) into `f64`.
fn parse_time(value: Option<&str>, idx: usize, field: &'static str) -> Result<f64, ProbeError> {
    let raw = value.unwrap_or_default();
    raw.parse::<f64>()
        .map_err(|_| ProbeError::InvalidChapterTime {
            idx,
            field,
            value: raw.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but faithful ffprobe payload: one mono AAC audio stream, one
    // attached-picture cover stream, three chapters (including a very short one),
    // and a format duration.
    const THREE_CHAPTERS: &str = r#"{
        "streams": [
            {"codec_type": "audio", "codec_name": "aac", "duration": "95.000000",
             "disposition": {"attached_pic": 0}},
            {"codec_type": "video", "codec_name": "mjpeg",
             "disposition": {"attached_pic": 1}}
        ],
        "chapters": [
            {"start_time": "0.000000", "end_time": "31.312000", "tags": {"title": "Introduction"}},
            {"start_time": "31.312000", "end_time": "33.600000", "tags": {"title": "Part 1"}},
            {"start_time": "33.600000", "end_time": "95.000000", "tags": {"title": "Chapter 1"}}
        ],
        "format": {"duration": "95.000000"}
    }"#;

    #[test]
    fn parses_chapters_duration_and_cover() {
        let book = parse_probe_json(THREE_CHAPTERS).unwrap();
        assert_eq!(book.chapters.len(), 3);
        assert!(book.has_cover);
        assert_eq!(book.cover_codec.as_deref(), Some("mjpeg"));
        assert_eq!(book.audio_codec.as_deref(), Some("aac"));
        assert!((book.duration_sec - 95.0).abs() < 1e-9);

        let c0 = &book.chapters[0];
        assert_eq!(c0.idx, 0);
        assert!((c0.start_sec - 0.0).abs() < 1e-9);
        assert!((c0.end_sec - 31.312).abs() < 1e-9);
        assert_eq!(c0.title.as_deref(), Some("Introduction"));

        // Short middle chapter — exercises small segments.
        assert!((book.chapters[1].duration_sec() - 2.288).abs() < 1e-9);
    }

    #[test]
    fn chapter_starts_are_monotonic() {
        let book = parse_probe_json(THREE_CHAPTERS).unwrap();
        for w in book.chapters.windows(2) {
            assert!(w[0].start_sec < w[1].start_sec);
        }
    }

    #[test]
    fn parses_track_and_title_tags() {
        let json = r#"{
            "streams": [{"codec_type": "audio", "disposition": {"attached_pic": 0}}],
            "chapters": [],
            "format": {"duration": "180.0", "tags": {"track": "3/12", "title": "The Third"}}
        }"#;
        let book = parse_probe_json(json).unwrap();
        assert_eq!(book.track, Some(3));
        assert_eq!(book.title.as_deref(), Some("The Third"));
    }

    #[test]
    fn track_parsing_handles_edge_cases() {
        assert_eq!(parse_track(Some("3")), Some(3));
        assert_eq!(parse_track(Some("03")), Some(3));
        assert_eq!(parse_track(Some("7/20")), Some(7));
        assert_eq!(parse_track(Some("")), None);
        assert_eq!(parse_track(Some("none")), None);
        assert_eq!(parse_track(None), None);
    }

    #[test]
    fn chapterless_file_is_ok_with_no_chapters() {
        let json = r#"{
            "streams": [{"codec_type": "audio", "codec_name": "aac", "duration": "600.0",
                         "disposition": {"attached_pic": 0}}],
            "chapters": [],
            "format": {"duration": "600.000000"}
        }"#;
        let book = parse_probe_json(json).unwrap();
        assert!(book.chapters.is_empty());
        assert!(!book.has_cover);
        assert!((book.duration_sec - 600.0).abs() < 1e-9);
    }

    #[test]
    fn missing_title_becomes_none() {
        let json = r#"{
            "streams": [{"codec_type": "audio", "disposition": {"attached_pic": 0}}],
            "chapters": [{"start_time": "0.0", "end_time": "10.0", "tags": {}}],
            "format": {"duration": "10.0"}
        }"#;
        let book = parse_probe_json(json).unwrap();
        assert_eq!(book.chapters[0].title, None);
    }

    #[test]
    fn no_audio_stream_is_an_error() {
        let json = r#"{
            "streams": [{"codec_type": "video", "disposition": {"attached_pic": 1}}],
            "chapters": [],
            "format": {"duration": "10.0"}
        }"#;
        assert!(matches!(
            parse_probe_json(json),
            Err(ProbeError::NoAudioStream)
        ));
    }

    #[test]
    fn duration_falls_back_to_stream_then_chapters() {
        // No format.duration -> use the audio stream's duration.
        let json = r#"{
            "streams": [{"codec_type": "audio", "duration": "123.5",
                         "disposition": {"attached_pic": 0}}],
            "chapters": [],
            "format": {}
        }"#;
        assert!((parse_probe_json(json).unwrap().duration_sec - 123.5).abs() < 1e-9);
    }

    #[test]
    fn unparseable_chapter_time_is_a_typed_error() {
        let json = r#"{
            "streams": [{"codec_type": "audio", "disposition": {"attached_pic": 0}}],
            "chapters": [{"start_time": "not-a-number", "end_time": "10.0", "tags": {}}],
            "format": {"duration": "10.0"}
        }"#;
        match parse_probe_json(json) {
            Err(ProbeError::InvalidChapterTime { idx, field, .. }) => {
                assert_eq!(idx, 0);
                assert_eq!(field, "start_time");
            }
            other => panic!("expected InvalidChapterTime, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_a_typed_error() {
        assert!(matches!(
            parse_probe_json("{not json"),
            Err(ProbeError::Json(_))
        ));
    }
}
