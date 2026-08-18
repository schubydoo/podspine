//! `ui` — `maud` server-rendered pages: book grid (cover, title), a per-book page
//! (copy feed URL + a QR to the subscribe page), and a `/subscribe` helper page
//! with per-app "Open in…" deep links + QRs. Compiled into the binary (no runtime
//! template files). No player.
//!
//! This crate is pure presentation: it takes plain view models ([`BookCard`],
//! [`BookDetail`]) and returns [`maud::Markup`], so it has no dependency on the
//! index or HTTP layers and is unit-testable without a database. The `http`
//! crate maps `BookRow`s into these and mounts `GET /` + `GET /book/{slug}`.
//! See TAD §4. Accessibility target NFR-C3: keyboard-navigable, alt text on
//! covers, AA contrast, and the feed URL usable without JavaScript.

use maud::{DOCTYPE, Markup, PreEscaped, html};
use qrcode::QrCode;
use qrcode::render::svg;

/// One book as shown in the grid on `GET /`.
pub struct BookCard {
    /// URL slug — the human `/book/{slug}` key (browse UI only).
    pub slug: String,
    /// Capability id — the `/cover/{feed_id}` key (unguessable).
    pub feed_id: String,
    /// Human title.
    pub title: String,
    /// Author, if known.
    pub author: Option<String>,
    /// Whether a cover image is available to serve at `/cover/{feed_id}`.
    pub has_cover: bool,
}

/// A single book's detail page (`GET /book/{slug}`).
pub struct BookDetail {
    /// URL slug — the human `/book/{slug}` key (also the base for the
    /// regenerate POST action).
    pub slug: String,
    /// Capability id — the `/cover/{feed_id}` key (unguessable).
    pub feed_id: String,
    /// Human title.
    pub title: String,
    /// Author, if known.
    pub author: Option<String>,
    /// Whether a cover image is available to serve at `/cover/{feed_id}`.
    pub has_cover: bool,
    /// The exact, working capability feed URL (what "copy" yields, pasted into apps).
    pub feed_url: String,
    /// Absolute URL of the `/subscribe/{feed_id}` helper page (what the book-page
    /// QR encodes, so an iOS Camera scan opens a real page instead of raw XML).
    pub subscribe_url: String,
    /// Number of episodes (chapters) in the feed.
    pub episode_count: usize,
}

