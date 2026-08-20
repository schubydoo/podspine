//! Test-only helpers shared across the workspace: the ffmpeg availability
//! probe (+ skip macros), panic-safe scratch directories, and the lavfi
//! fixture-synthesis commands the pipeline tests all need. Consumed strictly
//! as a `[dev-dependencies]` entry — nothing here ships in the server.

use std::env;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `true` when `ffmpeg` is invocable on this machine (`ffmpeg -version`
/// exits 0). Tests use it (via [`skip_unless_ffmpeg!`]) to skip rather than
/// fail on boxes without ffmpeg; production code has its own typed
/// `preflight()` in `podspine-config` instead.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip the rest of a test that this machine cannot run — no ffmpeg, or no
/// encoder for the format the test needs.
///
/// The `eprintln!` and the `return` live here, once, instead of at every call
/// site. Wherever ffmpeg IS installed (CI, most dev machines), they cannot
/// execute, and 62 copies of them dominated the uncovered-line count (35 of
/// the 56 lines in PR 152's diff).
///
/// Measured, not assumed: llvm-cov attributes a macro body to each
/// **expansion**, not to the definition, so a call site still reports one
/// unreachable line. One line instead of two took 63 lines off the report
/// (scanner 158 → 105 misses, splitter 45 → 35). One line per skip is the
/// floor for a runtime decision; something has to stand for "this did not
/// run".
#[macro_export]
macro_rules! skip {
    ($($why:tt)*) => {{
        eprintln!("skipping: {}", format_args!($($why)*));
        return;
    }};
}

/// [`skip!`] the test unless ffmpeg is invocable: the opener of every
/// ffmpeg-gated test.
#[macro_export]
macro_rules! skip_unless_ffmpeg {
    () => {
        if !$crate::ffmpeg_available() {
            $crate::skip!("ffmpeg not available");
        }
    };
}

/// A per-test scratch directory under the system temp dir, wiped on creation
/// and removed again on drop, so that a test that panics leaves no litter
/// behind. It derefs to [`Path`], so call sites use it exactly like the
/// `PathBuf` that the old hand-rolled code produced.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Defuse the drop cleanup and hand the path over, for the rare test
    /// that must leave the directory alive past its body (e.g. one that
    /// spawned a detached watcher thread that still holds the dir open).
    pub fn keep(self) -> PathBuf {
        let path = self.path.clone();
        std::mem::forget(self);
        path
    }
}

impl Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A fresh, empty scratch dir at `<temp>/podspine-<name>`. `name` must be
/// unique per test (the dir is wiped on creation, and the parallel test
/// runner would otherwise race two tests over one path).
pub fn scratch(name: &str) -> ScratchDir {
    let path = env::temp_dir().join(format!("podspine-{name}"));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    ScratchDir { path }
}

/// Synthesize a chapterless AAC audio file of `secs` seconds at `dir/name`
/// (lavfi `sine` source). Panics if ffmpeg fails.
pub fn synth_sine(dir: &Path, name: &str, secs: f64) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:a",
            "aac",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg synth failed");
    out
}

/// Synthesize an AAC file at `dir/name` with three embedded 10-second
/// chapters titled One/Two/Three (30 s total). The ffmetadata sidecar is
/// written to `dir/meta.txt`.
pub fn synth_three_chapters(dir: &Path, name: &str) -> PathBuf {
    let meta = dir.join("meta.txt");
    fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30000\ntitle=Three\n",
    )
    .unwrap();
    let input = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=30",
            "-i",
        ])
        .arg(&meta)
        .args(["-map_metadata", "1", "-map", "0:a", "-c:a", "aac"])
        .arg(&input)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg synth failed");
    input
}

/// Synthesize an AAC file with an embedded (attached-picture) cover at
/// `dir/cover.m4a`.
pub fn synth_with_cover(dir: &Path) -> PathBuf {
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

/// Synthesize a `size`×`size` PNG cover image at `dir/cover.png` (lavfi
/// `color` source, one frame).
pub fn synth_cover_png(dir: &Path, size: u32) -> PathBuf {
    let out = dir.join("cover.png");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=blue:s={size}x{size}:d=0.1"),
            "-frames:v",
            "1",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg cover synth failed");
    out
}
