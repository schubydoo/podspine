//! `http` — the Axum HTTP surface.
//!
//! Routes:
//! - `GET /healthz` — liveness.
//! - `GET /feed/{slug}.xml` — the podcast feed, built from the index and passed
//!   through the feed self-check before serving.
//! - `GET /audio/{slug}/{number}` — an episode file with HTTP Range support
//!   (206 / `Content-Range` / 416) via `axum-range`.
//!
//! Book/episode keys are resolved server-side through the index; the file path
//! served comes from the database (written at scan time), never built from user
//! input. As defense-in-depth the resolved path is canonicalized and asserted to
//! live under the data dir. Errors never leak filesystem paths (that detail is
//! logged, not returned). See TAD §4/§7. Concurrency limits + full traversal
//! hardening are Task 3.5.

use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::TypedHeader;
use axum_extra::headers::Range;
use axum_range::{KnownSize, Ranged};
use tokio::fs::File;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use podspine_feed::{FeedBook, FeedEpisode, render_checked};
use podspine_index::Index;

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    /// The index (SQLite `Connection` is not `Sync`, so it lives behind a mutex;
    /// handlers never hold the lock across an `.await`).
    pub index: Arc<Mutex<Index>>,
    /// External base URL for feed/enclosure links (no trailing slash).
    pub base_url: String,
    /// Canonical data dir — resolved audio paths must stay under it.
    pub data_dir: PathBuf,
}

impl AppState {
    /// Build state, canonicalizing the data dir for the path-safety check.
    pub fn new(index: Index, base_url: String, data_dir: &FsPath) -> Self {
        let data_dir = data_dir
            .canonicalize()
            .unwrap_or_else(|_| data_dir.to_path_buf());
        Self {
            index: Arc::new(Mutex::new(index)),
            base_url,
            data_dir,
        }
    }
}

/// Build the router with all routes and middleware layers.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/feed/{slug}", get(feed))
        .route("/audio/{slug}/{number}", get(audio))
        .layer(TraceLayer::new_for_http())
        // Bounds only response *production* (not the streamed body), so large
        // audio downloads aren't truncated.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        // We accept no request bodies; keep them tiny.
        .layer(RequestBodyLimitLayer::new(16 * 1024))
        .with_state(state)
}

/// Bind and serve until shutdown.
pub async fn serve(bind: SocketAddr, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "podspine listening");
    axum::serve(listener, router(state)).await
}

async fn healthz() -> &'static str {
    "ok"
}

/// `GET /feed/{slug}.xml` — the route captures `{slug}` including the `.xml`
/// suffix, which we strip before lookup.
async fn feed(
    State(state): State<AppState>,
    Path(slug_xml): Path<String>,
) -> Result<Response, AppError> {
    let slug = slug_xml.strip_suffix(".xml").ok_or(AppError::NotFound)?;
    let xml = build_feed_xml(&state, slug)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

/// `GET /audio/{slug}/{number}` — stream an episode with Range support.
async fn audio(
    State(state): State<AppState>,
    Path((slug, number)): Path<(String, u32)>,
    range: Option<TypedHeader<Range>>,
) -> Result<Ranged<KnownSize<File>>, AppError> {
    let path = resolve_audio_path(&state, &slug, number)?;
    let file = File::open(&path).await.map_err(|_| AppError::NotFound)?;
    let body = KnownSize::file(file).await.map_err(AppError::internal)?;
    let range = range.map(|TypedHeader(range)| range);
    Ok(Ranged::new(range, body))
}

/// Build and self-check the feed XML for a slug.
fn build_feed_xml(state: &AppState, slug: &str) -> Result<String, AppError> {
    let (book, episodes) = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_slug(slug)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        let episodes = index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?;
        (book, episodes)
    };

    let base = &state.base_url;
    let feed_book = FeedBook {
        id: book.id,
        title: book.title,
        author: book.author,
        description: None,
        cover_url: None, // cover serving lands in Task 3.4
        source_mtime: book.source_mtime,
        self_url: format!("{base}/feed/{slug}.xml"),
        episodes: episodes
            .iter()
            .map(|e| FeedEpisode {
                idx: e.idx as usize,
                title: e.title.clone(),
                audio_url: format!("{base}/audio/{slug}/{}", e.idx + 1),
                byte_length: e.byte_length as u64,
                duration_sec: e.duration_sec,
                mime_type: mime_for(&e.file_path).to_string(),
            })
            .collect(),
    };

    render_checked(&feed_book).map_err(|errs| {
        tracing::error!(?errs, slug, "feed failed self-check");
        AppError::Internal
    })
}

/// Resolve `(slug, episode number)` to a validated on-disk path.
fn resolve_audio_path(state: &AppState, slug: &str, number: u32) -> Result<PathBuf, AppError> {
    let idx = number.checked_sub(1).ok_or(AppError::NotFound)? as i64;

    let file_path = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_slug(slug)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?
            .into_iter()
            .find(|e| e.idx == idx)
            .ok_or(AppError::NotFound)?
            .file_path
    };

    // Defense-in-depth: the path came from the DB, but confirm it resolves under
    // the data dir before opening it.
    let canonical = PathBuf::from(&file_path)
        .canonicalize()
        .map_err(|_| AppError::NotFound)?;
    if !canonical.starts_with(&state.data_dir) {
        tracing::warn!(slug, number, "resolved audio path escaped the data dir");
        return Err(AppError::NotFound);
    }
    Ok(canonical)
}

/// Hardcoded MIME by extension (no content sniffing).
fn mime_for(path: &str) -> &'static str {
    match FsPath::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        _ => "audio/mp4", // .m4a/.m4b and default
    }
}

/// Handler error — maps to a status code and never leaks internals.
#[derive(Debug)]
enum AppError {
    NotFound,
    Internal,
}

impl AppError {
    /// Collapse any error into `Internal` (the detail is logged elsewhere).
    fn internal<E>(_e: E) -> Self {
        AppError::Internal
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND.into_response(),
            AppError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_for("/x/001.m4a"), "audio/mp4");
        assert_eq!(mime_for("/x/001.MP3"), "audio/mpeg");
        assert_eq!(mime_for("/x/blob"), "audio/mp4");
    }
}
