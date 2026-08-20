//! `http` — the Axum HTTP surface.
//!
//! Routes split into two surfaces (v1.5):
//! - **Browse UI (keyed by human `slug`)**: meant for the LAN or behind
//!   proxy-auth. It enumerates the library, so it must not be publicly
//!   exposed:
//!   - `GET /` — the browsable book grid.
//!   - `GET /book/{slug}` — a book's page: copy-feed-URL, QR, how-to panel.
//! - **Public capability surface (keyed by unguessable `feed_id`)**: safe to
//!   expose externally. A guessed id reveals nothing (404):
//!   - `GET /feed/{feed_id}.xml` — the podcast feed. The handler builds it
//!     from the index and passes it through the feed self-check before it
//!     serves it.
//!   - `GET /audio/{feed_id}/{number}` — an episode file with HTTP Range
//!     support (206 / `Content-Range` / 416) via `axum-range`.
//!   - `GET /cover/{feed_id}` — the book's cover image.
//! - `GET /healthz` — liveness.
//!
//! The handlers resolve book/episode keys server-side through the index. The
//! served file path comes from the database (written at scan time); it is
//! never built from user input. Hardening (Task 3.5, TAD §7):
//! - The slug must match an allow-list charset ([`valid_slug`]), and the
//!   capability id must match [`valid_feed_id`]. So `..`, separators, and
//!   absolute markers 404 before they touch the DB or the filesystem.
//! - As defense in depth, the resolved audio path is still canonicalized and
//!   asserted to live under a trusted root: the data dir, or the library
//!   root for whole-file episodes served in place (Sprint 6.2).
//! - A `ConcurrencyLimitLayer` bounds in-flight requests, alongside the
//!   timeout and body-limit layers.
//! - Errors never leak filesystem paths or ffmpeg stderr. That detail is
//!   logged; the client gets a bare status.
//!
//! See TAD §4/§7.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
use podspine_index::{BookRow, Index, StorageMode, TranscodeMode};
use podspine_splitter::{
    ChapterCut, cover_thumb_path, episode_file_name, is_episode_stem, remux_faststart,
    split_chapter,
};
use podspine_ui::{
    BookCard, BookDetail, Theme, book_page, index_page, scanning_page, subscribe_page,
};

/// Max concurrent in-flight requests before backpressure (DoS guard). Generous
/// for a homelab tool; only bounds a pathological flood.
const MAX_INFLIGHT_REQUESTS: usize = 512;

/// Whether a URL slug is safe to use as an opaque index key. Allow-list only:
/// non-empty and `[a-z0-9-]`, exactly what the scanner's `slugify` produces.
/// This rejects `..`, `/`, `\`, absolute markers, dots, and any other
/// separator-bearing or traversal input *before* it reaches the DB or the
/// filesystem. Callers 404 on rejection (no 403 oracle). This check is the
/// belt; the path canonicalization in [`resolve_audio_target`] is the
/// suspenders. See TAD §7 (A01).
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether a string is a syntactically valid capability `feed_id`: non-empty,
/// bounded length, and the URL-safe base64 alphabet (`[A-Za-z0-9_-]`) that
/// [`podspine_index::capability::generate`] produces. The purpose is the same
/// as [`valid_slug`]: reject traversal/separator input before the
/// DB/filesystem. The charset is wider because the id is random, not a
/// lowercase slug. A bad id 404s (no oracle); a well-formed but unknown id
/// also 404s.
fn valid_feed_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Same-origin guard for the state-changing `POST` routes (CSRF defense). The
/// only cookie the app sets is the non-auth `theme` preference (there is no
/// session/auth cookie), so `SameSite` carries no ambient authority to
/// protect. Instead the guard checks the browser-set fetch-metadata and
/// `Origin` headers. Modern browsers always send `Sec-Fetch-Site`, so the
/// guard catches cross-site form posts even in the proxy-auth deployment
/// (there, a forged request would otherwise ride the owner's proxy session).
/// Non-browser clients (curl) send neither header and carry no ambient auth
/// to abuse, so they are allowed. The guard fails closed on a mismatched
/// `Origin`.
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

/// The visitor's colour [`Theme`] from the `theme` cookie. Absent or garbage
/// input gives [`Theme::System`], i.e. follow the OS. The value is parsed by
/// hand: the app pulls in no cookie crate for one first-party, non-auth
/// preference cookie.
fn theme_from_cookie(headers: &HeaderMap) -> Theme {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|kv| kv.trim().strip_prefix("theme="))
        })
        .map(Theme::parse)
        .unwrap_or_default()
}

/// The same-origin path to redirect back to after a `POST /theme` (PRG),
/// taken from the `Referer`. Return the path+query of an absolute
/// `scheme://host/path` URL, or a value that is already a plain absolute
/// path. Reject protocol-relative input (`//host`) and anything that does not
/// start with a single `/`, so the result can never become an open redirect.
/// Return `None` when there is no usable path (the caller falls back to `/`).
fn referer_path(referer: &str) -> Option<String> {
    // Strip `scheme://authority`, which leaves `/path?query`. If there is no
    // `://`, the header was already a bare path (defensive; a Referer is
    // normally absolute).
    let candidate = match referer.split_once("://") {
        Some((_, after_authority)) => match after_authority.find('/') {
            Some(slash) => &after_authority[slash..],
            None => "/", // authority with no path
        },
        None => referer,
    };
    (candidate.starts_with('/') && !candidate.starts_with("//")).then(|| candidate.to_string())
}

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    /// The index. The SQLite `Connection` is not `Sync`, so it lives behind a
    /// mutex. Handlers never hold the lock across an `.await`.
    pub index: Arc<Mutex<Index>>,
    /// External base URL for feed/enclosure links (no trailing slash).
    pub base_url: String,
    /// Canonical data dir — extracted (chaptered) audio must stay under it.
    pub data_dir: PathBuf,
    /// Canonical library root. Whole-file episodes are streamed in place from
    /// here (Sprint 6.2), so a resolved in-place path must stay under it. This
    /// is the read-only source tree; the audio handler never writes to it.
    pub library_dir: PathBuf,
    /// Feed-level fallback cover URL for books with no embedded art.
    pub default_cover_url: Option<String>,
    /// Server-default storage mode. Under [`StorageMode::Saver`], episode
    /// files are not pre-split: the audio handler regenerates a chapter on
    /// demand and caches it. `Full` means pre-split. A book that carries its
    /// own mode overrides this.
    pub storage: StorageMode,
    /// `saver`-mode cache cap in bytes (`None` = unbounded).
    pub cache_size_bytes: Option<u64>,
    /// `saver`-mode cache TTL (`None` = size-only eviction).
    pub cache_ttl: Option<Duration>,
    /// Per-chapter regeneration locks (single-flight): concurrent requests for
    /// the same uncached chapter run ffmpeg once, not N times.
    inflight: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
    /// `true` once the initial library scan has finished. The default is
    /// `true`, so a state built for a test or an already-populated library
    /// serves normally. The server binary flips it to `false` before it kicks
    /// off the background first scan, and back to `true` when that scan
    /// completes (issue 159). While it is `false`, `GET /` serves a
    /// "Scanning…" holding page, and the capability routes (`/feed`, `/audio`,
    /// `/cover`) return 503 + `Retry-After` instead of a 502 or a bare 404. A
    /// first boot then reads as "starting up", not "broken".
    ready: Arc<AtomicBool>,
}