/// Shared styles + a page shell. Inlined so the binary needs no static assets.
/// The palette is chosen for WCAG AA contrast (NFR-C3): `#18181b` text on white
/// (~16:1), `#52525b` muted (~7:1), and white on the `#1d4ed8` accent (~5.3:1).
/// Everything is driven through the `--*` custom properties, so a theme is just an
/// override of those same variables — no per-rule duplication. The default (no
/// `data-theme`) follows the OS via `prefers-color-scheme`; an explicit choice from
/// the header picker sets `<html data-theme="light|dark">` (persisted in a cookie,
/// read server-side) which wins over the media query. `color-scheme` keeps native
/// controls/scrollbars in step. AA-contrast dark palette — `#f4f4f5` text on
/// `#18181b` (~16:1), `#a1a1aa` muted (~7:1), `#0b1120` on the lighter `#60a5fa`
/// accent (~7:1). The QR SVGs keep their own white background (`.appqr svg`) so a
/// phone can still scan in dark mode.
const STYLE: &str = r#"
:root { color-scheme: light dark;
        --bg:#ffffff; --surface:#f4f4f5; --border:#d4d4d8; --text:#18181b;
        --muted:#52525b; --accent:#1d4ed8; --accent-text:#ffffff; --danger:#b91c1c; }
/* Explicit "Light" from the picker: pin the scheme so native controls don't follow
   a dark OS; the light palette above already applies. */
:root[data-theme="light"] { color-scheme: light; }
/* The dark palette lives in two selectors so it applies both for an explicit "Dark"
   choice and for the OS default when the visitor hasn't chosen (:not([data-theme])). */
:root[data-theme="dark"] { color-scheme: dark;
        --bg:#18181b; --surface:#27272a; --border:#3f3f46; --text:#f4f4f5;
        --muted:#a1a1aa; --accent:#60a5fa; --accent-text:#0b1120; --danger:#f87171; }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme]) { --bg:#18181b; --surface:#27272a; --border:#3f3f46;
        --text:#f4f4f5; --muted:#a1a1aa; --accent:#60a5fa; --accent-text:#0b1120;
        --danger:#f87171; }
}
* { box-sizing:border-box; }
body { margin:0; font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
       color:var(--text); background:var(--bg); }
a { color:var(--accent); }
:focus-visible { outline:3px solid var(--accent); outline-offset:2px; border-radius:4px; }
header.site { padding:1rem 1.25rem; border-bottom:1px solid var(--border);
        display:flex; align-items:center; justify-content:space-between; gap:1rem; }
header.site h1 { margin:0; font-size:1.25rem; }
header.site a { text-decoration:none; color:var(--text); }
/* Theme picker: a no-JS segmented control (a form of submit buttons). The active
   theme is marked with aria-pressed, which also styles it. */
.themepicker { display:inline-flex; margin:0; border:1px solid var(--border);
        border-radius:6px; overflow:hidden; }
.themepicker button { font:inherit; font-size:.8rem; padding:.35rem .7rem; border:0;
        border-left:1px solid var(--border); background:var(--surface); color:var(--text);
        cursor:pointer; }
.themepicker button:first-child { border-left:0; }
.themepicker button[aria-pressed="true"] { background:var(--accent); color:var(--accent-text); }
main { max-width:960px; margin:0 auto; padding:1.5rem 1.25rem; }
.grid { list-style:none; margin:0; padding:0; display:grid; gap:1.25rem;
        grid-template-columns:repeat(auto-fill,minmax(150px,1fr)); }
.card a { display:block; text-decoration:none; color:var(--text); }
.cover, .placeholder { width:100%; aspect-ratio:1/1; border-radius:8px;
        border:1px solid var(--border); object-fit:cover; display:block; }
.placeholder { display:grid; place-items:center; background:var(--surface);
        font-size:2.5rem; font-weight:700; color:var(--muted); }
.card .title { display:block; margin-top:.5rem; font-weight:600; }
.card .author { display:block; color:var(--muted); font-size:.9rem; }
.empty { color:var(--muted); }
.detail { display:grid; gap:1.5rem; grid-template-columns:200px 1fr; align-items:start; }
@media (max-width:560px){ .detail{ grid-template-columns:1fr; } }
.detail .cover, .detail .placeholder { width:200px; }
.feedrow { display:flex; gap:.5rem; flex-wrap:wrap; margin:.5rem 0 0; }
.feedrow input { flex:1 1 260px; min-width:0; padding:.55rem .7rem;
        border:1px solid var(--border); border-radius:6px; font:inherit; color:var(--text); }
button.copy { padding:.55rem .9rem; border:0; border-radius:6px; font:inherit;
        font-weight:600; background:var(--accent); color:var(--accent-text); cursor:pointer; }
.qr { margin-top:1rem; width:180px; }
.qr svg { width:180px; height:180px; display:block; border:1px solid var(--border);
        border-radius:8px; background:#fff; }
.cta { display:inline-block; margin:.25rem 0 1rem; padding:.75rem 1.1rem; border-radius:8px;
        background:var(--accent); color:var(--accent-text); text-decoration:none; font-weight:600; }
.qrcap { margin:.4rem 0 0; color:var(--muted); font-size:.85rem; max-width:180px; }
.subscribe { max-width:640px; }
.subscribe .subcover { width:96px; margin-bottom:.25rem; }
.subscribe h1 { margin:.25rem 0; }
.lead { color:var(--muted); margin:.25rem 0 1.25rem; }
.applist { list-style:none; margin:1rem 0 0; padding:0; display:grid; gap:.6rem;
        grid-template-columns:repeat(auto-fill,minmax(210px,1fr)); }
.applist li { margin:0; }
.appbtn { display:block; width:100%; text-align:center; padding:.8rem 1rem; border-radius:8px;
        background:var(--accent); color:var(--accent-text); text-decoration:none; font-weight:600; }
.appbtn:hover { filter:brightness(1.08); }
.qrpanel { margin-top:1.25rem; border:1px solid var(--border); border-radius:8px;
        background:var(--surface); padding:.75rem 1rem 1rem; }
.qrpanel h2 { margin:0; font-size:1.05rem; }
.qrhint { color:var(--muted); font-size:.9rem; margin:.25rem 0 .75rem; }
.qraccordion { list-style:none; margin:0; padding:0; display:grid; gap:.5rem; }
.qraccordion li { margin:0; }
/* One collapsible section per app: a collapsed section shows no scannable code, so
   only the expanded app's QR is on screen — a phone camera can't lock onto a neighbour's. */
.qrapp { border:1px solid var(--border); border-radius:8px; background:var(--bg); padding:0 1rem; }
.qrapp summary { cursor:pointer; font-weight:600; padding:.75rem 0; }
.qrapp[open] { padding-bottom:1rem; }
.appqr { margin:.25rem 0 0; }
.appqr svg { width:180px; height:180px; display:block; background:#fff;
        border:1px solid var(--border); border-radius:6px; }
.qrapplink { margin:.6rem 0 0; }
.manual { margin-top:1.75rem; padding:1rem 1.25rem; background:var(--surface);
        border:1px solid var(--border); border-radius:8px; }
.manual h2 { margin-top:0; font-size:1.05rem; }
.manual .note { color:var(--muted); font-size:.85rem; margin:.5rem 0 0; }
.private { margin-top:1.5rem; padding:1rem 1.25rem; background:var(--surface);
        border:1px solid var(--border); border-radius:8px; }
.private h2 { margin-top:0; font-size:1.05rem; }
.private > p { margin:.25rem 0 1rem; color:var(--muted); }
.privrow { display:flex; gap:.6rem; align-items:center; flex-wrap:wrap; margin:.4rem 0; }
.privrow form { margin:0; }
.privrow .note { color:var(--muted); font-size:.85rem; }
button.regen { padding:.5rem .85rem; border:1px solid var(--danger); border-radius:6px;
        font:inherit; font-weight:600; background:transparent; color:var(--danger); cursor:pointer; }
button.regen:hover { background:var(--danger); color:#fff; }
.back { display:inline-block; margin-bottom:1rem; }
/* Detail/subscribe titles: the browser-default h1 (~2em) dominates on a phone and
   a long slug-title can overflow — cap the size and allow wrapping. */
.detail h1, .subscribe h1 { font-size:1.6rem; line-height:1.25; overflow-wrap:anywhere; margin:.25rem 0 .5rem; }
@media (max-width:480px){
  main { padding:1.25rem 1rem; }
  .detail h1, .subscribe h1 { font-size:1.35rem; }
  .grid { gap:1rem; grid-template-columns:repeat(auto-fill,minmax(130px,1fr)); }
}
"#;

/// Tiny clipboard helper. The feed input works without JS (selectable); this
/// only upgrades the copy button.
const COPY_JS: &str = r#"
document.addEventListener('click', function (e) {
  var b = e.target.closest('button.copy'); if (!b) return;
  var input = document.getElementById(b.getAttribute('data-target')); if (!input) return;
  input.select();
  navigator.clipboard && navigator.clipboard.writeText(input.value).then(function () {
    var t = b.textContent; b.textContent = 'Copied'; setTimeout(function(){ b.textContent = t; }, 1500);
  });
});
"#;

/// The visitor's chosen colour theme, decoded from the `theme` cookie. `System`
/// (the default, no cookie) follows the OS via `prefers-color-scheme`; `Light` and
/// `Dark` force it. Rendered as the `data-theme` attribute the CSS keys off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    /// Follow the operating system (no `data-theme` attribute).
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// Decode a `theme` cookie / picker `mode` value; anything else is `System`
    /// (so a stale or garbage cookie safely falls back to following the OS).
    pub fn parse(value: &str) -> Self {
        match value {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }

    /// The `data-theme` attribute value, or `None` for `System` (no attribute → the
    /// `prefers-color-scheme` media query applies).
    fn attr(self) -> Option<&'static str> {
        match self {
            Theme::System => None,
            Theme::Light => Some("light"),
            Theme::Dark => Some("dark"),
        }
    }

    /// The value to persist in the `theme` cookie, or `None` for `System` (which
    /// clears the cookie so the browser reverts to the OS preference).
    pub fn cookie_value(self) -> Option<&'static str> {
        self.attr()
    }
}

/// The no-JS theme picker: a small form of submit buttons in the header. Each
/// button's `formaction` posts to `/theme/{mode}`; the server sets the cookie and
/// redirects back. The active theme is marked with `aria-pressed` (also styled).
fn theme_picker(current: Theme) -> Markup {
    html! {
        form.themepicker method="post" aria-label="Colour theme" {
            @for &(mode, value, label) in &[
                (Theme::System, "system", "Auto"),
                (Theme::Light, "light", "Light"),
                (Theme::Dark, "dark", "Dark"),
            ] {
                button formaction=(format!("/theme/{value}"))
                    aria-pressed=(mode == current) { (label) }
            }
        }
    }
}

/// Wrap page `body` content in the full HTML document shell for the given `theme`.
fn page(title: &str, theme: Theme, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" data-theme=[theme.attr()] {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.site {
                    h1 { a href="/" { "Podspine" } }
                    (theme_picker(theme))
                }
                (body)
            }
        }
    }
}

