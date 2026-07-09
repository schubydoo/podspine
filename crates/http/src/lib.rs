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
//! canonicalized and asserted to live under a trusted root — the data dir, or the
//! library root for whole-file episodes served in place (Sprint 6.2); a
//! `ConcurrencyLimitLayer` bounds in-flight requests alongside the timeout and
//! body-limit layers. Errors never leak filesystem paths or ffmpeg stderr (that
//! detail is logged, collapsed to a bare status for the client). See TAD §4/§7.

use std::collections::{HashMap, HashSet};
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
use podspine_splitter::{ChapterCut, remux_faststart, split_chapter};
use podspine_ui::{BookCard, BookDetail, book_page, index_page, subscribe_page};

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
    /// Canonical data dir — extracted (chaptered) audio must stay under it.
    pub data_dir: PathBuf,
    /// Canonical library root — whole-file episodes are streamed in place from
    /// here (Sprint 6.2), so a resolved in-place path must stay under it. This is
    /// the read-only source tree; the audio handler never writes to it.
    pub library_dir: PathBuf,
    /// Feed-level fallback cover URL for books with no embedded art.
    pub default_cover_url: Option<String>,
    /// `saver` storage mode: episode files aren't pre-split — the audio handler
    /// regenerates a chapter on demand and caches it. `false` = pre-split.
    pub saver: bool,
    /// `saver`-mode cache cap in bytes (`None` = unbounded).
    pub cache_size_bytes: Option<u64>,
    /// `saver`-mode cache TTL (`None` = size-only eviction).
    pub cache_ttl: Option<Duration>,
    /// Per-chapter regeneration locks (single-flight): concurrent requests for
    /// the same uncached chapter run ffmpeg once, not N times.
    inflight: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl AppState {
    /// Build state, canonicalizing the data dir **and** the library root for the
    /// path-safety checks (served files must stay under one of them). The
    /// `saver`/cache args come from [`podspine_config::Config`] (pre-split
    /// defaults: `saver=false`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: Index,
        base_url: String,
        data_dir: &FsPath,
        library_dir: &FsPath,
        default_cover_url: Option<String>,
        saver: bool,
        cache_size_bytes: Option<u64>,
        cache_ttl: Option<Duration>,
    ) -> Self {
        let data_dir = data_dir
            .canonicalize()
            .unwrap_or_else(|_| data_dir.to_path_buf());
        let library_dir = library_dir
            .canonicalize()
            .unwrap_or_else(|_| library_dir.to_path_buf());
        Self {
            index: Arc::new(Mutex::new(index)),
            base_url,
            data_dir,
            library_dir,
            default_cover_url,
            saver,
            cache_size_bytes,
            cache_ttl,
            inflight: Arc::new(Mutex::new(HashMap::new())),
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
        .route("/subscribe/{feed_id}", get(subscribe))
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
        subscribe_url: format!("{}/subscribe/{}", state.base_url, book.feed_id),
        slug: book.slug,
        feed_id: book.feed_id,
        title: book.title,
        author: book.author,
        has_cover: book.cover_path.is_some(),
        episode_count,
    };
    Ok(Html(book_page(&detail).into_string()))
}

/// `GET /subscribe/{feed_id}` — the "add to a podcast app" helper page (per-app
/// deep links + QRs). Keyed by capability id: this is what the book-page QR points
/// at, so an iOS Camera scan lands on real "Open in…" app links, not raw feed XML.
async fn subscribe(
    State(state): State<AppState>,
    Path(feed_id): Path<String>,
) -> Result<Html<String>, AppError> {
    if !valid_feed_id(&feed_id) {
        return Err(AppError::NotFound);
    }
    let (book, episode_count) = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_feed_id(&feed_id)
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
        subscribe_url: format!("{}/subscribe/{}", state.base_url, book.feed_id),
        slug: book.slug,
        feed_id: book.feed_id,
        title: book.title,
        author: book.author,
        has_cover: book.cover_path.is_some(),
        episode_count,
    };
    Ok(Html(subscribe_page(&detail).into_string()))
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
    let target = resolve_audio_target(&state, &feed_id, number)?;
    // A missing file is regenerated on demand when the resolver supplied a `Regen`
    // (a `saver` chapter split, or a faststart remux of a whole-file episode);
    // otherwise (e.g. `full`-mode chapter, in-place whole file) it's a genuine 404.
    if !target.path.exists() {
        match &target.regen {
            Some(regen) => ensure_cached(&state, &target.path, regen).await?,
            None => return Err(AppError::NotFound),
        }
    }
    // Final defense-in-depth: the file now exists, so canonicalize it (resolving
    // any symlink) and confirm it still lives under a trusted root before opening
    // — the data dir (extracted chapters) OR the library root (whole-file
    // episodes served in place). The resolver already checked the relevant root;
    // this additionally catches a file that is itself a symlink pointing outside.
    let path = target.path.canonicalize().map_err(|_| AppError::NotFound)?;
    if !path.starts_with(&state.data_dir) && !path.starts_with(&state.library_dir) {
        tracing::warn!(
            feed_id,
            number,
            "resolved audio file escaped its trusted root"
        );
        return Err(AppError::NotFound);
    }
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
/// What `/audio` needs: the canonical target file (which may not exist yet in
/// `saver` mode) and, in `saver` mode, everything to regenerate it on demand.
struct AudioTarget {
    path: PathBuf,
    regen: Option<Regen>,
}