impl AppState {
    /// Build state. Canonicalize the data dir **and** the library root for
    /// the path-safety checks (served files must stay under one of them). The
    /// storage/cache args come from `podspine_config::Config` (the pre-split
    /// default is `StorageMode::Full`).
    ///
    /// This errors when either root cannot be canonicalized.
    /// `Config::validate` guarantees that both exist (the library is checked,
    /// the data dir is created), so a failure here means the filesystem
    /// changed under the server. A fallback to the as-given path would make
    /// every containment check fail closed: a server that silently 404s
    /// everything. A startup failure is louder and therefore kinder.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: Index,
        base_url: String,
        data_dir: &FsPath,
        library_dir: &FsPath,
        default_cover_url: Option<String>,
        storage: StorageMode,
        cache_size_bytes: Option<u64>,
        cache_ttl: Option<Duration>,
    ) -> std::io::Result<Self> {
        let data_dir = data_dir.canonicalize().inspect_err(|err| {
            tracing::error!(path = %data_dir.display(), error = %err, "cannot canonicalize the data dir");
        })?;
        let library_dir = library_dir.canonicalize().inspect_err(|err| {
            tracing::error!(path = %library_dir.display(), error = %err, "cannot canonicalize the library root");
        })?;
        Ok(Self {
            index: Arc::new(Mutex::new(index)),
            base_url,
            data_dir,
            library_dir,
            default_cover_url,
            storage,
            cache_size_bytes,
            cache_ttl,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            ready: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Flip the "initial scan finished" flag. The server binary sets it
    /// `false` before it spawns the background first scan, and `true` when
    /// that scan returns (issue 159); the state is cloned into the scan task
    /// via [`Clone`]. See [`AppState`]'s `ready` field for what the two
    /// states change.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    /// Whether the initial library scan has finished (see
    /// [`Self::set_ready`]).
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// 503 + `Retry-After` for the capability routes while the initial scan is
/// still running (issue 159). A podcatcher treats 503 as "retry shortly" and
/// keeps the subscription; it can treat a 404 as "gone". This response is not
/// routed through [`AppError`], so it does not count as a request failure. It
/// is a readiness state, not an error.
fn scanning_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        "Library is scanning; try again shortly.\n",
    )
        .into_response()
}

/// Build the router with all routes and middleware layers.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/book/{slug}", get(book))
        .route("/book/{slug}/regenerate", post(regenerate))
        .route("/theme/{mode}", post(set_theme))
        .route("/subscribe/{feed_id}", get(subscribe))
        .route("/cover/{feed_id}", get(cover))
        .route("/cover/{feed_id}/thumb", get(cover_thumb))
        .route("/feed/{feed_id}", get(feed))
        .route("/audio/{feed_id}/{number}", get(audio))
        .layer(TraceLayer::new_for_http())
        // This bounds only response *production* (not the streamed body), so
        // large audio downloads are not truncated.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        // Bound in-flight requests (DoS guard). Excess requests wait; they do
        // not exhaust resources (NFR-S3, TAD §7).
        .layer(ConcurrencyLimitLayer::new(MAX_INFLIGHT_REQUESTS))
        // The server accepts no request bodies; keep the limit tiny.
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

/// `GET /feed/{feed_id}.xml` — the route captures `{feed_id}` together with
/// the `.xml` suffix, and the handler strips the suffix before lookup. The
/// capability URL is never crawlable: an `X-Robots-Tag: noindex` keeps it out
/// of web search engines, and the `itunes:block` in the XML separately keeps
/// it out of podcast directories.
async fn feed(
    State(state): State<AppState>,
    Path(id_xml): Path<String>,
) -> Result<Response, AppError> {
    // Before the first scan completes there is no feed to build yet: answer
    // 503 (retry shortly), never a 404 that a podcatcher might treat as gone
    // (issue 159).
    if !state.is_ready() {
        return Ok(scanning_unavailable());
    }
    let feed_id = id_xml.strip_suffix(".xml").ok_or(AppError::NotFound)?;
    if !valid_feed_id(feed_id) {
        return Err(AppError::NotFound);
    }
    let xml = build_feed_xml(&state, feed_id)?;
    // The count happens after the self-check passes, so the metric means "a
    // subscriber got a usable feed", not "a request arrived".
    podspine_metrics::feed_served();
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
async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let theme = theme_from_cookie(&headers);
    // Until the initial scan finishes, hold on a "Scanning…" page. An empty
    // grid would read as "no books / broken" (issue 159).
    if !state.is_ready() {
        return Ok(Html(scanning_page(theme).into_string()));
    }
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
    Ok(Html(index_page(&cards, theme).into_string()))
}

/// Build the [`BookDetail`] view model that the detail pages share: count the
/// book's episodes and derive the public feed/subscribe URLs from its
/// capability id. Each handler keeps its own lookup (slug vs `feed_id`) and
/// page template.
fn book_detail(state: &AppState, book: BookRow) -> Result<BookDetail, AppError> {
    let episode_count = {
        let index = state.index.lock().map_err(AppError::internal)?;
        index
            .episodes_for_book(&book.id)
            .map_err(AppError::internal)?
            .len()
    };
    Ok(BookDetail {
        feed_url: format!("{}/feed/{}.xml", state.base_url, book.feed_id),
        subscribe_url: format!("{}/subscribe/{}", state.base_url, book.feed_id),
        slug: book.slug,
        feed_id: book.feed_id,
        title: book.title,
        author: book.author,
        has_cover: book.cover_path.is_some(),
        episode_count,
    })
}

/// `GET /book/{slug}` — a book's page: copy-feed-URL, QR, how-to panel.
async fn book(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    if !valid_slug(&slug) {
        return Err(AppError::NotFound);
    }
    let theme = theme_from_cookie(&headers);
    let book = {
        let index = state.index.lock().map_err(AppError::internal)?;
        index
            .get_book_by_slug(&slug)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?
    };
    let detail = book_detail(&state, book)?;
    Ok(Html(book_page(&detail, theme).into_string()))
}

/// `GET /subscribe/{feed_id}` — the "add to a podcast app" helper page
/// (per-app deep links + QRs). Keyed by capability id: the book-page QR
/// points here, so an iOS Camera scan lands on real "Open in…" app links, not
/// on raw feed XML.
async fn subscribe(
    State(state): State<AppState>,
    Path(feed_id): Path<String>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    if !valid_feed_id(&feed_id) {
        return Err(AppError::NotFound);
    }
    let theme = theme_from_cookie(&headers);
    let book = {
        let index = state.index.lock().map_err(AppError::internal)?;
        index
            .get_book_by_feed_id(&feed_id)
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?
    };
    let detail = book_detail(&state, book)?;
    Ok(Html(subscribe_page(&detail, theme).into_string()))
}

/// `POST /book/{slug}/regenerate` — rotate the book's capability `feed_id`
/// (leak recovery). The old feed/audio/cover URLs 404 immediately. The
/// handler redirects back to the book page (PRG), so a refresh does not
/// re-submit.
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

/// `POST /theme/{mode}` — persist the visitor's colour theme. `mode` is
/// `light`/`dark` (sets the `theme` cookie) or `system` (clears the cookie,
/// which reverts to the OS preference). No JS: the header picker is a form of
/// submit buttons whose `formaction` posts here. The handler sets the cookie
/// and 303-redirects back (PRG) to the same-origin page the visitor came
/// from. It is same-origin-guarded like [`regenerate`].
async fn set_theme(
    State(state): State<AppState>,
    Path(mode): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !same_origin(&headers, &state.base_url) {
        return Err(AppError::Forbidden);
    }
    // A year-long, first-party, non-`Secure` cosmetic cookie (non-`Secure` so
    // that it also works on a LAN over http). `SameSite=Lax` still lets the
    // top-level POST navigation set it. `system` clears the cookie via
    // `Max-Age=0`.
    let cookie = match Theme::parse(&mode).cookie_value() {
        Some(value) => format!("theme={value}; Path=/; Max-Age=31536000; SameSite=Lax"),
        None => "theme=; Path=/; Max-Age=0; SameSite=Lax".to_string(),
    };
    // Redirect back to the page the visitor came from. Take the *path* of the
    // Referer, not a match against `base_url`. The POST already passed the
    // same-origin guard, so the Referer is this origin's page. And the browse
    // UI is often on a different origin than `base_url` (which addresses
    // podcatchers), so a comparison against it would bounce every toggle to
    // `/`. Only a single-slash absolute path is kept, so a protocol-relative
    // (`//host`) or off-site value cannot become an open redirect.
    let back = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(referer_path)
        .unwrap_or_else(|| "/".to_string());

    let mut resp = Redirect::to(&back).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(AppError::internal)?,
    );
    Ok(resp)
}

