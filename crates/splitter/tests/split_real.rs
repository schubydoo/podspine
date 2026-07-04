//! Real ffmpeg split of a real audiobook — the POC's core proof.
//!
//! Existence-gated on the local golden-path fixture (or `$PODSPINE_TEST_M4B`);
//! quietly skips in CI where the large file is absent. Splits the first few
//! chapters and asserts each output's *actual* duration matches the requested
//! duration within ±1 s — which is exactly what a `-to`/`-t` mistake (the 2×
//! bug) would violate.

use std::path::{Path, PathBuf};

use podspine_prober::probe;
use podspine_splitter::{ChapterCut, split_book};

const DEFAULT_M4B: &str = "/home/claude/audiobooks/fixture-embedded-chapters.m4b";

fn fixture() -> Option<PathBuf> {
    let path = std::env::var("PODSPINE_TEST_M4B")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_M4B));
    path.exists().then_some(path)
}

fn source_fingerprint(p: &Path) -> (u64, Option<std::time::SystemTime>) {
    let m = std::fs::metadata(p).expect("stat source");
    (m.len(), m.modified().ok())
}

#[test]
fn splits_real_chapters_without_2x_bug_and_leaves_source_untouched() {
    let Some(input) = fixture() else {
        eprintln!("skipping: no fixture (set PODSPINE_TEST_M4B or place {DEFAULT_M4B})");
        return;
    };

    let book = probe(&input).expect("probe fixture");
    assert!(book.chapters.len() >= 3, "need at least 3 chapters to test");

    // First 3 chapters (a real fixture typically includes a short one — a
    // useful tiny-segment case).
    let cuts: Vec<ChapterCut> = book.chapters[..3]
        .iter()
        .map(|c| ChapterCut {
            idx: c.idx,
            start_sec: c.start_sec,
            end_sec: c.end_sec,
        })
        .collect();

    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("split_real_out");
    let _ = std::fs::remove_dir_all(&out_dir);

    let before = source_fingerprint(&input);
    let episodes = split_book(&input, &out_dir, &cuts, "m4a").expect("split first 3 chapters");
    let after = source_fingerprint(&input);

    // Source file must be byte-for-byte untouched.
    assert_eq!(
        before, after,
        "source file must not be modified by splitting"
    );

    assert_eq!(episodes.len(), 3, "3 chapters -> 3 episodes");
    for (ep, cut) in episodes.iter().zip(&cuts) {
        // Files are numbered 1-based and land in out_dir.
        assert_eq!(ep.path, out_dir.join(format!("{:03}.m4a", cut.idx + 1)));
        assert!(ep.path.exists(), "output file exists");

        // byte_length is the REAL file size.
        let real_len = std::fs::metadata(&ep.path).unwrap().len();
        assert_eq!(
            ep.byte_length, real_len,
            "byte_length must equal fs metadata len"
        );
        assert!(ep.byte_length > 0, "output not empty");

        // The anti-2x proof: probe the produced file and compare its actual
        // duration to what we asked for (±1 s, per NFR-P1).
        let requested = cut.end_sec - cut.start_sec;
        let actual = probe(&ep.path).expect("probe output").duration_sec;
        assert!(
            (actual - requested).abs() <= 1.0,
            "chapter {} duration off: requested {requested:.3}s, got {actual:.3}s \
             (a >~2x value means -to/-t was misused)",
            cut.idx
        );
    }

    let _ = std::fs::remove_dir_all(&out_dir);
}
