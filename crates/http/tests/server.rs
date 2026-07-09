//! HTTP integration tests: synthesize a book, scan it, and drive the router with
//! `oneshot` requests. Existence-gated on ffmpeg (skips if absent).

use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use podspine_http::{AppState, router};
use podspine_index::Index;
use podspine_scanner::{scan_book, scan_book_as};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn synth_three_chapters(dir: &Path) -> PathBuf {
    let meta = dir.join("meta.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30000\ntitle=Three\n",
    )
    .unwrap();
    let input = dir.join("synthetic.m4a");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=30",
            "-i",
        ])
        .arg(&meta)
        .args(["-map_metadata", "1", "-map", "0:a", "-c:a", "aac"])
        .arg(&input)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg synth failed");
    input
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// Synthesize an AAC file with an embedded (attached-picture) cover.
fn synth_with_cover(dir: &Path) -> PathBuf {
    let input = dir.join("cover.m4a");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=6",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=120x120:d=0.1",
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-frames:v",
            "1",
            "-c:a",
            "aac",
            "-c:v",
            "mjpeg",
            "-disposition:v:0",
            "attached_pic",
        ])
        .arg(&input)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg cover synth failed");
    input
}

#[tokio::test]
async fn serves_cover_when_present() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = std::env::temp_dir().join("podspine-http-cover");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_with_cover(&dir);
    let book = scan_book(&input, &data, &index).unwrap();
    let feed_id = book.feed_id.clone();
    assert!(
        book.cover_path.is_some(),
        "cover should have been extracted"
    );

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        None,
        false,
        None,
        None,
    );
    let app = router(state);

    // Covers are served by capability id, not the slug.
    let resp = app
        .oneshot(
            Request::get(format!("/cover/{feed_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    assert!(!body_bytes(resp).await.is_empty(), "cover bytes served");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn saver_mode_regenerates_a_chapter_on_demand() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = std::env::temp_dir().join("podspine-http-saver");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_three_chapters(&dir);
    // Ingest in `saver` mode: sizes recorded, split files deleted.
    let book = scan_book_as(&input, "saverbook", &data, &index, false, true, false).unwrap();
    let feed_id = book.feed_id.clone();

    let eps = index.episodes_for_book(&book.id).unwrap();
    assert_eq!(eps.len(), 3);
    let ch1 = eps[0].file_path.clone();
    assert!(
        !std::path::Path::new(&ch1).exists(),
        "saver ingest leaves no split file on disk"
    );
    let recorded_len = eps[0].byte_length;

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        None,
        true,
        None,
        None,
    );
    let app = router(state);

    // First request: the file is missing, so it's regenerated and served.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    assert_eq!(
        bytes.len() as i64,
        recorded_len,
        "served body matches the recorded enclosure length"
    );
    assert!(
        std::path::Path::new(&ch1).exists(),
        "regenerated chapter is now cached on disk"
    );

    // A Range request against the now-cached file yields a 206 partial.
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .header(header::RANGE, "bytes=0-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(resp).await.len(), 10, "range served 10 bytes");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn saver_cache_evicts_over_the_size_cap() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = std::env::temp_dir().join("podspine-http-saver-evict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_three_chapters(&dir);
    let book = scan_book_as(&input, "evictbook", &data, &index, false, true, false).unwrap();
    let feed_id = book.feed_id.clone();
    let book_out = data.join("books").join(&book.id);

    // A 1-byte cap forces eviction of everything but the file just served.
    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        None,
        true,
        Some(1),
        None,
    );
    let app = router(state);

    for n in [1u32, 2] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/audio/{feed_id}/{n}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = body_bytes(resp).await;
    }

    // After serving ch2 with a 1-byte cap, ch1 must have been evicted — leaving
    // exactly one cached chapter file.
    let cached = std::fs::read_dir(&book_out)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
        })
        .count();
    assert_eq!(cached, 1, "size cap keeps only the most-recent chapter");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn full_mode_missing_file_is_a_404_not_a_regeneration() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = std::env::temp_dir().join("podspine-http-fullmiss");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_three_chapters(&dir);
    let book = scan_book(&input, &data, &index).unwrap(); // full mode: files kept
    let feed_id = book.feed_id.clone();

    // Simulate a lost split file. In `full` mode this must 404, never regenerate.
    let eps = index.episodes_for_book(&book.id).unwrap();
    std::fs::remove_file(&eps[0].file_path).unwrap();

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        None,
        false,
        None,
        None,
    );
    let app = router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Synthesize a chapterless AAC single file → served in place (Sprint 6.2).
