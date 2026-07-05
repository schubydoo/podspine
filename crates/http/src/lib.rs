//! `http` — the Axum HTTP surface.
//!
//! Routes split into two surfaces (v1.5):
//! - **Browse UI (keyed by human `slug`)** — meant for the LAN / behind
//!   proxy-auth; it enumerates the library, so it must not be publicly exposed:
//!   - `GET /` — the browsable book grid.
//!   - `GET /book/{slug}` — a book's page: copy-feed-URL, QR, how-to panel.
//! - **Public capability surface (keyed by unguessable `feed_id`)** — safe to
//!   expose externally; a guessed id reveals nothing (404):
//!   - `GET /feed/{feed_id}.xml` — the podcast feed (built from the index and
//!     passed through the feed self-check before serving).
//!   - `GET /audio/{feed_id}/{number}` — an episode file with HTTP Range support
//!     (206 / `Content-Range` / 416) via `axum-range`.
//!   - `GET /cover/{feed_id}` — the book's cover image.
//! - `GET /healthz` — liveness.
//!
//! Book/episode keys are resolved server-side through the index; the file path
//! served comes from the database (written at scan time), never built from user
//! input. Hardening (Task 3.5, TAD §7): the human slug is validated against an
//! allow-list charset ([`valid_slug`]) and the capability id against
//! [`valid_feed_id`], so `..`/separators/absolute markers 404 before touching
//! the DB or filesystem; as defense-in-depth the resolved audio path is still
//! canonicalized and asserted to live under the data dir; a
//! `ConcurrencyLimitLayer` bounds in-flight requests alongside the timeout and
//! body-limit layers. Errors never leak filesystem paths or ffmpeg stderr (that
//! detail is logged, collapsed to a bare status for the client). See TAD §4/§7.

use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::TypedHeader;
use axum_extra::headers::Range;
use axum_range::{KnownSize, Ranged};
use tokio::fs::File;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use podspine_feed::{FeedBook, FeedEpisode, render_checked};
use podspine_index::Index;
use podspine_ui::{BookCard, BookDetail, book_page, index_page};

/// Max concurrent in-flight requests before backpressure (DoS guard). Generous
/// for a homelab tool; only bounds a pathological flood.
const MAX_INFLIGHT_REQUESTS: usize = 512;

/// Whether a URL slug is safe to use as an opaque index key. Allow-list only:
/// non-empty and `[a-z0-9-]` — exactly what the scanner's `slugify` produces.
/// This rejects `..`, `/`, `\`, absolute markers, dots, and any other
/// separator-bearing or traversal input *before* it reaches the DB or the
/// filesystem. Callers 404 on rejection (no 403 oracle). Belt to the path
/// canonicalization suspenders in [`resolve_audio_path`]. See TAD §7 (A01).
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether a string is a syntactically valid capability `feed_id`: non-empty,
/// bounded length, and the URL-safe base64 alphabet (`[A-Za-z0-9_-]`) that
/// [`podspine_index::capability::generate`] produces. Same purpose as
/// [`valid_slug`] — reject traversal/separator input before the DB/filesystem —
/// but a wider charset because the id is random, not a lowercase slug. A bad id
/// 404s (no oracle); a well-formed but unknown id also 404s.
fn valid_feed_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Same-origin guard for the state-changing `POST` routes (CSRF defense). This
/// app uses **no cookies**, so `SameSite` is inapplicable; instead we check the
/// browser-set fetch-metadata / `Origin` headers. Modern browsers always send
/// `Sec-Fetch-Site`, so cross-site form posts are caught even in the proxy-auth
/// deployment (where a forged request would otherwise ride the owner's proxy
/// session). Non-browser clients (curl) send neither header and carry no ambient
/// auth to abuse, so they're allowed. Fails closed on a mismatched `Origin`.
fn same_origin(headers: &HeaderMap, base_url: &str) -> bool {
    if let Some(sfs) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return sfs == "same-origin" || sfs == "none";
    }
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        // Compare against base_url's scheme://authority (ignore any path suffix).
        Some(origin) => origin == base_url.split('/').take(3).collect::<Vec<_>>().join("/"),
        None => true,
    }
}

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
    /// Feed-level fallback cover URL for books with no embedded art.
    pub default_cover_url: Option<String>,
}