/// `GET /cover/{feed_id}` — the book's cover image, keyed by capability id so
/// that it is not a guessable catalog-probe surface. Cover extraction
/// (Task 3.4) populates covers; until then books have no cover and this 404s.
/// The handler canonicalizes the stored path and confirms it is under the
/// data dir before it serves the file.
///
/// Cached: an `ETag` (a blake3 hash of the served bytes) plus `Cache-Control`
/// let a browser revalidate to a bodyless `304` instead of a re-download of
/// the (often multi-MB) image on every page refresh. The grid pulls many
/// covers at once, so over a slow link (e.g. Tailscale) that re-download was
/// the bottleneck.
async fn cover(
    State(state): State<AppState>,
    Path(feed_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // No covers exist until the first scan finishes: 503 (retry), not 404.
    if !state.is_ready() {
        return Ok(scanning_unavailable());
    }
    if !valid_feed_id(&feed_id) {
        return Err(AppError::NotFound);
    }
    let cover_path = book_cover_path(&state, &feed_id)?;
    let canonical = resolve_under_data_dir(&cover_path, &state.data_dir, &feed_id)?;
    serve_image(&canonical, &headers).await
}

/// `GET /cover/{feed_id}/thumb` — a small `cover_thumb.jpg` for the browse UI
/// grid (the RSS feed and `/cover` keep the full-res image). Serving is
/// read-only: the scanner generates the thumbnail alongside the cover, and
/// this handler only serves it. When the thumbnail is not generated yet (the
/// reconcile backfills a missing one on the next scan), or generation failed,
/// the handler falls back to the full cover; it does not 404.
async fn cover_thumb(
    State(state): State<AppState>,
    Path(feed_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !state.is_ready() {
        return Ok(scanning_unavailable());
    }
    if !valid_feed_id(&feed_id) {
        return Err(AppError::NotFound);
    }
    let cover_path = book_cover_path(&state, &feed_id)?;
    // The full cover is the source of truth (must exist, under the data dir).
    let cover = resolve_under_data_dir(&cover_path, &state.data_dir, &feed_id)?;

    // Prefer the scanner-generated thumbnail next to the cover. Fall back to
    // the full cover when the thumbnail is not generated yet (the reconcile
    // backfills a missing one) or generation failed, so a book with art never
    // 404s. Serving is read-only: generation lives in the scanner
    // (single-threaded, atomic), and that split keeps a thumbnail consistent
    // with its cover with no cross-thread race.
    //
    // Fall back to the cover when the thumbnail *serve* fails too, not only
    // when it cannot be resolved. A re-ingest deletes the old thumb before it
    // regenerates it, so the thumb can vanish between canonicalize and read.
    // The cover is only ever atomically replaced (never absent), so serving
    // it is always safe.
    if let Some(dir) = cover.parent() {
        let thumb = cover_thumb_path(dir);
        if let Ok(canonical) =
            resolve_under_data_dir(&thumb.to_string_lossy(), &state.data_dir, &feed_id)
            && let Ok(resp) = serve_image(&canonical, &headers).await
        {
            return Ok(resp);
        }
    }
    serve_image(&cover, &headers).await
}

/// The book's stored (full) cover path, or `NotFound` when the book/cover is absent.
fn book_cover_path(state: &AppState, feed_id: &str) -> Result<String, AppError> {
    let index = state.index.lock().map_err(AppError::internal)?;
    index
        .get_book_by_feed_id(feed_id)
        .map_err(AppError::internal)?
        .ok_or(AppError::NotFound)?
        .cover_path
        .ok_or(AppError::NotFound)
}

/// The A01 containment check that every serve path funnels through (TAD §7):
/// canonicalize `path` (this resolves `..` and symlinks) and confirm that the
/// result stays under the trusted `root`. A path that cannot be canonicalized
/// (a missing file), or that resolves outside `root`, is rejected as
/// `NotFound`. The rejection is never a distinct status, so it is not an
/// existence oracle. `what` names the site in the operator's log; the client
/// never sees it. `number` is the episode number when the site has one; it
/// identifies the poisoned row in the warn.
fn canonical_under(
    path: &FsPath,
    root: &FsPath,
    what: &'static str,
    feed_id: &str,
    number: Option<u32>,
) -> Result<PathBuf, AppError> {
    let canonical = path.canonicalize().map_err(|_| AppError::NotFound)?;
    if !canonical.starts_with(root) {
        match number {
            Some(number) => tracing::warn!(feed_id, number, "{what}"),
            None => tracing::warn!(feed_id, "{what}"),
        }
        return Err(AppError::NotFound);
    }
    Ok(canonical)
}

/// Canonicalize a stored image path and confirm that it stays under the data
/// dir. A resolved path that escapes the data dir is rejected as `NotFound`,
/// never served. This also fails with `NotFound` when the file does not
/// exist; the thumbnail handler relies on that to fall back to the full
/// cover.
fn resolve_under_data_dir(
    path: &str,
    data_dir: &FsPath,
    feed_id: &str,
) -> Result<PathBuf, AppError> {
    canonical_under(
        FsPath::new(path),
        data_dir,
        "resolved cover path escaped the data dir",
        feed_id,
        None,
    )
}

/// Serve an on-disk image with content-addressed caching: an `ETag` (a blake3
/// hash of the served bytes) plus `Cache-Control`, and a bodyless `304` on a
/// matching `If-None-Match`. A browser then revalidates cheaply instead of a
/// re-download of the image on every page refresh. `canonical` is already
/// resolved and confirmed under the data dir. The `ETag` is over the exact
/// bytes (not stat metadata), so it cannot advertise a stale validator
/// against a cover that was re-extracted between a stat and the read.
async fn serve_image(canonical: &FsPath, headers: &HeaderMap) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(canonical)
        .await
        .map_err(|_| AppError::NotFound)?;
    let etag = format!("\"{}\"", blake3::hash(&bytes).to_hex());
    let etag_value = HeaderValue::from_str(&etag).map_err(AppError::internal)?;
    let cache_control = HeaderValue::from_static("public, max-age=300");

    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| {
            h.trim() == "*"
                || h.split(',')
                    .any(|t| t.trim().trim_start_matches("W/") == etag)
        });
    if unchanged {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        resp.headers_mut().insert(header::ETAG, etag_value);
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, cache_control);
        return Ok(resp);
    }

    let mime = image_mime(&canonical.to_string_lossy());
    let mut resp = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::ETAG, etag_value);
    h.insert(header::CACHE_CONTROL, cache_control);
    Ok(resp)
}

