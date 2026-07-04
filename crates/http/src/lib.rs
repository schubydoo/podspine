//! `http` — Axum router: `GET /feed/{slug}.xml`, `GET /audio/{book}/{episode}`
//! (Range via `axum-range`), `GET /` + `GET /book/{slug}` (UI), `GET /healthz`,
//! and optional `GET /metrics`. Applies `tower-http` timeout + body-limit +
//! concurrency-limit layers. See TAD §4. Implemented in Sprint 2 (Tasks 2.4/2.5).
