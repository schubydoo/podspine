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
use podspine_scanner::{ScanOptions, spawn_library_watcher};

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
    let scan_opts = ScanOptions {
        force_embedded: config.force_embedded_chapters,
        storage: config.storage_mode,
        remux_non_faststart: config.remux_non_faststart,
        transcode: config.transcode,
    };

    let state = AppState::new(
        index,
        config.base_url.clone(),
        &config.data_dir,
        &config.library,
        config.default_cover_url.clone(),
        config.storage_mode,
        config.cache_size_bytes,
        config.cache_ttl,
    )
    .context("canonicalizing the data dir / library root")?;

    // First-run UX (issue 159): don't hold the HTTP port down behind the initial
    // reconcile. A large first scan takes minutes to split, and anything in front
    // of the server (a reverse proxy, a Funnel) returns 502 the whole time. Instead
    // mark the state "scanning" and hand the initial reconcile to the watcher's
    // background thread, which registers the filesystem watch first, runs the
    // reconcile, then flips the state to "ready" via the callback. Until then
    // `GET /` shows a "Scanning…" page and the capability routes answer 503 +
    // Retry-After rather than a 502/404. A warm restart reaches ready almost
    // immediately — the reconcile's idempotency early-returns.
    //
    // Watch-before-scan (not scan-before-watch): a library change that lands during
    // the initial scan is buffered and reconciled by the watcher rather than lost
    // until the next event or a restart. The watcher owns the only index writer, so
    // exactly one reconciler ever runs.
    state.set_ready(false);
    {
        let state = state.clone();
        spawn_library_watcher(
            config.library.clone(),
            config.data_dir.clone(),
            db_path,
            scan_opts,
            move || state.set_ready(true),
        );
    }

    if let Some((listener, handle)) = metrics {
        tokio::spawn(async move {
            if let Err(err) = podspine_metrics::serve(listener, handle).await {
                tracing::error!(error = %err, "metrics listener stopped");
            }
        });
    }

    serve(config.bind, state).await.context("serving")?;
    Ok(())
}