impl AppState {
    /// Build state, canonicalizing the data dir for the path-safety check.
    pub fn new(
        index: Index,
        base_url: String,
        data_dir: &FsPath,
        default_cover_url: Option<String>,
    ) -> Self {
        let data_dir = data_dir
            .canonicalize()
            .unwrap_or_else(|_| data_dir.to_path_buf());
        Self {
            index: Arc::new(Mutex::new(index)),
            base_url,
            data_dir,
            default_cover_url,
        }
    }
}

/// Build the router with all routes and middleware layers.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/book/{slug}", get(book))
        .route("/book/{slug}/regenerate", post(regenerate))
        .route("/cover/{feed_id}", get(cover))
        .route("/feed/{feed_id}", get(feed))
        .route("/audio/{feed_id}/{number}", get(audio))
        .layer(TraceLayer::new_for_http())
        // Bounds only response *production* (not the streamed body), so large
        // audio downloads aren't truncated.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        // Bound in-flight requests (DoS guard); excess requests wait rather than
        // exhaust resources (NFR-S3, TAD §7).
        .layer(ConcurrencyLimitLayer::new(MAX_INFLIGHT_REQUESTS))
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

/// `GET /feed/{feed_id}.xml` — the route captures `{feed_id}` including the
/// `.xml` suffix, which we strip before lookup. The capability URL is never
/// crawlable: an `X-Robots-Tag: noindex` keeps it out of web search engines
/// (the `itunes:block` in the XML separately keeps it out of podcast directories).
async fn feed(
    State(state): State<AppState>,
    Path(id_xml): Path<String>,
) -> Result<Response, AppError> {
    let feed_id = id_xml.strip_suffix(".xml").ok_or(AppError::NotFound)?;
    if !valid_feed_id(feed_id) {
        return Err(AppError::NotFound);
    }
    let xml = build_feed_xml(&state, feed_id)?;
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/rss+xml; charset=utf-8"),
            ("x-robots-tag", "noindex, nofollow"),
        ],
        xml,
    )
        .into_response())
}

/// `GET /` — the browsable book grid.
async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let books = {
        let index = state.index.lock().map_err(AppError::internal)?;
        index.list_books().map_err(AppError::internal)?
    };
    let cards: Vec<BookCard> = books
        .into_iter()
        .map(|b| BookCard {
            slug: b.slug,
            feed_id: b.feed_id,
            title: b.title,
            author: b.author,
            has_cover: b.cover_path.is_some(),
        })
        .collect();
    Ok(Html(index_page(&cards).into_string()))
}

/// `GET /book/{slug}` — a book's page: copy-feed-URL, QR, how-to panel.
async fn book(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Html<String>, AppError> {
    if !valid_slug(&slug) {
        return Err(AppError::NotFound);
    }
    let (book, episode_count) = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_slug(&slug)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        let count = index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?
            .len();
        (book, count)
    };

    let detail = BookDetail {
        feed_url: format!("{}/feed/{}.xml", state.base_url, book.feed_id),
        slug: book.slug,
        feed_id: book.feed_id,
        title: book.title,
        author: book.author,
        has_cover: book.cover_path.is_some(),
        episode_count,
    };
    Ok(Html(book_page(&detail).into_string()))
}

