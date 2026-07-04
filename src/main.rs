//! Podspine server entrypoint: `config -> scan -> http`.
//!
//! Resolves configuration (validating the library and preflighting ffmpeg),
//! opens the index, scans the library's top-level audio files into it, then
//! serves feeds + Range audio. MVP scans a flat directory; folder-of-files
//! audiobooks and robust multi-book scanning are Sprint 3 (Tasks 3.1/3.3).

use anyhow::{Context, Result};
use podspine_config::Config;
use podspine_http::{AppState, serve};
use podspine_index::Index;
use podspine_scanner::scan_book;

/// Extensions we attempt to ingest at the top level (DRM is skipped inside the
/// scanner; other formats arrive in Sprint 3).
const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3"];

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load().context("resolving configuration")?;
    let index = Index::open(config.data_dir.join("podspine.db")).context("opening the index")?;

    scan_library(&config, &index);

    let state = AppState::new(index, config.base_url.clone(), &config.data_dir);
    serve(config.bind, state).await.context("serving")?;
    Ok(())
}

/// Scan the top level of the library directory, indexing each audio file. Bad or
/// unsupported files are logged and skipped, never fatal.
fn scan_library(config: &Config, index: &Index) {
    let entries = match std::fs::read_dir(&config.library) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::error!(error = %err, library = %config.library.display(), "cannot read library");
            return;
        }
    };

    let mut indexed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_audio = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !path.is_file() || !is_audio {
            continue;
        }

        match scan_book(&path, &config.data_dir, index) {
            Ok(book) => {
                indexed += 1;
                tracing::info!(slug = %book.slug, title = %book.title, "indexed book");
            }
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "skipped");
            }
        }
    }
    tracing::info!(indexed, "library scan complete");
}