/// Inputs to regenerate one cache file on demand — a `saver` chapter split, or a
/// faststart remux of a whole-file episode (Sprint 6.3). The source is always a
/// validated file under a trusted root; the op decides which ffmpeg call rebuilds
/// the deterministic output.
struct Regen {
    source: PathBuf,
    out_dir: PathBuf,
    out_ext: String,
    op: RegenOp,
}

/// Which ffmpeg operation regenerates the cache file.
#[derive(Clone)]
enum RegenOp {
    /// Re-split one chapter (`saver` mode) — a sub-range of the container.
    Chapter(ChapterCut),
    /// Remux a whole file to faststart (`PODSPINE_REMUX_NON_FASTSTART`).
    Faststart { idx: usize, duration_sec: f64 },
}

/// Resolve `/audio/{feed_id}/{number}` to its on-disk target. Three path-safe
/// shapes, by whether the episode is a whole file (and how it's stored):
///
/// - **In place (whole-file episode, `file_path == source_path`):** a whole file
///   streamed from the library — canonicalized, asserted under the library root,
///   and size-checked against the recorded length (Sprint 6.2).
/// - **Faststart remux (whole-file episode, `file_path != source_path`):** the
///   served file is a cache copy under the data dir; it's regenerated on demand by
///   remuxing the library source to faststart (Sprint 6.3), so the resolver returns
///   a [`Regen`] carrying [`RegenOp::Faststart`].
/// - **Extracted (chaptered episode):** the path is reconstructed from the
///   canonical `data_dir` plus **opaque DB keys** (`book.id`, chapter index) and a
///   validated audio extension — never built from request input — so it stays
///   under the data dir by construction (no traversal). In `saver` mode it's
///   regenerated on demand ([`RegenOp::Chapter`]); existence is not required.
fn resolve_audio_target(
    state: &AppState,
    feed_id: &str,
    number: u32,
) -> Result<AudioTarget, AppError> {
    if !valid_feed_id(feed_id) {
        return Err(AppError::NotFound);
    }
    let idx = number.checked_sub(1).ok_or(AppError::NotFound)? as i64;

    let (book, ep) = {
        let index = state.index.lock().map_err(AppError::internal)?;
        let book = index
            .get_book_by_feed_id(feed_id)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        let ep = index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?
            .into_iter()
            .find(|e| e.idx == idx)
            .ok_or(AppError::NotFound)?;
        (book, ep)
    };

    // Serve-in-place (Sprint 6.2): a whole-file episode streamed directly from the
    // read-only library — never copied under the data dir. Recognized by a
    // non-empty `source_path` whose value equals `file_path` (a whole-file episode
    // whose `file_path != source_path` was remuxed to a faststart cache copy and
    // is handled by the data-dir path below). Three guards, all 404 on failure:
    //   1. canonicalize + assert under the library root (reject `..`/symlink
    //      escape) — the A01 "assert under the library root" rule (TAD §7);
    //   2. the recorded enclosure length must equal the on-disk source size —
    //      the WHOLE-FILE invariant. A chaptered episode (a sub-range) that
    //      wrongly carries a `source_path` from a bad migration / partial rescan
    //      / manual edit has a chapter-sized `byte_length` ≠ the container size,
    //      so it's rejected here instead of serving the full container's bytes
    //      under the chapter's enclosure length.
    // Returns before the data-dir/regeneration logic below, so a poisoned row can
    // never fall through into it.
    if !ep.source_path.is_empty() && ep.file_path == ep.source_path {
        let src = FsPath::new(&ep.source_path)
            .canonicalize()
            .map_err(|_| AppError::NotFound)?;
        if !src.starts_with(&state.library_dir) {
            tracing::warn!(feed_id, number, "in-place audio escaped the library root");
            return Err(AppError::NotFound);
        }
        let src_len = std::fs::metadata(&src)
            .map(|m| m.len() as i64)
            .map_err(|_| AppError::NotFound)?;
        if src_len != ep.byte_length {
            tracing::warn!(
                feed_id,
                number,
                "in-place source size != recorded enclosure length; refusing to serve (corrupt row?)"
            );
            return Err(AppError::NotFound);
        }
        return Ok(AudioTarget {
            path: src,
            regen: None,
        });
    }

    // The container extension is the audio ext the scanner recorded; reject
    // anything non-alphanumeric so it can never introduce a path separator.
    let out_ext = FsPath::new(&ep.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .ok_or(AppError::NotFound)?;
    // Defense-in-depth, at runtime (not a debug_assert that vanishes in release):
    // resolve the book dir and confirm it stays under the canonical data dir
    // before anything is opened or written. The components are opaque DB keys,
    // but a poisoned row must never let a path escape. The chapter file itself
    // may not exist yet (saver), so canonicalize the parent dir; a book dir that
    // doesn't exist (never ingested) is a clean 404.
    let out_dir = state
        .data_dir
        .join("books")
        .join(&book.id)
        .canonicalize()
        .map_err(|_| AppError::NotFound)?;
    if !out_dir.starts_with(&state.data_dir) {
        tracing::warn!(feed_id, number, "resolved audio path escaped the data dir");
        return Err(AppError::NotFound);
    }
    let path = out_dir.join(format!("{:03}.{out_ext}", idx + 1));

    // Two kinds of episode materialize under the data dir here:
    let regen = if !ep.source_path.is_empty() && ep.needs_faststart {
        // A non-faststart whole-file episode remuxed to a faststart cache copy
        // (Sprint 6.3, `file_path != source_path`). Regenerate it on demand from
        // the library source — always, independent of storage_mode. The
        // `needs_faststart` gate means a chaptered row that merely carries a stray
        // `source_path` is NOT remuxed into its container here; it drops to the
        // chaptered arm and serves its actual split (or 404s). Validate the source
        // stays under the library root first (the A01 rule), 404 on escape.
        let src = FsPath::new(&ep.source_path)
            .canonicalize()
            .map_err(|_| AppError::NotFound)?;
        if !src.starts_with(&state.library_dir) {
            tracing::warn!(feed_id, number, "remux source escaped the library root");
            return Err(AppError::NotFound);
        }
        Some(Regen {
            source: src,
            out_dir,
            out_ext,
            op: RegenOp::Faststart {
                idx: idx as usize,
                duration_sec: ep.duration_sec,
            },
        })
    } else {
        // A chaptered episode. Regen is possible only in `saver` mode when the
        // book's source is a single file to re-split; the `is_file` guard is
        // belt-and-suspenders (a directory source would make `ffmpeg <directory>`
        // fail), so a missing file is a clean 404, not a 500.
        (state.saver && FsPath::new(&book.source_path).is_file()).then(|| Regen {
            source: PathBuf::from(&book.source_path),
            out_dir,
            out_ext,
            // `end_sec` is reconstructed as start + duration. This is EXACT, not an
            // approximation: the scanner stores `duration_sec = cut.end - cut.start`
            // (the requested cut length, not a measured output duration), so this
            // yields the same `[start, end)` the ingest split used — and ffmpeg's
            // 6-decimal arg formatting absorbs any float round-trip. The stream
            // copy is therefore byte-identical (asserted in the serve test).
            op: RegenOp::Chapter(ChapterCut {
                idx: idx as usize,
                start_sec: ep.start_sec,
                end_sec: ep.start_sec + ep.duration_sec,
            }),
        })
    };
    Ok(AudioTarget { path, regen })
}

/// Ensure `target` exists, regenerating it on demand: a `saver` chapter split, or
/// a whole-file faststart remux (Sprint 6.3). A per-path single-flight lock means
/// concurrent requests for the same uncached file run ffmpeg once; the blocking
/// ffmpeg work runs off the async runtime.
async fn ensure_cached(state: &AppState, target: &FsPath, regen: &Regen) -> Result<(), AppError> {
    let lock = {
        let mut map = state.inflight.lock().map_err(AppError::internal)?;
        map.entry(target.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let outcome = async {
        // A concurrent request may have produced it while we waited on the lock.
        if target.exists() {
            return Ok(());
        }
        let source = regen.source.clone();
        let out_dir = regen.out_dir.clone();
        let out_ext = regen.out_ext.clone();
        let op = regen.op.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&out_dir)?;
            match op {
                RegenOp::Chapter(cut) => {
                    split_chapter(&source, &out_dir, &cut, &out_ext)
                        .map_err(std::io::Error::other)?;
                }
                RegenOp::Faststart { idx, duration_sec } => {
                    remux_faststart(&source, &out_dir, idx, &out_ext, duration_sec)
                        .map_err(std::io::Error::other)?;
                }
            }
            Ok(())
        })
        .await
        .map_err(AppError::internal)?
        .map_err(AppError::internal)?;

        // Keep the cache under its cap/TTL, never evicting what we just produced.
        enforce_cache(state, target).await;
        Ok(())
    }
    .await;

    // Drop the single-flight entry so the map stays bounded to *in-flight*
    // regenerations, not every chapter ever served. Any waiter still blocked on
    // `.lock()` holds its own `Arc` clone of this same mutex, and every path
    // re-checks `target.exists()` after acquiring it, so removing the map entry
    // here can never cause a duplicate ffmpeg run.
    if let Ok(mut map) = state.inflight.lock() {
        map.remove(target);
    }
    outcome
}

