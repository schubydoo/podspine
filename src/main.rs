//! Podspine server entrypoint: `config -> scan -> watch -> http`.
//!
//! Resolves configuration (validating the library and preflighting ffmpeg),
//! opens the index, reconciles the library of audiobooks into it (scan + prune),
//! spawns a background watcher that auto-refreshes on library changes, then
//! serves feeds + Range audio. Multi-book scanning and the watcher live in
//! [`podspine_scanner`].

use anyhow::{Context, Result};
use podspine_config::Config;
use podspine_http::{AppState, serve};
use podspine_index::Index;
use podspine_scanner::{reconcile, spawn_library_watcher};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load().context("resolving configuration")?;
    let db_path = config.data_dir.join("podspine.db");
    let index = Index::open(&db_path).context("opening the index")?;

    // Initial reconcile: index new/changed books and prune ones deleted while the
    // server was down.
    reconcile(
        &config.library,
        &config.data_dir,
        &index,
        config.force_embedded_chapters,
    );

    // Auto-refresh: a background thread (its own WAL index connection) re-runs the
    // reconcile whenever the library changes, so feeds appear without a restart.
    spawn_library_watcher(
        config.library.clone(),
        config.data_dir.clone(),
        db_path,
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