/// `GET /audio/{feed_id}/{number}` — stream an episode with Range support.
///
/// The handler sets the `Content-Type` explicitly. `axum-range`'s `Ranged`
/// emits Content-Range/Accept-Ranges/Content-Length but NO Content-Type, and
/// a missing type makes strict clients (Apple Podcasts / iOS `AVPlayer`) refuse
/// to play with "can't be played on this device", even though the enclosure
/// carries `type=`.
async fn audio(
    State(state): State<AppState>,
    Path((feed_id, number)): Path<(String, u32)>,
    range: Option<TypedHeader<Range>>,
) -> Result<Response, AppError> {
    // Episodes are not materialized until the first scan finishes: 503
    // (retry), not 404, so a podcatcher that polls an enclosure keeps trying
    // (issue 159).
    if !state.is_ready() {
        return Ok(scanning_unavailable());
    }
    // The resolver snapshots the row under the index lock and releases the
    // lock before any file I/O. A re-ingest that moves this book into another
    // container (a transcode flag or target flip) can therefore replace the
    // rows and sweep the old file between that snapshot and the `File::open`
    // below. Every step from here fails closed, so the request 404s and the
    // client retries; it can never receive partial or stale-length bytes. See
    // the note at the sweep in `podspine_scanner::scan_book_as` for why that
    // microsecond window is left unguarded.
    let target = resolve_audio_target(&state, &feed_id, number)?;
    // When the resolver supplied a `Regen` (a `saver` chapter split, or a
    // faststart remux of a whole-file episode), a missing file is regenerated
    // on demand. Otherwise (e.g. a `full`-mode chapter, an in-place whole
    // file) it is a genuine 404.
    if !target.path.exists() {
        match &target.regen {
            Some(regen) => ensure_cached(&state, &target.path, regen).await?,
            None => return Err(AppError::NotFound),
        }
    }
    // Final defense in depth: the file now exists, so canonicalize it (this
    // resolves any symlink) and confirm that it still lives under a trusted
    // root before the open. The trusted roots are the data dir (extracted
    // chapters) OR the library root (whole-file episodes served in place).
    // The resolver already checked the relevant root; this check additionally
    // catches a file that is itself a symlink that points outside.
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
    Ok(([(header::CONTENT_TYPE, mime)], Ranged::new(range, body)).into_response())
}

