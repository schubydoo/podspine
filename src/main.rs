//! Podspine server entrypoint: `config -> scan -> http`.
//!
//! Resolves configuration (validating the library and preflighting ffmpeg),
//! opens the index, scans the library of audiobooks into it, then serves feeds +
//! Range audio. Multi-book scanning (top-level files and per-book folders) lives
//! in [`podspine_scanner::scan_library`]; MP3-folder ingest arrives in Task 3.3.

use anyhow::{Context, Result};
use podspine_config::Config;
use podspine_http::{AppState, serve};
use podspine_index::Index;
use podspine_scanner::scan_library;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load().context("resolving configuration")?;
    let index = Index::open(config.data_dir.join("podspine.db")).context("opening the index")?;

    scan_library(
        &config.library,
        &config.data_dir,
        &index,
        config.force_embedded_chapters,
    );

    let state = AppState::new(
        index,
        config.base_url.clone(),
        &config.data_dir,
        config.default_cover_url.clone(),
    );
    serve(config.bind, state).await.context("serving")?;
    Ok(())
}
