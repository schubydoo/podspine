//! `index` — `rusqlite` access layer (behind `spawn_blocking` or a small pool).
//! Owns the schema in TAD §5 (`book` / `episode` / `feed_token`); provides the
//! book/episode/token queries. Idempotent upserts keyed on stable ids so
//! re-scans don't churn. See TAD §4/§5.1. Implemented in Sprint 2 (Task 2.1).