/// Build and self-check the feed XML for a capability `feed_id`. All public
/// URLs in the feed (self link, enclosures, cover) are built from `feed_id`,
/// so the whole book is reachable from the one capability, and nothing is
/// guessable.
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
    // The per-book cover is served at `/cover/{feed_id}` when extracted. The
    // fallbacks, in order: a per-book `.podspine.toml` URL (Sprint 6.4), then
    // the server-wide one, then no image at all. See Task 3.4.
    let cover_url = book
        .cover_path
        .as_ref()
        .map(|_| format!("{base}/cover/{feed_id}"))
        .or_else(|| book.default_cover_url.clone())
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

/// Whether this book's episodes were **re-encoded** at ingest (Task 5.2).
///
/// A re-encode is not byte-reproducible across ffmpeg builds. So such an
/// episode must never be regenerated on demand (the rebuilt file's length
/// could no longer match the `enclosure length` already published in the
/// feed), and it must never be evicted (nothing could rebuild it). Both
/// callers below therefore treat a transcoded book as `full`, independent of
/// its `storage_mode`. `None` is a pre-5.2 row, which was necessarily
/// stream-copied.
fn book_is_transcoded(book: &BookRow) -> bool {
    book.transcode.is_some_and(TranscodeMode::is_on)
}

/// Whether a book's effective storage mode is `saver`: its per-book
/// `.podspine.toml` override (Sprint 6.4) if set, else the server default.
/// `None` is a pre-6.4 row that follows the server config until it is
/// re-scanned.
fn book_is_saver(book: &BookRow, global: StorageMode) -> bool {
    book.storage_mode.unwrap_or(global) == StorageMode::Saver
}

/// What `/audio` needs: the canonical target file (which may not exist yet in
/// `saver` mode) and, in `saver` mode, everything to regenerate it on demand.
struct AudioTarget {
    path: PathBuf,
    regen: Option<Regen>,
}

/// Inputs to regenerate one cache file on demand: a `saver` chapter split, or
/// a faststart remux of a whole-file episode (Sprint 6.3). The source is
/// always a canonicalized file asserted to live under the library root. Both
/// arms of [`resolve_audio_target`] validate before they construct this, so
/// nothing that reaches ffmpeg can have escaped. The op decides which ffmpeg
/// call rebuilds the deterministic output.
struct Regen {
    source: PathBuf,
    out_dir: PathBuf,
    out_ext: String,
    op: RegenOp,
}

/// Which ffmpeg operation regenerates the cache file.
#[derive(Clone)]
enum RegenOp {
    /// Re-split one chapter (`saver` mode): a sub-range of the container.
    Chapter(ChapterCut),
    /// Remux a whole file to faststart (`PODSPINE_REMUX_NON_FASTSTART`).
    Faststart { idx: usize, duration_sec: f64 },
}

