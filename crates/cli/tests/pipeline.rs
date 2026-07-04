//! End-to-end POC test: synthesize a multi-chapter audiobook with ffmpeg, run
//! the real `podspine-cli` binary over it, and assert the episodes + feed.
//!
//! Unlike the large real-audio fixtures, this builds a tiny synthetic file, so it
//! runs in CI (where ffmpeg is installed). It skips only if ffmpeg is absent.

use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a 30s AAC file with three 10s embedded chapters via an ffmetadata file.
fn synthesize(dir: &Path) -> PathBuf {
    let meta = dir.join("meta.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30000\ntitle=Three\n",
    )
    .unwrap();

    let input = dir.join("synthetic.m4a");
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
    assert!(status.success(), "ffmpeg failed to synthesize the fixture");
    input
}

#[test]
fn cli_splits_and_emits_a_valid_feed() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli_pipeline");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let input = synthesize(&tmp);
    let out = tmp.join("dist");

    let run = Command::new(env!("CARGO_BIN_EXE_podspine-cli"))
        .arg("--input")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("spawn podspine-cli");
    assert!(
        run.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    // feed.xml exists, has 3 items, and carries the required itunes tags (which
    // means it passed the built-in self-check before being written).
    let feed = std::fs::read_to_string(out.join("feed.xml")).expect("feed.xml written");
    assert_eq!(
        feed.matches("<item>").count(),
        3,
        "expected 3 episodes in feed"
    );
    assert_eq!(feed.matches("<itunes:duration>").count(), 3);
    assert_eq!(feed.matches("<enclosure ").count(), 3);
    assert!(feed.contains("xmlns:itunes"));

    // Three episode files were written under books/<slug>/.
    let books = out.join("books");
    let book_dir = std::fs::read_dir(&books)
        .expect("books dir exists")
        .next()
        .expect("one book dir")
        .unwrap()
        .path();
    for i in 1..=3 {
        let ep = book_dir.join(format!("{i:03}.m4a"));
        assert!(ep.exists(), "missing episode file {}", ep.display());
        assert!(
            std::fs::metadata(&ep).unwrap().len() > 0,
            "episode is empty"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
