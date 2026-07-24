//! `metrics` — optional Prometheus instrumentation (TAD §6.5).
//!
//! Off unless the operator passes `--metrics-bind`. The rest of the workspace
//! calls the record helpers below unconditionally; the `metrics` facade is a
//! no-op until a recorder is installed, so a default deployment pays nothing
//! beyond an atomic load per call.
//!
//! The endpoint is a **standalone listener**, never a route on the feed server.
//! Feeds are capability URLs meant to be safe to expose publicly ([TAD §5.3]);
//! metrics are operator data — how many books exist, how often things fail —
//! and putting them on that same surface would hand an anonymous caller a
//! library-size oracle. Bind this to loopback or the LAN.
//!
//! Label cardinality is deliberately bounded: the only label anywhere is
//! [`ErrorKind`], a closed set of three static strings. Book titles, slugs,
//! capability ids, and filesystem paths are never emitted — they would both
//! explode series count and leak exactly what the private-feed design protects.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};

/// Books currently present in the index (gauge).
pub const BOOKS_INDEXED: &str = "podspine_books_indexed";
/// Feeds successfully rendered and served (counter).
pub const FEEDS_SERVED: &str = "podspine_feeds_served_total";
/// Wall-clock seconds spent splitting one chapter (histogram).
pub const SPLIT_DURATION: &str = "podspine_split_duration_seconds";
/// Request failures, labelled by [`ErrorKind`] (counter).
pub const ERRORS: &str = "podspine_errors_total";

/// Bucket bounds (seconds) for [`SPLIT_DURATION`]. Chosen around the measured
/// ingest profile — a stream-copy chapter split lands in the tens of
/// milliseconds to a few seconds — with a long tail up to two minutes so a
/// pathological source is visible rather than lumped into `+Inf`.
const SPLIT_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// How often histogram data is drained. Without this the recorder's histograms
/// grow unboundedly — with the crate's own HTTP listener disabled, running
/// upkeep is our job.
const UPKEEP_INTERVAL: Duration = Duration::from_secs(30);

/// What failed, as a bounded label set. Kept to a closed enum of static strings
/// so series count can't grow with traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Unknown book, episode, or capability id (also every rejected slug).
    NotFound,
    /// A resolved path escaped its trusted root, or an origin check failed.
    Forbidden,
    /// Anything we consider our own fault: I/O, ffmpeg, index, render.
    Internal,
}

impl ErrorKind {
    /// The `kind` label value.
    fn as_label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Forbidden => "forbidden",
            Self::Internal => "internal",
        }
    }
}

/// Build the Prometheus recorder and install it globally.
///
/// Call once, before serving. Until this runs every helper below is a no-op.
///
/// # Errors
/// Returns [`BuildError`] if the recorder can't be built, or if a global
/// recorder was already installed.
pub fn install() -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Full(SPLIT_DURATION.to_owned()), SPLIT_BUCKETS)?
        .install_recorder()?;

    metrics::describe_gauge!(BOOKS_INDEXED, "Books currently present in the index");
    metrics::describe_counter!(FEEDS_SERVED, "Feeds rendered and served to subscribers");
    metrics::describe_histogram!(SPLIT_DURATION, "Seconds spent splitting one chapter");
    metrics::describe_counter!(ERRORS, "Request failures by kind");

    // Touch every counter/gauge at zero. The exporter only renders a series once
    // it has been recorded, so without this a freshly started server exposes
    // nothing and dashboards read "no data" — indistinguishable from a scrape
    // failure — until the first feed request happens to arrive. The histogram is
    // deliberately left out: there is no way to register it without recording an
    // observation, and a fake 0s split would skew the very distribution it exists
    // to measure.
    metrics::gauge!(BOOKS_INDEXED).set(0.0);
    metrics::counter!(FEEDS_SERVED).increment(0);
    for kind in [
        ErrorKind::NotFound,
        ErrorKind::Forbidden,
        ErrorKind::Internal,
    ] {
        metrics::counter!(ERRORS, "kind" => kind.as_label()).increment(0);
    }

    Ok(handle)
}

/// Set the number of books in the index. Called after each reconcile rather
/// than incremented per book, so a prune is reflected as accurately as an add.
pub fn set_books_indexed(count: u64) {
    metrics::gauge!(BOOKS_INDEXED).set(count as f64);
}

/// Count one successfully served feed.
pub fn feed_served() {
    metrics::counter!(FEEDS_SERVED).increment(1);
}

/// Record one chapter split's wall-clock duration.
pub fn split_observed(elapsed: Duration) {
    metrics::histogram!(SPLIT_DURATION).record(elapsed.as_secs_f64());
}

/// Count one request failure.
pub fn error(kind: ErrorKind) {
    metrics::counter!(ERRORS, "kind" => kind.as_label()).increment(1);
}