/// A cover `<img>` when available, else an accessible lettered placeholder.
/// `id` is the book's capability `feed_id` — covers are served at
/// `/cover/{feed_id}`, never the guessable slug.
fn cover(id: &str, title: &str, has_cover: bool, class: &str) -> Markup {
    let initial = title
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    html! {
        @if has_cover {
            img class=(class) src=(format!("/cover/{id}")) alt=(format!("Cover of {title}")) loading="lazy";
        } @else {
            div class=(format!("{class} placeholder")) role="img" aria-label=(format!("No cover art for {title}")) {
                span aria-hidden="true" { (initial) }
            }
        }
    }
}

/// The home page: a grid of books, each linking to its detail page.
pub fn index_page(books: &[BookCard], theme: Theme) -> Markup {
    page(
        "Podspine",
        theme,
        html! {
            main {
                @if books.is_empty() {
                    p.empty { "No audiobooks found in your library yet." }
                } @else {
                    ul.grid {
                        @for b in books {
                            li.card {
                                a href=(format!("/book/{}", b.slug)) {
                                    (cover(&b.feed_id, &b.title, b.has_cover, "cover"))
                                    span.title { (b.title) }
                                    @if let Some(a) = &b.author { span.author { (a) } }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// A holding page shown at `GET /` while the initial library scan runs (issue
/// 159), so a first boot never surfaces a proxy 502 or a misleading empty grid.
/// Self-refreshes via a `<meta http-equiv="refresh">` (no JavaScript), so it
/// becomes the normal book list on its own once the scan finishes. It reuses the
/// page shell but needs its own `<head>` (the refresh directive and a distinct
/// title), so it is built directly rather than through [`page`].
pub fn scanning_page(theme: Theme) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" data-theme=[theme.attr()] {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta http-equiv="refresh" content="5";
                title { "Scanning… · Podspine" }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.site { h1 { a href="/" { "Podspine" } } }
                main {
                    p.empty { "Scanning your library…" }
                    p.empty {
                        "This can take a minute on first start while episodes are prepared. "
                        "This page refreshes on its own — your books will appear here when it's done."
                    }
                }
            }
        }
    }
}

/// A book's detail page: cover, copy-feed-URL, scannable QR, and how-to panel.
pub fn book_page(book: &BookDetail, theme: Theme) -> Markup {
    // The QR encodes the /subscribe helper page, not the raw feed: a raw RSS URL
    // scanned by the iOS Camera opens Safari to bare XML ("can't open"), whereas
    // the helper page offers real per-app "Open in…" deep links.
    let qr = qr_svg(&book.subscribe_url);
    page(
        &book.title,
        theme,
        html! {
            main {
                a.back href="/" { "← All books" }
                div.detail {
                    (cover(&book.feed_id, &book.title, book.has_cover, "cover"))
                    div {
                        h1 { (book.title) }
                        @if let Some(a) = &book.author { p.author { (a) } }
                        p { (book.episode_count) " episodes" }

                        a.cta href=(format!("/subscribe/{}", book.feed_id)) {
                            "＋ Add to a podcast app" }

                        div.qr {
                            figure role="img" aria-label="QR code that opens the add-to-app page" {
                                (PreEscaped(qr))
                            }
                            figcaption.qrcap { "Scan to open the add-to-app page on your phone" }
                        }

                        label for="feed-url" { "Podcast feed URL" }
                        div.feedrow {
                            input #feed-url type="text" readonly value=(book.feed_url)
                                aria-label="Podcast feed URL" onclick="this.select()";
                            button.copy type="button" data-target="feed-url" { "Copy" }
                        }

                        (private_panel(&book.slug))
                    }
                }
            }
            script { (PreEscaped(COPY_JS)) }
        },
    )
}

/// The "private link" controls: regenerate the capability URL (leak recovery).
/// A plain `POST` form — no JS required. `slug` is the (LAN-only) UI key the
/// route acts on; `feed_id` never appears in the action URL. Feeds are always
/// kept out of podcast directories, so there's nothing to toggle.
fn private_panel(slug: &str) -> Markup {
    html! {
        section.private {
            h2 { "🔒 Private link" }
            p {
                "Anyone with the URL above can subscribe — treat it like a password. "
                "If it leaks, regenerate to replace it. This feed is kept out of "
                "podcast directories."
            }
            div.privrow {
                form method="post" action=(format!("/book/{slug}/regenerate")) {
                    button.regen type="submit" { "Regenerate link" }
                }
                span.note { "Replaces the URL above — the current link stops working immediately." }
            }
        }
    }
}

/// A podcast app + its "subscribe to this feed" deep link. Formats per
/// [nathangathright/podcast-platform-links]; `feedURL` is the feed URL WITHOUT the
/// http(s):// scheme, except Overcast which takes the full URL, percent-encoded.
pub struct AppLink {
    pub name: &'static str,
    pub url: String,
}

/// Strip the URL scheme (`http://` / `https://`) — the `feedURL` form most app
/// subscribe schemes expect.
fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
}

/// Percent-encode per RFC 3986 (unreserved chars pass through) — for embedding the
/// feed URL as a query parameter (Overcast's `?url=`). Avoids a url-encoding dep.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Per-app "subscribe to this feed" deep links for the popular podcast apps.
/// Tapping one on a phone hands off to the installed app; the same URL as a QR
/// lets a desktop viewer scan straight into their phone's app.
pub fn subscribe_links(feed_url: &str) -> Vec<AppLink> {
    let bare = strip_scheme(feed_url);
    vec![
        AppLink {
            name: "Apple Podcasts",
            url: format!("podcast://{bare}"),
        },
        AppLink {
            name: "Overcast",
            url: format!(
                "overcast://x-callback-url/add?url={}",
                percent_encode(feed_url)
            ),
        },
        AppLink {
            name: "Pocket Casts",
            url: format!("pktc://subscribe/{bare}"),
        },
        AppLink {
            name: "Castro",
            url: format!("castros://subscribe/{bare}"),
        },
        AppLink {
            name: "AntennaPod",
            url: format!("antennapod-subscribe://{bare}"),
        },
        AppLink {
            name: "Podcast Addict",
            url: format!("podcastaddict://{bare}"),
        },
    ]
}

/// The `/subscribe/{feed_id}` helper page: big per-app "Open in…" buttons (deep
/// links), each app's QR code tucked behind its own `<details>` expand — a shared
/// `name` makes them an exclusive accordion, so only one code is ever on screen to
/// scan (desktop→phone handoff) — and a copy-the-URL fallback. This is what the
/// book-page QR points at, so an iOS Camera scan lands on real app links instead of
/// raw feed XML.
pub fn subscribe_page(book: &BookDetail, theme: Theme) -> Markup {
    let apps = subscribe_links(&book.feed_url);
    page(
        &format!("Add \u{201c}{}\u{201d} to a podcast app", book.title),
        theme,
        html! {
            main {
                a.back href=(format!("/book/{}", book.slug)) { "← Back to book" }
                div.subscribe {
                    (cover(&book.feed_id, &book.title, book.has_cover, "cover subcover"))
                    h1 { (book.title) }
                    @if let Some(a) = &book.author { p.author { (a) } }
                    p.lead { "Tap your app to subscribe." }

                    ul.applist {
                        @for app in &apps {
                            li { a.appbtn href=(app.url) { "Open in " (app.name) } }
                        }
                    }

                    // One collapsible <details> per app, not a grid of all codes at
                    // once: with every QR on screen a phone camera readily locks onto
                    // an adjacent app's code and opens the wrong app. A collapsed
                    // section shows nothing scannable, so expanding one puts exactly
                    // that app's code on screen. Native <details> — no JS. For the
                    // desktop→phone case: expand an app and scan to open it on a phone.
                    section.qrpanel {
                        h2 { "Scan a code from another device" }
                        p.qrhint { "On a computer? Expand an app and point your phone's camera at just that code." }
                        ul.qraccordion {
                            @for app in &apps {
                                li {
                                    // Shared `name` makes these an exclusive accordion
                                    // group (like radio buttons): opening one closes
                                    // any other — enforcing one-code-on-screen without
                                    // JS. Browsers without `<details name>` support just
                                    // treat them as independent (still collapsed by
                                    // default), so it degrades safely.
                                    details.qrapp name="podcatcher-qr" {
                                        summary { (app.name) }
                                        figure.appqr role="img"
                                            aria-label=(format!("QR code to open in {}", app.name)) {
                                            (PreEscaped(qr_svg_sized(&app.url, 180)))
                                        }
                                        p.qrapplink { a href=(app.url) { "Open in " (app.name) } }
                                    }
                                }
                            }
                        }
                    }

                    section.manual {
                        h2 { "Add by URL" }
                        p { "Using a different app? Paste this feed URL into its \u{201c}add by URL\u{201d} field:" }
                        div.feedrow {
                            input #feed-url type="text" readonly value=(book.feed_url)
                                aria-label="Podcast feed URL" onclick="this.select()";
                            button.copy type="button" data-target="feed-url" { "Copy" }
                        }
                        p.note { "This link is private — treat it like a password." }
                    }
                }
            }
            script { (PreEscaped(COPY_JS)) }
        },
    )
}

/// Render `data` as an inline SVG QR code (black on white) at the default size.
fn qr_svg(data: &str) -> String {
    qr_svg_sized(data, 180)
}

/// Render `data` as an inline SVG QR code at `px` minimum size. Empty string if
/// the data can't be encoded (never panics on the request path).
fn qr_svg_sized(data: &str, px: u32) -> String {
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(px, px)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(slug: &str, title: &str, has_cover: bool) -> BookCard {
        BookCard {
            slug: slug.into(),
            feed_id: format!("cap-{slug}"),
            title: title.into(),
            author: Some("An Author".into()),
            has_cover,
        }
    }

    #[test]
    fn an_unencodable_qr_yields_an_empty_string_not_a_panic() {
        // QR capacity tops out around 2953 bytes; a longer base_url would
        // otherwise panic on a page request. Empty string = the page still
        // renders, just without a code.
        let too_long = "u".repeat(4000);
        assert!(qr_svg_sized(&too_long, 180).is_empty());
        // Sanity: a normal URL still produces a code, so the assert above is
        // testing the capacity limit and not a broken renderer.
        assert!(qr_svg_sized("http://podspine.test/feed/abc.xml", 180).contains("<svg"));
    }

    #[test]
    fn index_lists_books_with_links_and_cover_alt() {
        let books = [
            card("dracula", "Dracula", true),
            card("solaris", "Solaris", false),
        ];
        let html = index_page(&books, Theme::System).into_string();
        assert!(html.contains("href=\"/book/dracula\""));
        assert!(html.contains("href=\"/book/solaris\""));
        // Cover present -> img with alt; absent -> labelled placeholder.
        // Covers are served by capability id, not the slug.
        assert!(html.contains("src=\"/cover/cap-dracula\""));
        assert!(html.contains("alt=\"Cover of Dracula\""));
        assert!(html.contains("aria-label=\"No cover art for Solaris\""));
    }

    #[test]
    fn scanning_page_holds_and_self_refreshes() {
        let html = scanning_page(Theme::System).into_string();
        // A clear scanning state, not an empty book grid...
        assert!(html.contains("Scanning your library…"));
        assert!(!html.contains("No audiobooks found"));
        // ...that reloads itself (no JS) so it flips to the list when the scan ends.
        assert!(html.contains("<meta http-equiv=\"refresh\" content=\"5\">"));
    }

    #[test]
    fn empty_library_shows_a_message() {
        let html = index_page(&[], Theme::System).into_string();
        assert!(html.contains("No audiobooks found"));
        assert!(!html.contains("<ul"));
    }

    #[test]
    fn theme_picker_reflects_choice() {
        // System (the default): no data-theme on <html>, so the page follows the OS.
        // (The CSS names data-theme in selectors, so assert on the tag, not a substring.)
        let sys = index_page(&[], Theme::System).into_string();
        assert!(
            sys.contains("<html lang=\"en\">"),
            "System sets no data-theme"
        );
        assert!(sys.contains("themepicker"), "the picker is always present");
        // The active option is marked on its button (assert on the label so the CSS
        // selector `[aria-pressed="true"]` doesn't count as a match). Auto for System.
        assert!(sys.contains("aria-pressed=\"true\">Auto</button>"));

        // Dark: the <html> tag carries data-theme="dark" and posts to /theme/{mode}.
        let dark = index_page(&[], Theme::Dark).into_string();
        assert!(dark.contains("<html lang=\"en\" data-theme=\"dark\">"));
        assert!(dark.contains("formaction=\"/theme/dark\""));
        assert!(
            dark.contains("aria-pressed=\"true\">Dark</button>"),
            "Dark active"
        );
        assert!(
            !dark.contains("aria-pressed=\"true\">Auto</button>"),
            "Auto not active in dark"
        );
    }

    #[test]
    fn style_supports_os_dark_mode() {
        // Auto dark mode is an OS-preference override of the same --* variables,
        // with color-scheme so native controls follow. No JS/toggle.
        assert!(STYLE.contains("color-scheme: light dark"));
        assert!(STYLE.contains("@media (prefers-color-scheme: dark)"));
        // The dark override redefines the palette variables.
        assert!(STYLE.contains("--bg:#18181b"));
    }

    fn detail() -> BookDetail {
        BookDetail {
            slug: "dracula".into(),
            feed_id: "Xk9mQ2vP7nR4tB1cY6wZ8a".into(),
            title: "Dracula".into(),
            author: Some("Frank Herbert".into()),
            has_cover: true,
            feed_url: "http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml".into(),
            subscribe_url: "http://host:8080/subscribe/Xk9mQ2vP7nR4tB1cY6wZ8a".into(),
            episode_count: 12,
        }
    }

    #[test]
    fn book_page_has_exact_feed_url_and_qr() {
        let html = book_page(&detail(), Theme::System).into_string();
        // The copy input carries the exact working (capability) URL.
        assert!(html.contains("value=\"http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml\""));
        assert!(html.contains("12 episodes"));
        // QR rendered as inline SVG; it now opens the /subscribe helper page (not
        // the raw feed), so an iOS Camera scan lands on real app links.
        assert!(html.contains("<svg"));
        assert!(html.contains("aria-label=\"QR code that opens the add-to-app page\""));
        assert!(html.contains("href=\"/subscribe/Xk9mQ2vP7nR4tB1cY6wZ8a\""));
        // The regenerate control posts to the slug-keyed route (feed_id never in
        // the action URL).
        assert!(html.contains("action=\"/book/dracula/regenerate\""));
        assert!(html.contains("Regenerate link"));
    }

    #[test]
    fn subscribe_page_has_per_app_deep_links_and_qrs() {
        let html = subscribe_page(&detail(), Theme::System).into_string();
        // Apple Podcasts: podcast:// + feed URL WITHOUT the scheme.
        assert!(html.contains("href=\"podcast://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml\""));
        assert!(html.contains("pktc://subscribe/host:8080/feed/"));
        assert!(html.contains("antennapod-subscribe://host:8080/feed/"));
        // Overcast takes the FULL url, percent-encoded, as a query param.
        assert!(
            html.contains("overcast://x-callback-url/add?url=http%3A%2F%2Fhost%3A8080%2Ffeed%2F")
        );
        // Each app also renders a QR (>=6 apps -> >=6 inline SVGs)...
        assert!(html.matches("<svg").count() >= 6);
        // ...each collapsed behind its OWN native <details> (issue 160): one QR per
        // app, so a collapsed section shows nothing scannable and only the expanded
        // app's code is on screen. >=6 apps -> >=6 <details>, each summarised by the
        // app name rather than one shared "scan a code" panel.
        assert!(html.matches("<details").count() >= 6);
        assert!(html.contains("<summary>Apple Podcasts</summary>"));
        assert!(html.contains("<summary>Overcast</summary>"));
        // They share a `name`, forming an exclusive accordion (opening one closes
        // the others) so no two QRs can be on screen at once — no JS.
        assert!(html.matches("name=\"podcatcher-qr\"").count() >= 6);
        // The per-app deep link stays available inside each expanded section.
        assert!(html.contains(">Open in Apple Podcasts</a>"));
        // Manual paste fallback still present.
        assert!(html.contains("value=\"http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml\""));
    }

    #[test]
    fn subscribe_links_cover_major_apps_and_strip_scheme() {
        let links = subscribe_links("https://ex.com/feed/abc.xml");
        let apple = links.iter().find(|l| l.name == "Apple Podcasts").unwrap();
        // https scheme stripped for the feedURL-style apps.
        assert_eq!(apple.url, "podcast://ex.com/feed/abc.xml");
        // Overcast keeps the full URL (with scheme), percent-encoded.
        let oc = links.iter().find(|l| l.name == "Overcast").unwrap();
        assert!(oc.url.contains("url=https%3A%2F%2Fex.com%2Ffeed%2Fabc.xml"));
        for app in ["Pocket Casts", "Castro", "AntennaPod", "Podcast Addict"] {
            assert!(links.iter().any(|l| l.name == app), "missing {app}");
        }
    }

    #[test]
    fn qr_encodes_without_panicking() {
        assert!(qr_svg("http://x/feed/a.xml").contains("<svg"));
        // Even an empty string is encodable; never panics.
        let _ = qr_svg("");
    }

    #[test]
    fn markup_escapes_untrusted_title() {
        let books = [card("x", "<script>alert(1)</script>", false)];
        let html = index_page(&books, Theme::System).into_string();
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
