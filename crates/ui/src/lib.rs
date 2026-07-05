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
const STYLE: &str = r#"
:root { --bg:#ffffff; --surface:#f4f4f5; --border:#d4d4d8; --text:#18181b;
        --muted:#52525b; --accent:#1d4ed8; --accent-text:#ffffff; --danger:#b91c1c; }
* { box-sizing:border-box; }
body { margin:0; font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
       color:var(--text); background:var(--bg); }
a { color:var(--accent); }
:focus-visible { outline:3px solid var(--accent); outline-offset:2px; border-radius:4px; }
header.site { padding:1rem 1.25rem; border-bottom:1px solid var(--border); }
header.site h1 { margin:0; font-size:1.25rem; }
header.site a { text-decoration:none; color:var(--text); }
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
        background:var(--surface); padding:.25rem 1rem; }
.qrpanel summary { cursor:pointer; font-weight:600; padding:.75rem 0; }
.qrhint { color:var(--muted); font-size:.9rem; margin:.25rem 0 1rem; }
.qrgrid { list-style:none; margin:0; padding:0 0 .75rem; display:grid; gap:1rem;
        grid-template-columns:repeat(auto-fill,minmax(140px,1fr)); }
.qrcard { display:flex; flex-direction:column; align-items:center; gap:.4rem; }
.qrname { font-size:.85rem; color:var(--muted); text-align:center; }
.appqr { margin:0; }
.appqr svg { width:120px; height:120px; display:block; background:#fff;
        border:1px solid var(--border); border-radius:6px; }
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

/// Wrap page `body` content in the full HTML document shell.
fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.site { h1 { a href="/" { "Podspine" } } }
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
pub fn index_page(books: &[BookCard]) -> Markup {
    page(
        "Podspine",
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

/// A book's detail page: cover, copy-feed-URL, scannable QR, and how-to panel.
pub fn book_page(book: &BookDetail) -> Markup {
    // The QR encodes the /subscribe helper page, not the raw feed: a raw RSS URL
    // scanned by the iOS Camera opens Safari to bare XML ("can't open"), whereas
    // the helper page offers real per-app "Open in…" deep links.
    let qr = qr_svg(&book.subscribe_url);
    page(
        &book.title,
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
/// links), the per-app QR codes tucked behind a `<details>` expand (desktop→phone
/// handoff), and a copy-the-URL fallback. This is what the book-page QR points at,
/// so an iOS Camera scan lands on real app links instead of raw feed XML.
pub fn subscribe_page(book: &BookDetail) -> Markup {
    let apps = subscribe_links(&book.feed_url);
    page(
        &format!("Add \u{201c}{}\u{201d} to a podcast app", book.title),
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

                    // QRs collapsed by default (6 codes is visually noisy) — a native
                    // <details> keeps them one tap away and needs no JS. For the
                    // desktop→phone case: scan a code to open that app on a phone.
                    details.qrpanel {
                        summary { "Scan a code from another device" }
                        p.qrhint { "On a computer? Point your phone's camera at a code to open that app." }
                        ul.qrgrid {
                            @for app in &apps {
                                li.qrcard {
                                    span.qrname { (app.name) }
                                    figure.appqr role="img"
                                        aria-label=(format!("QR code to open in {}", app.name)) {
                                        (PreEscaped(qr_svg_sized(&app.url, 120)))
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
    fn index_lists_books_with_links_and_cover_alt() {
        let books = [
            card("dune", "Dune", true),
            card("solaris", "Solaris", false),
        ];
        let html = index_page(&books).into_string();
        assert!(html.contains("href=\"/book/dune\""));
        assert!(html.contains("href=\"/book/solaris\""));
        // Cover present -> img with alt; absent -> labelled placeholder.
        // Covers are served by capability id, not the slug.
        assert!(html.contains("src=\"/cover/cap-dune\""));
        assert!(html.contains("alt=\"Cover of Dune\""));
        assert!(html.contains("aria-label=\"No cover art for Solaris\""));
    }

    #[test]
    fn empty_library_shows_a_message() {
        let html = index_page(&[]).into_string();
        assert!(html.contains("No audiobooks found"));
        assert!(!html.contains("<ul"));
    }

    fn detail() -> BookDetail {
        BookDetail {
            slug: "dune".into(),
            feed_id: "Xk9mQ2vP7nR4tB1cY6wZ8a".into(),
            title: "Dune".into(),
            author: Some("Frank Herbert".into()),
            has_cover: true,
            feed_url: "http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml".into(),
            subscribe_url: "http://host:8080/subscribe/Xk9mQ2vP7nR4tB1cY6wZ8a".into(),
            episode_count: 12,
        }
    }

    #[test]
    fn book_page_has_exact_feed_url_and_qr() {
        let html = book_page(&detail()).into_string();
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
        assert!(html.contains("action=\"/book/dune/regenerate\""));
        assert!(html.contains("Regenerate link"));
    }

    #[test]
    fn subscribe_page_has_per_app_deep_links_and_qrs() {
        let html = subscribe_page(&detail()).into_string();
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
        // ...but the QRs are collapsed behind a native <details> to cut clutter.
        assert!(html.contains("<details"));
        assert!(html.contains("<summary>Scan a code from another device</summary>"));
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
        let html = index_page(&books).into_string();
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
