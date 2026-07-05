//! `ui` — `maud` server-rendered pages: book grid (cover, title), per-book
//! "copy feed URL" + QR (inline SVG), and a per-app "how to add this" panel.
//! Templates compile into the binary (no runtime template files). No player.
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
    /// The exact, working capability feed URL (what "copy" yields, QR encodes).
    pub feed_url: String,
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
.howto { margin-top:1.5rem; padding:1rem 1.25rem; background:var(--surface);
        border:1px solid var(--border); border-radius:8px; }
.howto h2 { margin-top:0; font-size:1.05rem; }
.howto ul { margin:0; padding-left:1.25rem; }
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
    let qr = qr_svg(&book.feed_url);
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

                        label for="feed-url" { "Podcast feed URL" }
                        div.feedrow {
                            input #feed-url type="text" readonly value=(book.feed_url)
                                aria-label="Podcast feed URL" onclick="this.select()";
                            button.copy type="button" data-target="feed-url" { "Copy" }
                        }

                        div.qr {
                            figure role="img" aria-label="QR code linking to the podcast feed URL" {
                                (PreEscaped(qr))
                            }
                        }

                        (private_panel(&book.slug))

                        (howto(&book.feed_url))
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

/// A short per-app "how to add this" panel. The full app-by-app import guide is
/// Task 3.6; this is the inline quick-start.
fn howto(feed_url: &str) -> Markup {
    html! {
        section.howto {
            h2 { "How to add this to your podcast app" }
            ul {
                li { "Apple Podcasts: Library → ⋯ / File → " em { "Add a Show by URL…" } " → paste the feed URL." }
                li { "Pocket Casts: Profile → Add Podcast → " em { "Add URL" } " → paste the feed URL." }
                li { "Overcast: " em { "＋" } " → Add URL → paste the feed URL." }
                li { "AntennaPod: Add Podcast → " em { "Add podcast by RSS address" } " → paste the feed URL." }
            }
            p { "Or scan the QR code above. Feed URL: " code { (feed_url) } }
        }
    }
}

/// Render `data` as an inline SVG QR code (black on white). Empty string if the
/// data can't be encoded (never panics on the request path).
fn qr_svg(data: &str) -> String {
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(180, 180)
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

    #[test]
    fn book_page_has_exact_feed_url_and_qr() {
        let book = BookDetail {
            slug: "dune".into(),
            feed_id: "Xk9mQ2vP7nR4tB1cY6wZ8a".into(),
            title: "Dune".into(),
            author: Some("Frank Herbert".into()),
            has_cover: true,
            feed_url: "http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml".into(),
            episode_count: 12,
        };
        let html = book_page(&book).into_string();
        // The copy input carries the exact working (capability) URL, and it
        // appears in the panel.
        assert!(html.contains("value=\"http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml\""));
        assert!(html.contains("<code>http://host:8080/feed/Xk9mQ2vP7nR4tB1cY6wZ8a.xml</code>"));
        assert!(html.contains("12 episodes"));
        // QR rendered as inline SVG, labelled for AT.
        assert!(html.contains("<svg"));
        assert!(html.contains("aria-label=\"QR code linking to the podcast feed URL\""));
        // The regenerate control posts to the slug-keyed route (feed_id never in
        // the action URL).
        assert!(html.contains("action=\"/book/dune/regenerate\""));
        assert!(html.contains("Regenerate link"));
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