/// Evict cached chapter files to keep the `saver` cache under its size cap and
/// TTL; `keep` (the file we just served) is never evicted. No-op when both
/// limits are unset. Best-effort — eviction never fails a request.
async fn enforce_cache(state: &AppState, keep: &FsPath) {
    let (cap, ttl) = (state.cache_size_bytes, state.cache_ttl);
    if cap.is_none() && ttl.is_none() {
        return; // unbounded + no TTL: nothing to evict
    }
    let books = state.data_dir.join("books");
    // Only single-file-source books are regenerable; their cached chapters are
    // safe to evict (they re-split on demand). A non-regenerable book dir must be
    // left alone — nothing would rebuild it: whole-file books are served in place
    // (any dir here is just a cover, or a legacy pre-6.2 copy not yet reclaimed).
    // So restrict eviction to the regenerable book dirs. Snapshot the sources
    // without holding the lock across the `is_file` stats.
    let sources: Vec<(String, String)> = {
        let Ok(index) = state.index.lock() else {
            return;
        };
        match index.list_books() {
            Ok(bs) => bs.into_iter().map(|b| (b.id, b.source_path)).collect(),
            Err(_) => return,
        }
    };
    let regenerable: HashSet<PathBuf> = sources
        .into_iter()
        .filter(|(_, src)| FsPath::new(src).is_file())
        .map(|(id, _)| books.join(id))
        .collect();
    let keep = keep.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || evict(&books, cap, ttl, &keep, &regenerable)).await;
}

