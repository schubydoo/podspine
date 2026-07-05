//! `podspine-cli` — the Sprint 1 POC.
//!
//! Takes one audiobook file, runs the full pipeline —
//! `prober -> splitter -> feed -> self-check` — and writes per-chapter episode
//! files plus a validated `feed.xml`. This exists to prove the hardest piece
//! (correct M4B split + sequential pubDates) before any server work (Task 1.6).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow};
use clap::Parser;

use podspine_feed::{FeedBook, FeedEpisode, render_checked};
use podspine_prober::probe;
use podspine_splitter::{ChapterCut, split_book};

#[derive(Parser)]
#[command(
    name = "podspine-cli",
    about = "Split one audiobook into per-chapter episodes and emit a podcast feed"
)]
struct Args {
    /// Input audiobook file (.m4b/.m4a with embedded chapters).
    #[arg(long)]
    input: PathBuf,
    /// Output directory for split episodes + feed.xml.
    #[arg(long)]
    out: PathBuf,
    /// Base URL used to build enclosure and feed URLs.
    #[arg(long, default_value = "http://localhost:8080")]
    base_url: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    run(&args)
}

fn run(args: &Args) -> Result<()> {
    let probed = probe(&args.input).with_context(|| format!("probing {}", args.input.display()))?;

    let base = args.base_url.trim_end_matches('/');
    let id = slugify(&file_stem(&args.input));
    let source_mtime = mtime_epoch(&args.input)?;
    let book_out = args.out.join("books").join(&id);

    // Chapters -> (cut, title). A chapter-less book degrades to a single episode
    // spanning the whole file, with a surfaced warning (Task 1.7). Corrupt input
    // never reaches here — probe() already returned a typed error above.
    let specs: Vec<(ChapterCut, String)> = if probed.chapters.is_empty() {
        eprintln!(
            "warning: {} has no embedded chapters — emitting a single-episode feed",
            args.input.display()
        );
        vec![(
            ChapterCut {
                idx: 0,
                start_sec: 0.0,
                end_sec: probed.duration_sec,
            },
            file_stem(&args.input),
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

    let cuts: Vec<ChapterCut> = specs.iter().map(|(cut, _)| cut.clone()).collect();
    let episodes = split_book(&args.input, &book_out, &cuts, "m4a")
        .with_context(|| format!("splitting {}", args.input.display()))?;

    let feed_episodes: Vec<FeedEpisode> = episodes
        .iter()
        .zip(&specs)
        .map(|(ep, (_, title))| FeedEpisode {
            idx: ep.idx,
            title: title.clone(),
            audio_url: format!("{base}/audio/{id}/{:03}.m4a", ep.idx + 1),
            byte_length: ep.byte_length,
            duration_sec: ep.duration_sec,
            mime_type: "audio/mp4".to_string(),
        })
        .collect();

    let book = FeedBook {
        id: id.clone(),
        title: file_stem(&args.input),
        author: None,
        description: None,
        cover_url: None,
        source_mtime,
        self_url: format!("{base}/feed/{id}.xml"),
        blocked: false,
        episodes: feed_episodes,
    };

    // Build + self-check + render — refuses to write a broken feed.
    let xml = render_checked(&book).map_err(|errs| anyhow!("feed self-check failed: {errs:?}"))?;

    fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let feed_path = args.out.join("feed.xml");
    fs::write(&feed_path, xml).with_context(|| format!("writing {}", feed_path.display()))?;

    println!(
        "ok: {} episodes -> {}\n     feed -> {}",
        episodes.len(),
        book_out.display(),
        feed_path.display()
    );
    Ok(())
}

/// File stem as a lossy string (fallback `"book"`).
fn file_stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string())
}

/// Lowercase ASCII slug: alphanumerics kept, runs of anything else become a
/// single `-`. Falls back to `"book"` if nothing survives.
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
fn mtime_epoch(p: &Path) -> Result<i64> {
    let modified = fs::metadata(p)
        .with_context(|| format!("stat {}", p.display()))?
        .modified()
        .with_context(|| format!("mtime of {}", p.display()))?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}
