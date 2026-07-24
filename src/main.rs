//! Podspine server entrypoint: `config -> scan -> watch -> http`.
//!
//! Resolves configuration (validating the library and preflighting ffmpeg),
//! opens the index, reconciles the library of audiobooks into it (scan + prune),
//! spawns a background watcher that auto-refreshes on library changes, then
//! serves feeds + Range audio. Multi-book scanning and the watcher live in
//! [`podspine_scanner`].

use anyhow::{Context, Result};
use podspine_config::{Config, StorageMode};
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

    // Install the recorder (and bind its listener) before the first reconcile,
    // so startup ingest is measured rather than silently dropped. Both failures
    // are fatal: metrics were explicitly asked for, so a missing endpoint should
    // surface here, not as a gap in a dashboard.
    let metrics = match config.metrics_bind {
        Some(bind) => {
            let handle = podspine_metrics::install()
                .map_err(|err| anyhow::anyhow!("{err}"))
                .context("installing the metrics recorder")?;
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("binding the metrics listener on {bind}"))?;
            tracing::info!(%bind, "podspine metrics listening");
            Some((listener, handle))
        }
        None => None,
    };

    let db_path = config.data_dir.join("podspine.db");
    let index = Index::open(&db_path).context("opening the index")?;
    let saver = matches!(config.storage_mode, StorageMode::Saver);

    // Initial reconcile: index new/changed books and prune ones deleted while the
    // server was down.
    reconcile(
        &config.library,
        &config.data_dir,
        &index,
        config.force_embedded_chapters,
        saver,
        config.remux_non_faststart,
    );

    // Auto-refresh: a background thread (its own WAL index connection) re-runs the
    // reconcile whenever the library changes, so feeds appear without a restart.
    spawn_library_watcher(
        config.library.clone(),
        config.data_dir.clone(),
        db_path,
        config.force_embedded_chapters,
        saver,
        config.remux_non_faststart,
    );

    if let Some((listener, handle)) = metrics {
        tokio::spawn(async move {
            if let Err(err) = podspine_metrics::serve(listener, handle).await {
                tracing::error!(error = %err, "metrics listener stopped");
            }
        });
    }

    let state = AppState::new(
        index,
        config.base_url.clone(),
        &config.data_dir,
        &config.library,
        config.default_cover_url.clone(),
        saver,
        config.cache_size_bytes,
        config.cache_ttl,
    );
    serve(config.bind, state).await.context("serving")?;
    Ok(())
}