/// Collect cached chapter files (numeric stems under `books/*/`) from
/// **regenerable** books only, drop TTL-expired ones, then delete oldest-first
/// until under `cap`. mtime is the LRU key: regenerating a chapter refreshes it.
/// Non-regenerable book dirs (a directory-source book, or a legacy pre-6.2 copy)
/// are skipped entirely so nothing that can't be rebuilt is destroyed. Best-effort;
/// per-file I/O errors are ignored.
fn evict(
    books_dir: &FsPath,
    cap: Option<u64>,
    ttl: Option<Duration>,
    keep: &FsPath,
    regenerable: &HashSet<PathBuf>,
) {
    let now = std::time::SystemTime::now();
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let Ok(book_dirs) = std::fs::read_dir(books_dir) else {
        return;
    };
    for book in book_dirs.flatten() {
        let book_path = book.path();
        // Never touch a non-regenerable book's files (MP3-folder tracks would be
        // lost until a rescan).
        if !regenerable.contains(&book_path) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&book_path) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            // Only chapter files (`001.m4a`-style, numeric stem): skips covers.
            let numeric = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
            if !numeric {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(now);
            if let Some(ttl) = ttl
                && p != keep
                && now.duration_since(mtime).is_ok_and(|age| age > ttl)
            {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            files.push((p, meta.len(), mtime));
        }
    }
    let Some(cap) = cap else { return };
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= cap {
        return;
    }
    files.sort_by_key(|(_, _, mtime)| *mtime); // oldest first
    for (p, len, _) in files {
        if total <= cap {
            break;
        }
        if p == keep {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(len);
        }
    }
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

    // ---- saver-mode cache eviction (unit-tested without ffmpeg) ----

    fn touch(path: &FsPath, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; bytes]).unwrap();
    }

    fn numeric_files(dir: &FsPath) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            })
            .count()
    }

    fn regen_set(dirs: &[&FsPath]) -> HashSet<PathBuf> {
        dirs.iter().map(|d| d.to_path_buf()).collect()
    }

    #[test]
    fn evict_enforces_size_cap_and_skips_non_chapter_files() {
        let dir = std::env::temp_dir().join("podspine-evict-size");
        let _ = std::fs::remove_dir_all(&dir);
        let books = dir.join("books");
        let bk = books.join("b1");
        for n in 1..=3 {
            touch(&bk.join(format!("{n:03}.m4a")), 100);
        }
        touch(&bk.join("cover.jpg"), 500); // non-numeric stem: never a cache file
        let keep = bk.join("003.m4a");

        // Cap 150B: with `keep` (100B) protected, older chapters are evicted.
        evict(&books, Some(150), None, &keep, &regen_set(&[&bk]));

        assert!(keep.exists(), "the just-served file is kept");
        assert!(
            bk.join("cover.jpg").exists(),
            "non-chapter files are untouched"
        );
        assert!(numeric_files(&bk) <= 1, "size cap evicted older chapters");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_drops_ttl_expired_chapters_except_keep() {
        let dir = std::env::temp_dir().join("podspine-evict-ttl");
        let _ = std::fs::remove_dir_all(&dir);
        let books = dir.join("books");
        let bk = books.join("b1");
        touch(&bk.join("001.m4a"), 100);
        touch(&bk.join("002.m4a"), 100);
        let keep = bk.join("002.m4a");
        // Ensure the files are measurably older than the (1ns) TTL.
        std::thread::sleep(Duration::from_millis(5));

        evict(
            &books,
            None,
            Some(Duration::from_nanos(1)),
            &keep,
            &regen_set(&[&bk]),
        );

        assert!(!bk.join("001.m4a").exists(), "TTL-expired chapter evicted");
        assert!(keep.exists(), "keep is never evicted, even past TTL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_is_a_noop_without_a_cap_or_ttl() {
        let dir = std::env::temp_dir().join("podspine-evict-noop");
        let _ = std::fs::remove_dir_all(&dir);
        let books = dir.join("books");
        let bk = books.join("b1");
        touch(&bk.join("001.m4a"), 100);
        let keep = bk.join("001.m4a");

        evict(&books, None, None, &keep, &regen_set(&[&bk])); // unbounded + no TTL

        assert!(keep.exists());
        assert_eq!(numeric_files(&bk), 1, "nothing evicted when unbounded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_tolerates_a_missing_books_dir() {
        // Top-level read_dir fails -> clean no-op, no panic.
        evict(
            FsPath::new("/no/such/podspine/books"),
            Some(1),
            None,
            FsPath::new("/no/such/keep"),
            &HashSet::new(),
        );
    }

    #[test]
    fn evict_never_touches_non_regenerable_books() {
        // A regenerable (single-file-source) book and a non-regenerable book dir
        // (a directory source — e.g. an MP3 folder, or a legacy pre-6.2 copy).
        // Only the regenerable one may be evicted; the non-regenerable files must
        // survive even a tiny cap (Greptile P1) — nothing would rebuild them.
        let dir = std::env::temp_dir().join("podspine-evict-mp3safe");
        let _ = std::fs::remove_dir_all(&dir);
        let books = dir.join("books");
        let split = books.join("splitbook"); // regenerable
        touch(&split.join("001.m4a"), 100);
        touch(&split.join("002.m4a"), 100);
        let folder = books.join("mp3book"); // NOT regenerable (directory source / legacy copy)
        touch(&folder.join("001.mp3"), 100);
        touch(&folder.join("002.mp3"), 100);
        let keep = split.join("002.m4a");

        // 1-byte cap: eviction is limited to the regenerable book dir.
        evict(&books, Some(1), None, &keep, &regen_set(&[&split]));

        assert!(
            folder.join("001.mp3").exists() && folder.join("002.mp3").exists(),
            "MP3-folder tracks are never evicted (they can't be regenerated)"
        );
        assert!(keep.exists());
        assert!(
            !split.join("001.m4a").exists(),
            "regenerable chapters are still evicted under the cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
