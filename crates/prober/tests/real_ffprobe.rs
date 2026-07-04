//! Integration test that runs the *real* `ffprobe` against a real audiobook.
//!
//! Fixtures are large and local-only, so this is existence-gated: it points at
//! `$PODSPINE_TEST_M4B` if set, else a generic local fixture, and quietly skips
//! when neither the file nor ffprobe is available (keeps CI green, runs
//! automatically on a dev box that has the fixture). Assertions are structural
//! so any DRM-free multi-chapter M4B works as the fixture.

use std::path::PathBuf;

use podspine_prober::probe;

/// Golden-path fixture: a DRM-free M4B with embedded chapters and cover art.
const DEFAULT_M4B: &str = "/home/claude/audiobooks/fixture-embedded-chapters.m4b";

fn fixture() -> Option<PathBuf> {
    let path = std::env::var("PODSPINE_TEST_M4B")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_M4B));
    path.exists().then_some(path)
}

#[test]
fn probes_a_real_multichapter_m4b() {
    let Some(path) = fixture() else {
        eprintln!("skipping: no fixture (set PODSPINE_TEST_M4B or place a file at {DEFAULT_M4B})");
        return;
    };

    let book = probe(&path).expect("probe should succeed on a real DRM-free M4B");

    // A multi-chapter fixture yields multiple chapters.
    assert!(
        book.chapters.len() >= 2,
        "expected an embedded-chapter fixture, got {} chapters",
        book.chapters.len()
    );
    assert!(book.duration_sec > 0.0, "duration should be positive");

    // pubDate ordering depends on this: chapter starts must strictly increase,
    // and every chapter must sit within the book (+1s slack).
    for w in book.chapters.windows(2) {
        assert!(
            w[0].start_sec < w[1].start_sec,
            "chapter {} starts before {}",
            w[1].idx,
            w[0].idx
        );
    }
    for c in &book.chapters {
        assert!(
            c.end_sec >= c.start_sec,
            "chapter {} ends before it starts",
            c.idx
        );
        assert!(
            c.end_sec <= book.duration_sec + 1.0,
            "chapter {} runs past the book duration",
            c.idx
        );
    }
}