/// Resolve `/audio/{feed_id}/{number}` to its on-disk target. There are three
/// path-safe shapes, by whether the episode is a whole file (and how it is
/// stored):
///
/// - **In place (whole-file episode, `file_path == source_path`):** a whole
///   file streamed from the library. It is canonicalized, asserted under the
///   library root, and size-checked against the recorded length (Sprint 6.2).
/// - **Faststart remux (whole-file episode, `file_path != source_path`):**
///   the served file is a cache copy under the data dir. A remux of the
///   library source to faststart regenerates it on demand (Sprint 6.3), so
///   the resolver returns a [`Regen`] that carries [`RegenOp::Faststart`].
/// - **Extracted (chaptered episode):** the path is reconstructed from the
///   canonical `data_dir` plus **opaque DB keys** (`book.id`, chapter index)
///   and a validated audio extension. It is never built from request input,
///   so it stays under the data dir by construction (no traversal). In
///   `saver` mode a chapter split regenerates it on demand
///   ([`RegenOp::Chapter`]), and existence is not required — unless the book
///   was transcoded (Task 5.2); a transcoded book is never regenerated.
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

    // Serve-in-place (Sprint 6.2): a whole-file episode streamed directly
    // from the read-only library, never copied under the data dir. It is
    // recognized by a non-empty `source_path` whose value equals `file_path`.
    // (A whole-file episode whose `file_path != source_path` was remuxed to a
    // faststart cache copy; the data-dir path below handles it.) Two guards,
    // both 404 on failure:
    //   1. Canonicalize and assert under the library root (reject a `..` or
    //      symlink escape): the A01 "assert under the library root" rule
    //      (TAD §7).
    //   2. The recorded enclosure length must equal the on-disk source size:
    //      the WHOLE-FILE invariant. A chaptered episode (a sub-range) that
    //      wrongly carries a `source_path` (from a bad migration, a partial
    //      rescan, or a manual edit) has a chapter-sized `byte_length` that
    //      differs from the container size. It is rejected here; the full
    //      container's bytes are never served under the chapter's enclosure
    //      length.
    // This branch returns before the data-dir/regeneration logic below, so a
    // poisoned row can never fall through into it.
    if !ep.source_path.is_empty() && ep.file_path == ep.source_path {
        let src = canonical_under(
            FsPath::new(&ep.source_path),
            &state.library_dir,
            "in-place audio escaped the library root",
            feed_id,
            Some(number),
        )?;
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

    // The container extension is the audio extension the scanner recorded.
    // Reject anything non-alphanumeric, so that it can never introduce a path
    // separator.
    let out_ext = FsPath::new(&ep.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .ok_or(AppError::NotFound)?;
    // Defense in depth, at runtime (not a debug_assert that vanishes in
    // release): resolve the book dir and confirm that it stays under the
    // canonical data dir before anything is opened or written. The components
    // are opaque DB keys, but a poisoned row must never let a path escape.
    // The chapter file itself may not exist yet (saver), so canonicalize the
    // parent dir. A book dir that does not exist (never ingested) is a clean
    // 404.
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
    let path = out_dir.join(episode_file_name(idx as usize, &out_ext));

    // Two kinds of episode materialize under the data dir here:
    let regen = if !ep.source_path.is_empty() && ep.needs_faststart {
        // A non-faststart whole-file episode remuxed to a faststart cache
        // copy (Sprint 6.3, `file_path != source_path`). Regenerate it on
        // demand from the library source: always, independent of
        // `storage_mode`. The `needs_faststart` gate means a chaptered row
        // that merely carries a stray `source_path` is NOT remuxed into its
        // container here; it drops to the chaptered arm and serves its actual
        // split (or 404s). Validate first that the source stays under the
        // library root (the A01 rule); 404 on escape.
        let src = canonical_under(
            FsPath::new(&ep.source_path),
            &state.library_dir,
            "remux source escaped the library root",
            feed_id,
            Some(number),
        )?;
        Some(Regen {
            source: src,
            out_dir,
            out_ext,
            op: RegenOp::Faststart {
                idx: idx as usize,
                duration_sec: ep.duration_sec,
            },
        })
    } else if book_is_saver(&book, state.storage)
        && !book_is_transcoded(&book)
        && FsPath::new(&book.source_path).is_file()
    {
        // A chaptered episode. Regen is possible only in `saver` mode
        // (per-book, Sprint 6.4) when the book's source is a single file to
        // re-split. The `is_file` guard is belt-and-suspenders (a directory
        // source would make `ffmpeg <directory>` fail), and a missing file is
        // a clean 404, not a 500.
        //
        // The same A01 containment rule as the remux arm above applies:
        // `book.source_path` is an opaque DB value, so canonicalize it and
        // assert that it stays under the library root before it can reach
        // ffmpeg. The check runs here, not at regen time, so that a poisoned
        // row is rejected before anything is opened or written — including
        // when the cached chapter happens to already exist.
        let src = canonical_under(
            FsPath::new(&book.source_path),
            &state.library_dir,
            "saver regen source escaped the library root",
            feed_id,
            Some(number),
        )?;
        Some(Regen {
            source: src,
            out_dir,
            out_ext,
            // `end_sec` is reconstructed as start + duration. This is EXACT,
            // not an approximation: the scanner stores
            // `duration_sec = cut.end - cut.start` (the requested cut length,
            // not a measured output duration), so this yields the same
            // `[start, end)` that the ingest split used. And ffmpeg's
            // 6-decimal arg formatting absorbs any float round-trip. The
            // stream copy is therefore byte-identical (asserted in the serve
            // test).
            op: RegenOp::Chapter(ChapterCut {
                idx: idx as usize,
                start_sec: ep.start_sec,
                end_sec: ep.start_sec + ep.duration_sec,
            }),
        })
    } else {
        None
    };
    Ok(AudioTarget { path, regen })
}

/// Ensure that `target` exists; regenerate it on demand (a `saver` chapter
/// split, or a whole-file faststart remux, Sprint 6.3). A per-path
/// single-flight lock means concurrent requests for the same uncached file
/// run ffmpeg once. The blocking ffmpeg work runs off the async runtime.
async fn ensure_cached(state: &AppState, target: &FsPath, regen: &Regen) -> Result<(), AppError> {
    let lock = {
        let mut map = state.inflight.lock().map_err(AppError::internal)?;
        map.entry(target.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let outcome = async {
        // A concurrent request may have produced the file during the wait on
        // the lock.
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

        // Keep the cache under its cap/TTL; never evict the file just
        // produced.
        enforce_cache(state, target).await;
        Ok(())
    }
    .await;

    // Drop the single-flight entry, so that the map stays bounded to
    // *in-flight* regenerations, not every chapter ever served. Any waiter
    // still blocked on `.lock()` holds its own `Arc` clone of this same
    // mutex, and every path re-checks `target.exists()` after it acquires the
    // lock. So the removal of the map entry here can never cause a duplicate
    // ffmpeg run.
    if let Ok(mut map) = state.inflight.lock() {
        map.remove(target);
    }
    outcome
}

/// Evict cached chapter files to keep the `saver` cache under its size cap
/// and TTL. `keep` (the file just served) is never evicted. This is a no-op
/// when both limits are unset. It is best-effort: eviction never fails a
/// request.
async fn enforce_cache(state: &AppState, keep: &FsPath) {
    let (cap, ttl) = (state.cache_size_bytes, state.cache_ttl);
    if cap.is_none() && ttl.is_none() {
        return; // unbounded + no TTL: nothing to evict
    }
    let books = state.data_dir.join("books");
    // Evict only from **effective-`saver`, single-file-source,
    // stream-copied** books (per-book `storage_mode`, Sprint 6.4): their
    // cached chapters re-split on demand, so a delete is safe. A transcoded
    // book (Task 5.2) is excluded: nothing regenerates a re-encode, so an
    // eviction would 404 until a rescan. A `full` book's chapters are kept:
    // an eviction would 404 (no regen). A non-single-file book (MP3 folder)
    // is served in place, so those dirs are left alone. (A `full` book's
    // remux cache, if any, therefore persists: a minor, safe
    // over-conservatism.) Snapshot the sources without the lock held across
    // the `is_file` stats.
    let sources: Vec<(String, bool)> = {
        let Ok(index) = state.index.lock() else {
            tracing::warn!("cache eviction skipped: index lock poisoned");
            return;
        };
        match index.list_books() {
            Ok(bs) => bs
                .into_iter()
                .map(|b| {
                    let regen = book_is_saver(&b, state.storage)
                        && !book_is_transcoded(&b)
                        && FsPath::new(&b.source_path).is_file();
                    (b.id, regen)
                })
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, "cache eviction skipped: listing books failed");
                return;
            }
        }
    };
    let regenerable: HashSet<PathBuf> = sources
        .into_iter()
        .filter(|(_, regen)| *regen)
        .map(|(id, _)| books.join(id))
        .collect();
    let keep = keep.to_path_buf();
    if let Err(err) =
        tokio::task::spawn_blocking(move || evict(&books, cap, ttl, &keep, &regenerable)).await
    {
        tracing::warn!(error = %err, "cache eviction task failed");
    }
}

/// Collect cached chapter files (numeric stems under `books/*/`) from
/// **regenerable** books only. Drop TTL-expired ones. Then delete
/// oldest-first until the total is under `cap`. The mtime is the LRU key: a
/// chapter regeneration refreshes it. Non-regenerable book dirs (a
/// directory-source book, or a legacy pre-6.2 copy) are skipped entirely, so
/// nothing that cannot be rebuilt is destroyed. Best-effort; per-file I/O
/// errors are ignored.
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
            // Only chapter files (`001.m4a`-style, numeric stem); this skips
            // covers.
            let numeric = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(is_episode_stem);
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

/// Handler error. It maps to a status code and never leaks internals.
#[derive(Debug)]
enum AppError {
    NotFound,
    Forbidden,
    Internal,
}

impl AppError {
    /// Collapse any error into `Internal` and log the detail. The client sees
    /// only a 500; the operator gets the cause.
    fn internal<E: std::fmt::Display>(e: E) -> Self {
        tracing::error!(error = %e, "internal error");
        AppError::Internal
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Counting here rather than at each `return Err(..)` catches every error
        // path exactly once, including the ones added later.
        podspine_metrics::error(match self {
            AppError::NotFound => podspine_metrics::ErrorKind::NotFound,
            AppError::Forbidden => podspine_metrics::ErrorKind::Forbidden,
            AppError::Internal => podspine_metrics::ErrorKind::Internal,
        });
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
    use podspine_test_support::scratch;

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_for("/x/001.m4a"), "audio/mp4");
        assert_eq!(mime_for("/x/001.MP3"), "audio/mpeg");
        assert_eq!(mime_for("/x/001.flac"), "audio/flac");
        assert_eq!(mime_for("/x/001.ogg"), "audio/ogg");
        assert_eq!(mime_for("/x/001.opus"), "audio/ogg");
        assert_eq!(mime_for("/x/blob"), "audio/mp4");
    }

    /// `evict` deletes files, so pin its guard branches directly: a
    /// regenerable book dir that vanished, a directory entry with a numeric
    /// stem, a non-regenerable sibling, the under-cap early return, and the
    /// stop-once-under-cap break.
    #[test]
    fn evict_edge_branches() {
        let dir = scratch("http-evict-edges");
        let books = dir.path().join("books");
        let b1 = books.join("b1");
        std::fs::create_dir_all(&b1).unwrap();
        // Chapters: 001 backdated (the LRU victim), 002 fresh. A cover (skipped
        // by stem) and a directory with a chapter-shaped name (skipped by kind).
        let old = b1.join("001.m4a");
        let newer = b1.join("002.m4a");
        std::fs::write(&old, [0u8; 10]).unwrap();
        std::fs::write(&newer, [0u8; 10]).unwrap();
        std::fs::write(b1.join("cover.jpg"), [0u8; 64]).unwrap();
        std::fs::create_dir(b1.join("003.m4a")).unwrap();
        let past = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        // Non-regenerable sibling: never a candidate.
        let b2 = books.join("b2");
        std::fs::create_dir_all(&b2).unwrap();
        std::fs::write(b2.join("001.m4a"), [0u8; 10]).unwrap();

        // Regenerable set: b1, plus a dir that no longer exists on disk.
        let regenerable: HashSet<PathBuf> = [b1.clone(), books.join("gone")].into();

        // Under cap: nothing deleted.
        evict(&books, Some(1024), None, FsPath::new("/keep"), &regenerable);
        assert!(old.exists() && newer.exists(), "under cap deletes nothing");

        // Cap of one file: delete oldest-first, then stop once under cap.
        evict(&books, Some(10), None, FsPath::new("/keep"), &regenerable);
        assert!(!old.exists(), "oldest chapter evicted first");
        assert!(newer.exists(), "eviction stops once under cap");
        assert!(b1.join("cover.jpg").exists(), "covers are never candidates");
        assert!(
            b1.join("003.m4a").is_dir(),
            "directories are never candidates"
        );
        assert!(
            b2.join("001.m4a").exists(),
            "non-regenerable book untouched"
        );
    }

    /// Eviction is best-effort and must never panic or fail a request. The
    /// test fault-injects both abandon paths: a database failure (a second
    /// connection drops the `book` table out from under a live [`Index`]) and
    /// a poisoned index lock. Each returns quietly after it logs a warn.
    #[tokio::test]
    async fn enforce_cache_survives_a_broken_index_and_a_poisoned_lock() {
        let dir = scratch("http-enforce-cache-faults");
        let db = dir.path().join("test.db");
        let state = AppState::new(
            Index::open(&db).unwrap(),
            "http://test".to_string(),
            dir.path(),
            dir.path(),
            None,
            StorageMode::Saver,
            Some(1), // a cap, so eviction doesn't no-op before the fault
            None,
        )
        .expect("test dirs canonicalize");

        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("DROP TABLE book", [])
            .unwrap();
        enforce_cache(&state, FsPath::new("/keep")).await; // DB error: skipped, logged

        let poison = state.clone();
        std::thread::spawn(move || {
            let _guard = poison.index.lock().unwrap();
            panic!("poison the index lock");
        })
        .join()
        .expect_err("the poisoning thread must panic");
        enforce_cache(&state, FsPath::new("/keep")).await; // poisoned lock: skipped, logged
    }

    #[test]
    fn book_is_saver_follows_per_book_then_global() {
        let mk = |mode: Option<StorageMode>| BookRow {
            id: "b".into(),
            slug: "b".into(),
            feed_id: "cap".into(),
            title: "T".into(),
            author: None,
            cover_path: None,
            source_path: "/x".into(),
            source_mtime: 0,
            storage_mode: mode,
            default_cover_url: None,
            force_embedded: false,
            transcode: None,
        };
        // An explicit per-book mode wins regardless of the server default.
        assert!(book_is_saver(
            &mk(Some(StorageMode::Saver)),
            StorageMode::Full
        ));
        assert!(!book_is_saver(
            &mk(Some(StorageMode::Full)),
            StorageMode::Saver
        ));
        // No mode (a pre-6.4 row) follows the server default.
        assert!(book_is_saver(&mk(None), StorageMode::Saver));
        assert!(!book_is_saver(&mk(None), StorageMode::Full));
    }

    #[test]
    fn a_transcoded_book_is_never_treated_as_regenerable() {
        let mk = |storage: Option<StorageMode>, transcode: Option<TranscodeMode>| BookRow {
            id: "b".into(),
            slug: "b".into(),
            feed_id: "cap".into(),
            title: "T".into(),
            author: None,
            cover_path: None,
            source_path: "/x".into(),
            source_mtime: 0,
            storage_mode: storage,
            default_cover_url: None,
            force_embedded: false,
            transcode,
        };
        let saver = Some(StorageMode::Saver);
        // A re-encode cannot be rebuilt byte-for-byte, so `saver` must not
        // apply.
        assert!(book_is_transcoded(&mk(saver, Some(TranscodeMode::Aac))));
        assert!(book_is_transcoded(&mk(saver, Some(TranscodeMode::Mp3))));
        // Stream-copied books (and pre-5.2 rows) stay regenerable.
        assert!(!book_is_transcoded(&mk(saver, Some(TranscodeMode::Off))));
        assert!(!book_is_transcoded(&mk(saver, None)));
        // The two predicates compose: still a saver book, but not a regenerable one.
        assert!(book_is_saver(
            &mk(saver, Some(TranscodeMode::Aac)),
            StorageMode::Full
        ));
    }

    #[test]
    fn valid_slug_allow_list() {
        // Accepts exactly what slugify produces.
        assert!(valid_slug("dracula"));
        assert!(valid_slug("dracula-2"));
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
            "Dracula",
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
    fn referer_path_extracts_path_and_blocks_open_redirect() {
        // An absolute URL gives its path+query, independent of host/port (so
        // a UI origin that differs from `base_url` still returns to the right
        // page).
        assert_eq!(
            referer_path("http://192.168.1.5:8080/book/dracula").as_deref(),
            Some("/book/dracula")
        );
        assert_eq!(
            referer_path("https://podspine.example/subscribe/x?y=1").as_deref(),
            Some("/subscribe/x?y=1")
        );
        // An absolute URL with no path gives the root.
        assert_eq!(referer_path("http://host").as_deref(), Some("/"));
        // A bare path is accepted as-is.
        assert_eq!(referer_path("/book/x").as_deref(), Some("/book/x"));
        // Open-redirect vectors are rejected (caller falls back to `/`).
        assert_eq!(referer_path("http://evil.com//evil.com/x"), None);
        assert_eq!(referer_path("//evil.com/x"), None);
        assert_eq!(referer_path("javascript:alert(1)"), None);
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
        // A non-browser client (no headers) is allowed; it has no ambient auth
        // to abuse.
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
        let dir = scratch("evict-size");
        let books = dir.join("books");
        let bk = books.join("b1");
        for n in 1..=3 {
            touch(&bk.join(format!("{n:03}.m4a")), 100);
        }
        touch(&bk.join("cover.jpg"), 500); // non-numeric stem: never a cache file
        let keep = bk.join("003.m4a");

        // Cap 150B: `keep` (100B) is protected, so older chapters are evicted.
        evict(&books, Some(150), None, &keep, &regen_set(&[&bk]));

        assert!(keep.exists(), "the just-served file is kept");
        assert!(
            bk.join("cover.jpg").exists(),
            "non-chapter files are untouched"
        );
        assert!(numeric_files(&bk) <= 1, "size cap evicted older chapters");
    }

    #[test]
    fn evict_drops_ttl_expired_chapters_except_keep() {
        let dir = scratch("evict-ttl");
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
    }

    #[test]
    fn evict_is_a_noop_without_a_cap_or_ttl() {
        let dir = scratch("evict-noop");
        let books = dir.join("books");
        let bk = books.join("b1");
        touch(&bk.join("001.m4a"), 100);
        let keep = bk.join("001.m4a");

        evict(&books, None, None, &keep, &regen_set(&[&bk])); // unbounded + no TTL

        assert!(keep.exists());
        assert_eq!(numeric_files(&bk), 1, "nothing evicted when unbounded");
    }

    #[test]
    fn evict_tolerates_a_missing_books_dir() {
        // A failed top-level read_dir is a clean no-op, not a panic.
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
        // A regenerable (single-file-source) book and a non-regenerable book
        // dir (a directory source, e.g. an MP3 folder, or a legacy pre-6.2
        // copy). Only the regenerable one may be evicted. The non-regenerable
        // files must survive even a tiny cap (Greptile P1): nothing would
        // rebuild them.
        let dir = scratch("evict-mp3safe");
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
    }

    #[test]
    fn every_apperror_maps_to_its_status_and_never_leaks_a_body() {
        // Also covers the metrics mapping arms: each variant must count as its
        // own kind, and none may put internals on the wire.
        for (err, want) in [
            (AppError::NotFound, StatusCode::NOT_FOUND),
            (AppError::Forbidden, StatusCode::FORBIDDEN),
            (AppError::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let response = err.into_response();
            assert_eq!(response.status(), want);
        }
    }

    #[test]
    fn internal_swallows_the_source_error() {
        // `AppError::internal` is what every `.map_err(..)` on the request
        // path funnels through. Whatever the underlying error was (a poisoned
        // mutex, a rusqlite failure), it must collapse to a bare `Internal`,
        // so that no filesystem path or SQL text can reach the client.
        let from_io = AppError::internal(std::io::Error::other("secret /srv/path detail"));
        assert!(matches!(from_io, AppError::Internal));
        assert_eq!(
            from_io.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        // Generic over the error type, so exercise a second instantiation.
        let from_str = AppError::internal("some other failure");
        assert!(matches!(from_str, AppError::Internal));
    }
}