/// `POST /book/{slug}/regenerate` — rotate the book's capability `feed_id` (leak
/// recovery). The old feed/audio/cover URLs 404 immediately. Redirects back to
/// the book page (PRG) so a refresh doesn't re-submit.
async fn regenerate(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Redirect, AppError> {
    if !same_origin(&headers, &state.base_url) {
        return Err(AppError::Forbidden);
    }
    if !valid_slug(&slug) {
        return Err(AppError::NotFound);
    }
    {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_slug(&slug)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        index
            .regenerate_feed_id(&book.id)
            .map_err(AppError::internal)?;
    }
    Ok(Redirect::to(&format!("/book/{slug}")))
}

/// `GET /cover/{feed_id}` — the book's cover image, keyed by capability id so it
/// isn't a guessable catalog-probe surface. Covers are populated by cover
/// extraction (Task 3.4); until then books have no cover and this 404s. The
/// stored path is canonicalized and confirmed under the data dir before serving.
async fn cover(
    State(state): State<AppState>,
    Path(feed_id): Path<String>,
) -> Result<Response, AppError> {
    if !valid_feed_id(&feed_id) {
        return Err(AppError::NotFound);
    }
    let cover_path = {
        let index = state.index.lock().map_err(AppError::internal)?;
        index
            .get_book_by_feed_id(&feed_id)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?
            .cover_path
            .ok_or(AppError::NotFound)?
    };

    let canonical = PathBuf::from(&cover_path)
        .canonicalize()
        .map_err(|_| AppError::NotFound)?;
    if !canonical.starts_with(&state.data_dir) {
        tracing::warn!(feed_id, "resolved cover path escaped the data dir");
        return Err(AppError::NotFound);
    }
    let bytes = tokio::fs::read(&canonical)
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, image_mime(&cover_path))],
        bytes,
    )
        .into_response())
}

/// `GET /audio/{feed_id}/{number}` — stream an episode with Range support.
///
/// The `Content-Type` is set explicitly: `axum-range`'s `Ranged` emits
/// Content-Range/Accept-Ranges/Content-Length but NO Content-Type, and a missing
/// type makes strict clients (Apple Podcasts / iOS AVPlayer) refuse to play with
/// "can't be played on this device" — even though the enclosure carries `type=`.
async fn audio(
    State(state): State<AppState>,
    Path((feed_id, number)): Path<(String, u32)>,
    range: Option<TypedHeader<Range>>,
) -> Result<impl IntoResponse, AppError> {
    let path = resolve_audio_path(&state, &feed_id, number)?;
    let mime = mime_for(&path.to_string_lossy());
    let file = File::open(&path).await.map_err(|_| AppError::NotFound)?;
    let body = KnownSize::file(file).await.map_err(AppError::internal)?;
    let range = range.map(|TypedHeader(range)| range);
    // Header parts apply on top of Ranged's response, so the 206/Content-Range
    // and the 200 full-body case both keep their status and gain Content-Type.
    Ok(([(header::CONTENT_TYPE, mime)], Ranged::new(range, body)))
}

/// Build and self-check the feed XML for a capability `feed_id`. All public URLs
/// in the feed (self link, enclosures, cover) are built from `feed_id` so the
/// whole book is reachable from the one capability, and nothing guessable.
fn build_feed_xml(state: &AppState, feed_id: &str) -> Result<String, AppError> {
    let (book, episodes) = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_feed_id(feed_id)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        let episodes = index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?;
        (book, episodes)
    };

    let base = &state.base_url;
    // Per-book cover served at /cover/{feed_id} when extracted; otherwise the
    // configured feed-level fallback (or no image at all). See Task 3.4.
    let cover_url = book
        .cover_path
        .as_ref()
        .map(|_| format!("{base}/cover/{feed_id}"))
        .or_else(|| state.default_cover_url.clone());
    let feed_book = FeedBook {
        id: book.id,
        title: book.title,
        author: book.author,
        description: None,
        cover_url,
        source_mtime: book.source_mtime,
        self_url: format!("{base}/feed/{feed_id}.xml"),
        episodes: episodes
            .iter()
            .map(|e| FeedEpisode {
                idx: e.idx as usize,
                title: e.title.clone(),
                audio_url: format!("{base}/audio/{feed_id}/{}", e.idx + 1),
                byte_length: e.byte_length as u64,
                duration_sec: e.duration_sec,
                mime_type: mime_for(&e.file_path).to_string(),
            })
            .collect(),
    };

    render_checked(&feed_book).map_err(|errs| {
        tracing::error!(?errs, feed_id, "feed failed self-check");
        AppError::Internal
    })
}

