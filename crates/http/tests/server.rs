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
use podspine_scanner::scan_book;

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
    let slug = book.slug.clone();

    let state = AppState::new(index, "http://test".to_string(), &data);
    let app = router(state);

    // healthz
    let resp = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"ok");

    // feed
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/feed/{slug}.xml"))
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
    let xml = String::from_utf8(body_bytes(resp).await).unwrap();
    assert_eq!(xml.matches("<item>").count(), 3);
    assert!(xml.contains("<itunes:duration>"));
    assert!(xml.contains(&format!("http://test/audio/{slug}/1")));

    // unknown slug + missing .xml -> 404
    for uri in ["/feed/nope.xml".to_string(), format!("/feed/{slug}")] {
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
            Request::get(format!("/audio/{slug}/1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");

    // Range request -> 206 + Content-Range, exactly 100 bytes
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{slug}/1"))
                .header(header::RANGE, "bytes=0-99")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert!(resp.headers().get(header::CONTENT_RANGE).is_some());
    assert_eq!(body_bytes(resp).await.len(), 100);

    // unknown episode number -> 404
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/audio/{slug}/999"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}
