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
use podspine_scanner::{ScanOptions, WatchSignal, spawn_library_watcher};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Resolve configuration first, then build the log subscriber from it. The
    // config crate emits no tracing during load, and a fatal config error
    // prints through `anyhow` on exit, so nothing is lost by initializing the
    // subscriber here rather than before the load.
    let config = Config::load().context("resolving configuration")?;

    let (filter, log_warnings) =
        resolve_log_filter(std::env::var("RUST_LOG").ok().as_deref(), &config.log_level);
    tracing_subscriber::fmt().with_env_filter(filter).init();
    // Print these to stderr, not through tracing. Each one reports that a log
    // filter was rejected, and the installed filter is the one just built from
    // that same input. A restrictive filter (`error`, `off`, or a target-only
    // directive) would suppress a `tracing::warn!` here and hide the diagnostic
    // the fallback exists to give. stderr always shows it.
    for message in log_warnings {
        eprintln!("podspine: {message}");
    }

    // Install the recorder (and bind its listener) before the first
    // reconcile, so that the startup ingest is measured, not silently
    // dropped. Both failures are fatal: metrics were explicitly asked for, so
    // a missing endpoint must surface here, not as a gap in a dashboard.
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

    // The watcher thread owns the single reconcile loop (the only index writer).
    // The HTTP layer cannot reconcile itself, so the per-book Refresh handler
    // sends `WatchSignal::Reconcile` down this channel after it invalidates the
    // book. The sender is behind a mutex because `mpsc::Sender` is not `Sync` and
    // `AppState` (axum state) must be.
    let (watch_tx, watch_rx) = std::sync::mpsc::channel::<WatchSignal>();
    let reconcile: Arc<dyn Fn() + Send + Sync> = {
        let tx = Mutex::new(watch_tx.clone());
        Arc::new(move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(WatchSignal::Reconcile);
            }
        })
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
        reconcile,
    )
    .context("canonicalizing the data dir / library root")?;

    // First-run UX (issue 159): do not hold the HTTP port down behind the
    // initial reconcile. A large first scan takes minutes to split, and
    // anything in front of the server (a reverse proxy, a Funnel) returns 502
    // the whole time. Instead, mark the state "scanning" and hand the initial
    // reconcile to the watcher's background thread. That thread registers the
    // filesystem watch first, runs the reconcile, and then flips the state to
    // "ready" via the callback. Until then, `GET /` shows a "Scanning…" page,
    // and the capability routes answer 503 + Retry-After, not a 502/404. A
    // warm restart reaches ready almost immediately: the reconcile's
    // idempotency early-returns.
    //
    // Watch-before-scan (not scan-before-watch): the watcher buffers and
    // reconciles a library change that lands during the initial scan; the
    // change is not lost until the next event or a restart. The watcher owns
    // the only index writer, so exactly one reconciler ever runs.
    state.set_ready(false);
    {
        let state = state.clone();
        spawn_library_watcher(
            config.library.clone(),
            config.data_dir.clone(),
            db_path,
            scan_opts,
            watch_tx,
            watch_rx,
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

/// Choose the tracing filter: `RUST_LOG` when it is set and valid, otherwise
/// the configured level, otherwise `info`. Each fallback also returns a warning
/// line, so a typo degrades logging instead of passing unnoticed or aborting.
///
/// `RUST_LOG` stays the standard per-module escape hatch and wins whenever it
/// parses. When it is unset the configured `log_level` applies. When it is set
/// but unparsable, the configured level applies and a warning names the bad
/// `RUST_LOG`. An unparsable configured level falls back to `info` with a
/// warning of its own.
fn resolve_log_filter(rust_log: Option<&str>, configured: &str) -> (EnvFilter, Vec<String>) {
    let mut warnings = Vec::new();

    // RUST_LOG wins whenever it parses.
    if let Some(directives) = rust_log {
        match EnvFilter::try_new(directives) {
            Ok(filter) => return (filter, warnings),
            Err(err) => warnings.push(format!(
                "invalid RUST_LOG {directives:?}: {err}; using the configured log level"
            )),
        }
    }

    match build_configured_filter(configured) {
        Ok(filter) => (filter, warnings),
        Err(err) => {
            warnings.push(format!(
                "invalid log level {configured:?}: {err}; falling back to \"info\""
            ));
            (EnvFilter::new("info"), warnings)
        }
    }
}

/// Build a filter from the configured `log_level`, rejecting a bare word that is
/// not a level name.
///
/// A directive with no `=` must name a level (`info`, `debug`, `off`, ...).
/// `EnvFilter` otherwise reads an unknown bare word as a *target* name at trace
/// level, so `--log-level warning` would enable only a phantom `warning` target
/// and silence every real log line with no error. Validate the bare form as a
/// level first; leave the `target=level` form (and multi-directive strings) to
/// `EnvFilter` itself, which already validates the level after each `=`.
fn build_configured_filter(configured: &str) -> Result<EnvFilter, String> {
    if !configured.contains('=') {
        LevelFilter::from_str(configured).map_err(|err| err.to_string())?;
    }
    EnvFilter::try_new(configured).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_level_is_applied_when_rust_log_unset() {
        let (filter, warnings) = resolve_log_filter(None, "debug");
        assert!(warnings.is_empty());
        assert_eq!(filter.to_string(), "debug");
    }

    #[test]
    fn rust_log_wins_over_the_configured_level() {
        let (filter, warnings) = resolve_log_filter(Some("trace"), "info");
        assert!(warnings.is_empty());
        assert_eq!(filter.to_string(), "trace");
    }

    #[test]
    fn a_bare_misspelled_level_warns_and_falls_back_to_info() {
        // "warning" is not a level name. EnvFilter would read it as a target and
        // go silent, so this must warn and fall back to info (finding 1).
        let (filter, warnings) = resolve_log_filter(None, "warning");
        assert_eq!(filter.to_string(), "info");
        assert!(warnings.iter().any(|w| w.contains("warning")));
    }

    #[test]
    fn an_invalid_per_module_level_warns_and_falls_back_to_info() {
        let (filter, warnings) = resolve_log_filter(None, "podspine=chatty");
        assert_eq!(filter.to_string(), "info");
        assert!(!warnings.is_empty());
    }

    #[test]
    fn an_invalid_rust_log_warns_and_uses_the_configured_level() {
        // A bad RUST_LOG must warn, then the configured level applies (finding 2).
        let (filter, warnings) = resolve_log_filter(Some("=bogus"), "debug");
        assert_eq!(filter.to_string(), "debug");
        assert!(warnings.iter().any(|w| w.contains("RUST_LOG")));
    }
}