/// Resolve `(feed_id, episode number)` to a validated on-disk path.
fn resolve_audio_path(state: &AppState, feed_id: &str, number: u32) -> Result<PathBuf, AppError> {
    if !valid_feed_id(feed_id) {
        return Err(AppError::NotFound);
    }
    let idx = number.checked_sub(1).ok_or(AppError::NotFound)? as i64;

    let file_path = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_feed_id(feed_id)
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
        tracing::warn!(feed_id, number, "resolved audio path escaped the data dir");
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
        Some("flac") => "audio/flac",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        _ => "audio/mp4", // .m4a/.m4b and default
    }
}

/// Hardcoded image MIME by extension for cover serving (no content sniffing).
fn image_mime(path: &str) -> &'static str {
    match FsPath::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg", // .jpg/.jpeg and default
    }
}

/// Handler error — maps to a status code and never leaks internals.
#[derive(Debug)]
enum AppError {
    NotFound,
    Forbidden,
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
            AppError::Forbidden => StatusCode::FORBIDDEN.into_response(),
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
        assert_eq!(mime_for("/x/001.flac"), "audio/flac");
        assert_eq!(mime_for("/x/001.ogg"), "audio/ogg");
        assert_eq!(mime_for("/x/001.opus"), "audio/ogg");
        assert_eq!(mime_for("/x/blob"), "audio/mp4");
    }

    #[test]
    fn valid_slug_allow_list() {
        // Accepts exactly what slugify produces.
        assert!(valid_slug("dune"));
        assert!(valid_slug("dune-2"));
        assert!(valid_slug("a1b2-c3"));
        // Rejects traversal / separators / absolute / case / dots / empty.
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "/abs",
            "C:",
            "Dune",
            "a.b",
            "a b",
            "a%2e",
            "café",
        ] {
            assert!(!valid_slug(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn valid_feed_id_allow_list() {
        // Accepts the URL-safe base64 alphabet capability::generate produces.
        assert!(valid_feed_id("Xk9mQ2vP7nR4tB1cY6wZ8a"));
        assert!(valid_feed_id("aA0-_zZ"));
        // Rejects traversal / separators / dots / empty / over-long.
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "a.b",
            "a b",
            "a%2e",
            "café",
            &"x".repeat(65),
        ] {
            assert!(!valid_feed_id(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn same_origin_guard() {
        let base = "http://host:8087";
        let with = |k: &'static str, v: &str| {
            let mut m = HeaderMap::new();
            m.insert(k, v.parse().unwrap());
            m
        };
        // Fetch metadata (all modern browsers): same-origin/none pass, cross-site fails.
        assert!(same_origin(&with("sec-fetch-site", "same-origin"), base));
        assert!(same_origin(&with("sec-fetch-site", "none"), base));
        assert!(!same_origin(&with("sec-fetch-site", "cross-site"), base));
        // Origin fallback: exact origin passes; a look-alike host fails (no prefix bug).
        assert!(same_origin(&with("origin", "http://host:8087"), base));
        assert!(!same_origin(
            &with("origin", "http://host:8087.evil.com"),
            base
        ));
        assert!(!same_origin(&with("origin", "http://evil.com"), base));
        // A base_url with a path suffix still compares by scheme://authority.
        assert!(same_origin(
            &with("origin", "http://host:8087"),
            "http://host:8087/sub"
        ));
        // Non-browser client (no headers) is allowed — no ambient auth to abuse.
        assert!(same_origin(&HeaderMap::new(), base));
    }

    #[test]
    fn image_mime_by_extension() {
        assert_eq!(image_mime("/x/cover.jpg"), "image/jpeg");
        assert_eq!(image_mime("/x/cover.JPEG"), "image/jpeg");
        assert_eq!(image_mime("/x/cover.png"), "image/png");
        assert_eq!(image_mime("/x/cover.webp"), "image/webp");
        assert_eq!(image_mime("/x/blob"), "image/jpeg");
    }
}