/// Build the metrics router: `GET /metrics` and nothing else.
pub fn router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(render))
        .with_state(handle)
}

/// Render the exposition payload.
async fn render(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        handle.render(),
    )
}

/// Serve the metrics endpoint on an already-bound listener until shutdown,
/// draining histograms on an interval alongside it.
///
/// Takes a bound listener rather than an address on purpose: this runs as a
/// detached background task, so a bind failure here would only reach a log
/// line. The caller binds first and treats that failure as fatal — if the
/// operator asked for metrics and the port is taken, they should hear about it
/// at startup, not discover a silently missing endpoint at scrape time.
///
/// # Errors
/// Returns the serve error if the listener stops accepting.
pub async fn serve(
    listener: tokio::net::TcpListener,
    handle: PrometheusHandle,
) -> std::io::Result<()> {
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPKEEP_INTERVAL);
        loop {
            ticker.tick().await;
            upkeep.run_upkeep();
        }
    });

    axum::serve(listener, router(handle)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// One recorder per process — `install` on a second test would fail with
    /// `FailedToSetGlobalRecorder`, so the tests that need real output share
    /// this one handle.
    ///
    /// Every test must call this *before* it records anything: the facade is a
    /// no-op until the global recorder exists, so a call made first would be
    /// silently dropped and the scrape would come back empty.
    fn handle() -> PrometheusHandle {
        use std::sync::OnceLock;
        static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
        HANDLE
            .get_or_init(|| install().expect("recorder installs"))
            .clone()
    }

    async fn scrape() -> (StatusCode, String, String) {
        let response = router(handle())
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    #[tokio::test]
    async fn scrape_returns_prometheus_text() {
        let _installed = handle();
        feed_served();
        let (status, content_type, body) = scrape().await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type.starts_with("text/plain"),
            "content type was {content_type:?}"
        );
        assert!(
            body.contains(FEEDS_SERVED),
            "counter missing from exposition: {body}"
        );
    }

    #[tokio::test]
    async fn gauge_reflects_the_last_value_set() {
        let _installed = handle();
        set_books_indexed(7);
        set_books_indexed(3);
        let (_, _, body) = scrape().await;
        assert!(
            body.contains(&format!("{BOOKS_INDEXED} 3")),
            "gauge should hold the latest value, got: {body}"
        );
    }

    #[tokio::test]
    async fn split_histogram_uses_configured_buckets() {
        let _installed = handle();
        split_observed(Duration::from_millis(120));
        let (_, _, body) = scrape().await;
        assert!(
            body.contains(&format!("{SPLIT_DURATION}_bucket")),
            "expected a true histogram (buckets), not a summary: {body}"
        );
        assert!(
            body.contains("le=\"0.25\""),
            "expected our configured bucket bounds: {body}"
        );
    }

    #[tokio::test]
    async fn errors_are_labelled_by_kind() {
        let _installed = handle();
        error(ErrorKind::NotFound);
        error(ErrorKind::Forbidden);
        let (_, _, body) = scrape().await;
        assert!(body.contains("kind=\"not_found\""), "{body}");
        assert!(body.contains("kind=\"forbidden\""), "{body}");
    }

    #[tokio::test]
    async fn every_counter_series_exists_before_its_first_event() {
        // A fresh server must expose zeros, not an empty page: "no data" on a
        // dashboard should mean the scrape failed, not that nothing happened yet.
        let _installed = handle();
        let (_, _, body) = scrape().await;
        for expected in [FEEDS_SERVED, BOOKS_INDEXED] {
            assert!(body.contains(expected), "{expected} missing from: {body}");
        }
        for kind in ["not_found", "forbidden", "internal"] {
            assert!(
                body.contains(&format!("kind=\"{kind}\"")),
                "error series for {kind} should be pre-registered: {body}"
            );
        }
    }

    #[tokio::test]
    async fn metrics_carry_help_text() {
        let _installed = handle();
        let (_, _, body) = scrape().await;
        assert!(
            body.contains(&format!("# HELP {BOOKS_INDEXED}")),
            "expected HELP lines: {body}"
        );
    }

    #[test]
    fn error_labels_are_stable_snake_case() {
        // These strings are a query contract — dashboards and alert rules bind
        // to them, so a rename is a breaking change, not a refactor.
        assert_eq!(ErrorKind::NotFound.as_label(), "not_found");
        assert_eq!(ErrorKind::Forbidden.as_label(), "forbidden");
        assert_eq!(ErrorKind::Internal.as_label(), "internal");
    }

    #[test]
    fn split_buckets_are_ascending_and_positive() {
        assert!(SPLIT_BUCKETS.windows(2).all(|w| w[0] < w[1]));
        assert!(SPLIT_BUCKETS.iter().all(|b| *b > 0.0));
    }
}