fn synth_flat(dir: &Path) -> PathBuf {
    let input = dir.join("flat.m4a");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:duration=8",
            "-c:a",
            "aac",
        ])
        .arg(&input)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg synth failed");
    input
}

#[tokio::test]
async fn serves_whole_file_episode_in_place_from_the_library() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let base = std::env::temp_dir().join("podspine-http-inplace");
    let _ = std::fs::remove_dir_all(&base);
    // Library and data are SEPARATE trees, so "served from the library" is
    // provable (the file is not, and could not be, under the data dir).
    let library = base.join("library");
    let data = base.join("data");
    std::fs::create_dir_all(&library).unwrap();

    let index = Index::open_in_memory().unwrap();
    let input = synth_flat(&library);
    let book = scan_book(&input, &data, &index).unwrap();
    let feed_id = book.feed_id.clone();

    // Ingest recorded the episode as in-place (source under the library), and
    // copied nothing under the data dir.
    let eps = index.episodes_for_book(&book.id).unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(
        eps[0].source_path,
        input.canonicalize().unwrap().to_string_lossy()
    );
    let source_len = std::fs::metadata(&input).unwrap().len();
    assert_eq!(
        eps[0].byte_length as u64, source_len,
        "enclosure length = real source size"
    );
    assert!(
        !data.join("books").join(&book.id).join("001.m4a").exists(),
        "nothing copied under the data dir"
    );

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &library,
        None,
        false,
        None,
        None,
    );
    let app = router(state);

    // Full GET streams the whole library file back, byte-for-byte.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_bytes(resp).await;
    assert_eq!(body.len() as u64, source_len, "served the whole file");
    assert_eq!(
        body,
        std::fs::read(&input).unwrap(),
        "served the library bytes verbatim (no remux)"
    );

    // Range against the in-place file yields a 206 partial.
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .header(header::RANGE, "bytes=0-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(resp).await.len(), 10, "range served 10 bytes");

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn in_place_source_outside_the_library_is_a_404() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let base = std::env::temp_dir().join("podspine-http-inplace-escape");
    let _ = std::fs::remove_dir_all(&base);
    let library = base.join("library");
    let data = base.join("data");
    std::fs::create_dir_all(&library).unwrap();

    let index = Index::open_in_memory().unwrap();
    let input = synth_flat(&library);
    let book = scan_book(&input, &data, &index).unwrap();
    let feed_id = book.feed_id.clone();

    // Poison the row: point the episode's in-place source at a real file OUTSIDE
    // the library root. Canonicalize succeeds, but the library-root guard must
    // still reject it — a poisoned/traversing source path never escapes.
    let outside = base.join("outside.m4a");
    std::fs::copy(&input, &outside).unwrap();
    let mut ep = index.episodes_for_book(&book.id).unwrap()[0].clone();
    ep.source_path = outside.to_string_lossy().into_owned();
    ep.file_path = outside.to_string_lossy().into_owned();
    index.upsert_episode(&ep).unwrap();

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &library,
        None,
        false,
        None,
        None,
    );
    let app = router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "in-place source outside the library root is rejected"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn in_place_source_on_a_chaptered_episode_is_rejected() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let base = std::env::temp_dir().join("podspine-http-corrupt-inplace");
    let _ = std::fs::remove_dir_all(&base);
    let library = base.join("library");
    let data = base.join("data");
    std::fs::create_dir_all(&library).unwrap();

    let index = Index::open_in_memory().unwrap();
    // A chaptered container under the library, split into the data dir (full mode).
    let input = synth_three_chapters(&library);
    let book = scan_book(&input, &data, &index).unwrap();
    let feed_id = book.feed_id.clone();
    let mut ep = index.episodes_for_book(&book.id).unwrap()[0].clone();
    assert!(
        ep.source_path.is_empty(),
        "a chaptered episode has no source_path"
    );

    // Corrupt the row: mark chapter 1 as an in-place whole-file episode pointing at
    // the WHOLE container (`file_path == source_path`, under the library so the
    // library-root check passes). Its recorded byte_length is the chapter's size,
    // not the container's — so the whole-file size invariant must reject it rather
    // than serve the full container's bytes.
    let container = input.canonicalize().unwrap().to_string_lossy().into_owned();
    ep.source_path = container.clone();
    ep.file_path = container;
    assert_ne!(
        ep.byte_length as u64,
        std::fs::metadata(&input).unwrap().len(),
        "precondition: chapter length differs from the container size"
    );
    index.upsert_episode(&ep).unwrap();

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &library,
        None,
        false,
        None,
        None,
    );
    let app = router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a chaptered episode with a stray source_path must not serve the container"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn serves_a_remuxed_faststart_copy_on_demand() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let base = std::env::temp_dir().join("podspine-http-remux");
    let _ = std::fs::remove_dir_all(&base);
    let library = base.join("library");
    let data = base.join("data");
    std::fs::create_dir_all(&library).unwrap();

    let index = Index::open_in_memory().unwrap();
    // A non-faststart m4a (moov at end) under the library. Ingest with remux ON.
    let input = synth_flat(&library);
    let book = scan_book_as(&input, "remuxbook", &data, &index, false, false, true).unwrap();
    let feed_id = book.feed_id.clone();

    // Recorded as a faststart cache episode: file_path (cache) != source_path, and
    // the cache file was measured then deleted at ingest (regenerated on demand).
    let eps = index.episodes_for_book(&book.id).unwrap();
    assert_eq!(eps.len(), 1);
    assert!(eps[0].needs_faststart);
    assert_ne!(
        eps[0].source_path, eps[0].file_path,
        "remuxed: a cache copy, not in place"
    );
    assert!(std::path::Path::new(&eps[0].file_path).starts_with(&data));
    assert!(
        !std::path::Path::new(&eps[0].file_path).exists(),
        "cache file deleted at ingest"
    );
    let recorded_len = eps[0].byte_length;

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &library,
        None,
        false,
        None,
        None,
    );
    let app = router(state);

    // First request regenerates the remux and serves it; the body matches the
    // recorded enclosure length and is faststart (moov before mdat).
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_bytes(resp).await;
    assert_eq!(
        body.len() as i64,
        recorded_len,
        "served body matches the recorded enclosure length"
    );
    let find = |h: &[u8], n: &[u8]| h.windows(n.len()).position(|w| w == n);
    assert!(
        find(&body, b"moov") < find(&body, b"mdat"),
        "the served copy is faststart (moov before mdat)"
    );
    assert!(
        std::path::Path::new(&eps[0].file_path).exists(),
        "the remux is now cached on disk"
    );

    // A Range request against the cached remux yields a 206 partial.
    let resp = app
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .header(header::RANGE, "bytes=0-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(resp).await.len(), 10, "range served 10 bytes");

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn regenerate_rotates_the_capability() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = std::env::temp_dir().join("podspine-http-manage");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_three_chapters(&dir);
    let book = scan_book(&input, &data, &index).unwrap();
    let slug = book.slug.clone();
    let old_feed_id = book.feed_id.clone();

    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        None,
        false,
        None,
        None,
    );
    let app = router(state);

    // Every feed is always blocked from directories.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/feed/{old_feed_id}.xml"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let xml = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(xml.contains("<itunes:block>Yes</itunes:block>"));

    // CSRF guard: a cross-site POST (browser fetch-metadata) is refused (403).
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/book/{slug}/regenerate"))
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "CSRF rejected");

    // Regenerate -> the old capability URL 404s immediately.
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/book/{slug}/regenerate"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/feed/{old_feed_id}.xml"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "old capability URL dies after regenerate"
    );

    // The book page (by slug) now advertises a different capability URL.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/book/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let page = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(
        !page.contains(&old_feed_id),
        "book page shows the rotated feed_id, not the old one"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn serves_feed_and_range_audio() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = std::env::temp_dir().join("podspine-http-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");

    let index = Index::open_in_memory().unwrap();
    let input = synth_three_chapters(&dir);
    let book = scan_book(&input, &data, &index).unwrap();
    let slug = book.slug.clone(); // human key → UI routes
    let feed_id = book.feed_id.clone(); // capability → feed/audio/cover

    // The synthetic book has no embedded cover, so configure a feed-level
    // fallback to exercise the Task 3.4 default-cover path.
    let state = AppState::new(
        index,
        "http://test".to_string(),
        &data,
        &dir,
        Some("http://test/default-cover.png".to_string()),
        false,
        None,
        None,
    );
    let app = router(state);

    // healthz
    let resp = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"ok");

    // feed (by capability id)
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/feed/{feed_id}.xml"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/rss+xml; charset=utf-8"
    );
    // Capability feeds are never crawlable.
    assert_eq!(
        resp.headers().get("x-robots-tag").unwrap(),
        "noindex, nofollow"
    );
    let xml = String::from_utf8(body_bytes(resp).await).unwrap();
    assert_eq!(xml.matches("<item>").count(), 3);
    assert!(xml.contains("<itunes:duration>"));
    assert!(xml.contains(&format!("http://test/audio/{feed_id}/1")));
    // Feeds are always blocked from podcast directories.
    assert!(xml.contains("<itunes:block>Yes</itunes:block>"));
    // No embedded cover -> feed-level fallback image is emitted.
    assert!(xml.contains("<itunes:image"));
    assert!(xml.contains("http://test/default-cover.png"));

    // unknown id + missing .xml -> 404
    for uri in ["/feed/nope.xml".to_string(), format!("/feed/{feed_id}")] {
        let resp = app
            .clone()
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }

    // full audio GET -> 200 + Accept-Ranges
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
    // Regression: audio responses MUST carry a Content-Type. axum-range's Ranged
    // sets none, and a missing type makes Apple Podcasts / iOS refuse playback
    // ("can't be played on this device"). The synthetic .m4a AAC -> audio/mp4.
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "audio/mp4"
    );

    // Range request -> 206 + Content-Range, exactly 100 bytes
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/1"))
                .header(header::RANGE, "bytes=0-99")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert!(resp.headers().get(header::CONTENT_RANGE).is_some());
    // ...and the 206 keeps Content-Type too (applied on top of Ranged).
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "audio/mp4"
    );
    assert_eq!(body_bytes(resp).await.len(), 100);

    // unknown episode number -> 404
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{feed_id}/999"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // UI: home grid lists the book and links to its page.
    let resp = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let home = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(home.contains(&format!("/book/{slug}")));

    // UI: book page carries the exact working feed URL + an inline QR SVG.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/book/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let page = String::from_utf8(body_bytes(resp).await).unwrap();
    // The book page (by slug) shows the capability feed URL (by feed_id).
    assert!(page.contains(&format!("http://test/feed/{feed_id}.xml")));
    assert!(page.contains("<svg"));
    // ...and links to the /subscribe helper page (what the QR points at).
    assert!(page.contains(&format!("/subscribe/{feed_id}")));

    // Subscribe helper page (by capability id): per-app deep links + raw feed URL.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/subscribe/{feed_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sub = String::from_utf8(body_bytes(resp).await).unwrap();
    // Apple Podcasts deep link = podcast:// + feed URL without the scheme.
    assert!(sub.contains(&format!("podcast://test/feed/{feed_id}.xml")));
    // Manual-paste fallback still carries the exact feed URL.
    assert!(sub.contains(&format!("http://test/feed/{feed_id}.xml")));
    // A bad capability id 404s.
    let resp = app
        .clone()
        .oneshot(Request::get("/subscribe/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Unknown book -> 404; cover with no extracted art (Task 3.4) -> 404.
    for uri in ["/book/nope".to_string(), format!("/cover/{feed_id}")] {
        let resp = app
            .clone()
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }

    // Path-traversal / bad-charset ids are rejected with 404 and no path leak in
    // the body (NFR-S1). `.` and `/` are outside both the slug and feed_id
    // allow-lists, so these 404 before any DB/filesystem touch. %2e%2e keeps `..`
    // from being normalized by the router.
    for uri in [
        "/feed/..%2f..%2fetc%2fpasswd.xml",
        "/audio/..%2f..%2fetc%2fpasswd/1",
        "/book/..%2fsecret",
        "/cover/..%2fsecret",
        "/subscribe/bad.dotted",
        "/feed/bad.dotted.xml",
    ] {
        let resp = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            body_bytes(resp).await.is_empty(),
            "no leak in body for {uri}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
