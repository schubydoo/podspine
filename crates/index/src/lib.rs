//! `index` — the `rusqlite` (bundled SQLite) access layer.
//!
//! Owns the schema (TAD §5.1) and provides idempotent upserts keyed on stable
//! ids (`book.id`, `episode.guid`) so re-scans of an unchanged source don't churn
//! the database. `rusqlite::Connection` is not `Sync`, so the server accesses an
//! [`Index`] behind a mutex / `spawn_blocking` (Sprint 2.4); this layer is
//! synchronous.
//!
//! Feed privacy (v1.5) is a **capability URL**: each book carries a random,
//! unguessable `feed_id` (128 bits) that is the public key for its feed, audio,
//! and cover — the human `slug` is only for the LAN browse UI. The `feed_id` is
//! stable across re-scans (preserved on upsert) and rotated only on an explicit
//! [`Index::regenerate_feed_id`] (leak recovery). Feeds are always kept out of
//! podcast directories (`itunes:block`) — they're private capability URLs.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

/// Capability-id generation: an unguessable, URL-safe feed id. Stored raw (it is
/// the owner's own retrievable link, shown in the UI and QR — not a hashed
/// per-subscriber secret).
pub mod capability {
    use base64::Engine;

    /// Capability entropy in bytes (128 bits).
    const ID_BYTES: usize = 16;

    /// Generate a fresh, URL-safe capability id (128-bit, base64url, unpadded).
    ///
    /// Panics only if the OS RNG is unavailable (an unrecoverable platform fault).
    pub fn generate() -> String {
        let mut buf = [0u8; ID_BYTES];
        getrandom::fill(&mut buf).expect("OS RNG unavailable");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    }
}

/// Schema, created on open. `IF NOT EXISTS` makes open idempotent. Pre-release:
/// changing this is a fresh-schema change (delete an old `data/podspine.db` and
/// re-scan) — no migration path is maintained before v1.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS book (
    id           TEXT PRIMARY KEY,
    slug         TEXT NOT NULL UNIQUE,
    feed_id      TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    author       TEXT,
    cover_path   TEXT,
    source_path  TEXT NOT NULL,
    source_mtime INTEGER NOT NULL,
    status       TEXT NOT NULL,
    storage_mode TEXT NOT NULL DEFAULT '',
    default_cover_url TEXT,
    force_embedded INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS episode (
    guid          TEXT PRIMARY KEY,
    book_id       TEXT NOT NULL REFERENCES book(id) ON DELETE CASCADE,
    idx           INTEGER NOT NULL,
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,
    byte_length   INTEGER NOT NULL,
    duration_sec  REAL NOT NULL,
    start_sec     REAL NOT NULL,
    pubdate_epoch INTEGER NOT NULL,
    source_path   TEXT NOT NULL DEFAULT '',
    needs_faststart INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS episode_book_idx ON episode(book_id, idx);
";

/// One audiobook.
#[derive(Debug, Clone, PartialEq)]
pub struct BookRow {
    /// Opaque stable id.
    pub id: String,
    /// URL slug (unique) — the human key for the LAN browse UI only.
    pub slug: String,
    /// Capability id (unique, unguessable) — the public key for feed/audio/cover.
    /// Stable across re-scans; rotated only by [`Index::regenerate_feed_id`].
    pub feed_id: String,
    /// Title.
    pub title: String,
    /// Author, if known.
    pub author: Option<String>,
    /// Cover image path, if extracted.
    pub cover_path: Option<String>,
    /// Path to the source file/folder.
    pub source_path: String,
    /// Source mtime (epoch seconds).
    pub source_mtime: i64,
    /// Processing status (e.g. `ready`).
    pub status: String,
    /// Effective per-book storage mode as a string (`"full"`/`"saver"`), or `""`
    /// to follow the global config (a pre-6.4 row, until re-scanned). Persisted so
    /// the serve/evict layers honor a per-book `.podspine.toml` override without
    /// the sidecar (Sprint 6.4).
    pub storage_mode: String,
    /// Per-book feed cover fallback (a `.podspine.toml` override), tried before
    /// the server-wide `default_cover_url`.
    pub default_cover_url: Option<String>,
    /// Effective `force_embedded_chapters` at last ingest (a `.podspine.toml`
    /// override). Persisted only so a scan can detect a toggle and re-ingest;
    /// not read at serve time.
    pub force_embedded: bool,
}

/// One episode (a split chapter). Numeric fields are stored as SQLite integers.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeRow {
    /// Stable guid (`blake3(book.id:idx:mtime)`).
    pub guid: String,
    /// Owning book id.
    pub book_id: String,
    /// Zero-based chapter position.
    pub idx: i64,
    /// Episode title.
    pub title: String,
    /// Path to the served audio file. Extracted (chaptered) episode → the split
    /// under `<data_dir>`. Whole-file episode served in place → the original
    /// library file (`== source_path`). Whole-file episode remuxed to faststart
    /// (Sprint 6.3) → the cache file under `<data_dir>` (`!= source_path`).
    pub file_path: String,
    /// When non-empty, the episode IS a whole source file (MP3-folder track, or a
    /// chapterless single file). `file_path == source_path` ⇒ streamed in place
    /// from the library; `file_path != source_path` ⇒ remuxed to faststart and
    /// cached under `<data_dir>`. Empty ⇒ an extracted sub-range (`full`/`saver`).
    /// See TAD §5.3.
    pub source_path: String,
    /// `true` when the source is a non-faststart MP4 (`moov` after `mdat`), so a
    /// whole-file serve seeks slowly. Recorded at ingest; drives the callout and,
    /// with `PODSPINE_REMUX_NON_FASTSTART` on, the remux-vs-in-place decision.
    pub needs_faststart: bool,
    /// Real output size in bytes.
    pub byte_length: i64,
    /// Duration in seconds.
    pub duration_sec: f64,
    /// Chapter start offset in the source, in seconds. Needed to regenerate the
    /// chapter on demand in `saver` storage mode.
    pub start_sec: f64,
    /// pubDate epoch seconds.
    pub pubdate_epoch: i64,
}

/// Errors from the index layer.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// An underlying SQLite error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A handle to the SQLite index.
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Open (creating if needed) the database at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IndexError> {
        Self::init(Connection::open(path)?)
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self, IndexError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, IndexError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL lets the background library watcher (its own connection) write
        // during a rescan without blocking the server's feed/audio reads. A
        // no-op for `:memory:` databases. (Task 4.3.)
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Additive migrations for databases created by an older Podspine. Pre-v1 we
    /// don't keep a full migration path, but silently breaking an existing DB at
    /// request time is worse than a cheap `ADD COLUMN` here: `CREATE TABLE IF NOT
    /// EXISTS` won't add a column to a table that already exists, so an older
    /// `episode` table would be missing `start_sec` and every audio/feed query
    /// would fail mid-request. `ADD COLUMN` preserves all rows (and the
    /// capability `feed_id`s — a drop+recreate would rotate every feed URL).
    /// Existing episodes get `start_sec = 0`; that value is only read for
    /// `saver`-mode regeneration and is corrected on the next re-scan.
    fn migrate(conn: &Connection) -> Result<(), IndexError> {
        let has_start_sec: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('episode') WHERE name = 'start_sec'",
            [],
            |r| r.get(0),
        )?;
        if has_start_sec == 0 {
            conn.execute(
                "ALTER TABLE episode ADD COLUMN start_sec REAL NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // `source_path` (serve-in-place, Sprint 6.2). Existing rows default to
        // `''` (extracted/copied under `<data_dir>`, as before); a re-scan flips
        // whole-file books to in-place serving and reclaims their copies.
        let has_source_path: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('episode') WHERE name = 'source_path'",
            [],
            |r| r.get(0),
        )?;
        if has_source_path == 0 {
            conn.execute(
                "ALTER TABLE episode ADD COLUMN source_path TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        // `needs_faststart` (faststart detection, Sprint 6.3). Existing rows
        // default to `0`; a re-scan re-detects and records it per whole-file mp4.
        let has_needs_faststart: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('episode') WHERE name = 'needs_faststart'",
            [],
            |r| r.get(0),
        )?;
        if has_needs_faststart == 0 {
            conn.execute(
                "ALTER TABLE episode ADD COLUMN needs_faststart INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // `book.storage_mode` + `book.default_cover_url` (per-book overrides,
        // Sprint 6.4). Existing rows default to `''`/NULL, meaning "follow the
        // global config"; a re-scan records the effective per-book value.
        let has_book_storage_mode: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('book') WHERE name = 'storage_mode'",
            [],
            |r| r.get(0),
        )?;
        if has_book_storage_mode == 0 {
            conn.execute(
                "ALTER TABLE book ADD COLUMN storage_mode TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let has_book_cover_url: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('book') WHERE name = 'default_cover_url'",
            [],
            |r| r.get(0),
        )?;
        if has_book_cover_url == 0 {
            conn.execute("ALTER TABLE book ADD COLUMN default_cover_url TEXT", [])?;
        }
        // `book.force_embedded` (per-book overrides, 6.4). Recorded so a scan can
        // detect a `.podspine.toml` `force_embedded_chapters` toggle (which changes
        // the chapter source without changing the audio mtime) and re-ingest.
        let has_force_embedded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('book') WHERE name = 'force_embedded'",
            [],
            |r| r.get(0),
        )?;
        if has_force_embedded == 0 {
            conn.execute(
                "ALTER TABLE book ADD COLUMN force_embedded INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        Ok(())
    }

    /// Insert or update a book by `id` (idempotent — no duplicate rows).
    ///
    /// On conflict, `feed_id` is **preserved**, not overwritten: a re-scan
    /// supplies a fresh `feed_id` it doesn't know is already set, and the
    /// capability must stay stable across re-scans (it changes only via
    /// [`Index::regenerate_feed_id`]).
    pub fn upsert_book(&self, b: &BookRow) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO book
               (id, slug, feed_id, title, author, cover_path, source_path, source_mtime, status, storage_mode, default_cover_url, force_embedded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
               slug=excluded.slug, title=excluded.title, author=excluded.author,
               cover_path=excluded.cover_path, source_path=excluded.source_path,
               source_mtime=excluded.source_mtime, status=excluded.status,
               storage_mode=excluded.storage_mode, default_cover_url=excluded.default_cover_url,
               force_embedded=excluded.force_embedded",
            params![
                b.id,
                b.slug,
                b.feed_id,
                b.title,
                b.author,
                b.cover_path,
                b.source_path,
                b.source_mtime,
                b.status,
                b.storage_mode,
                b.default_cover_url,
                b.force_embedded,
            ],
        )?;
        Ok(())
    }

    /// Insert or update an episode by `guid` (idempotent).
    pub fn upsert_episode(&self, e: &EpisodeRow) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO episode
               (guid, book_id, idx, title, file_path, byte_length, duration_sec, start_sec, pubdate_epoch, source_path, needs_faststart)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(guid) DO UPDATE SET
               book_id=excluded.book_id, idx=excluded.idx, title=excluded.title,
               file_path=excluded.file_path, byte_length=excluded.byte_length,
               duration_sec=excluded.duration_sec, start_sec=excluded.start_sec,
               pubdate_epoch=excluded.pubdate_epoch, source_path=excluded.source_path,
               needs_faststart=excluded.needs_faststart",
            params![
                e.guid,
                e.book_id,
                e.idx,
                e.title,
                e.file_path,
                e.byte_length,
                e.duration_sec,
                e.start_sec,
                e.pubdate_epoch,
                e.source_path,
                e.needs_faststart,
            ],
        )?;
        Ok(())
    }

    /// Fetch a book by id.
    pub fn get_book(&self, id: &str) -> Result<Option<BookRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, slug, feed_id, title, author, cover_path, source_path, source_mtime, status, storage_mode, default_cover_url, force_embedded
                 FROM book WHERE id = ?1",
                [id],
                book_from_row,
            )
            .optional()?)
    }

    /// Fetch a book by slug (the feed-route lookup key).
    pub fn get_book_by_slug(&self, slug: &str) -> Result<Option<BookRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, slug, feed_id, title, author, cover_path, source_path, source_mtime, status, storage_mode, default_cover_url, force_embedded
                 FROM book WHERE slug = ?1",
                [slug],
                book_from_row,
            )
            .optional()?)
    }

    /// All books, ordered by title.
    pub fn list_books(&self) -> Result<Vec<BookRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, feed_id, title, author, cover_path, source_path, source_mtime, status, storage_mode, default_cover_url, force_embedded
             FROM book ORDER BY title",
        )?;
        let rows = stmt.query_map([], book_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Episodes for a book, ordered by chapter index (chapter 1 first).
    pub fn episodes_for_book(&self, book_id: &str) -> Result<Vec<EpisodeRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT guid, book_id, idx, title, file_path, byte_length, duration_sec, start_sec, pubdate_epoch, source_path, needs_faststart
             FROM episode WHERE book_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([book_id], episode_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch a book by its capability `feed_id` (the public feed/audio/cover
    /// lookup key). Unknown ids return `None` → 404, so a guessed id reveals
    /// nothing.
    pub fn get_book_by_feed_id(&self, feed_id: &str) -> Result<Option<BookRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, slug, feed_id, title, author, cover_path, source_path, source_mtime, status, storage_mode, default_cover_url, force_embedded
                 FROM book WHERE feed_id = ?1",
                [feed_id],
                book_from_row,
            )
            .optional()?)
    }

    /// Rotate a book's capability id (leak recovery): the old feed/audio/cover
    /// URLs stop resolving immediately. Returns the new `feed_id`. No-op returns
    /// `Ok(None)` if the book id is unknown.
    pub fn regenerate_feed_id(&self, book_id: &str) -> Result<Option<String>, IndexError> {
        let new_id = capability::generate();
        let n = self.conn.execute(
            "UPDATE book SET feed_id = ?2 WHERE id = ?1",
            params![book_id, new_id],
        )?;
        Ok((n > 0).then_some(new_id))
    }

    /// Delete a book by id; its episodes and feed capability cascade away.
    /// Returns whether a row was removed. Used by the watcher's orphan prune
    /// (Task 4.3) when a source disappears from the library.
    pub fn delete_book(&self, id: &str) -> Result<bool, IndexError> {
        let n = self.conn.execute("DELETE FROM book WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// Fetch an episode by guid.
    pub fn get_episode(&self, guid: &str) -> Result<Option<EpisodeRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT guid, book_id, idx, title, file_path, byte_length, duration_sec, start_sec, pubdate_epoch, source_path, needs_faststart
                 FROM episode WHERE guid = ?1",
                [guid],
                episode_from_row,
            )
            .optional()?)
    }
}

fn book_from_row(row: &Row) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        id: row.get(0)?,
        slug: row.get(1)?,
        feed_id: row.get(2)?,
        title: row.get(3)?,
        author: row.get(4)?,
        cover_path: row.get(5)?,
        source_path: row.get(6)?,
        source_mtime: row.get(7)?,
        status: row.get(8)?,
        storage_mode: row.get(9)?,
        default_cover_url: row.get(10)?,
        force_embedded: row.get(11)?,
    })
}

fn episode_from_row(row: &Row) -> rusqlite::Result<EpisodeRow> {
    Ok(EpisodeRow {
        guid: row.get(0)?,
        book_id: row.get(1)?,
        idx: row.get(2)?,
        title: row.get(3)?,
        file_path: row.get(4)?,
        byte_length: row.get(5)?,
        duration_sec: row.get(6)?,
        start_sec: row.get(7)?,
        pubdate_epoch: row.get(8)?,
        source_path: row.get(9)?,
        needs_faststart: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(id: &str, slug: &str, title: &str) -> BookRow {
        BookRow {
            id: id.to_string(),
            slug: slug.to_string(),
            feed_id: format!("cap-{id}"),
            title: title.to_string(),
            author: Some("Author".to_string()),
            cover_path: None,
            source_path: format!("/library/{id}.m4b"),
            source_mtime: 1_700_000_000,
            status: "ready".to_string(),
            storage_mode: String::new(),
            default_cover_url: None,
            force_embedded: false,
        }
    }

    fn episode(book_id: &str, idx: i64) -> EpisodeRow {
        EpisodeRow {
            guid: format!("{book_id}-{idx}"),
            book_id: book_id.to_string(),
            idx,
            title: format!("Chapter {}", idx + 1),
            file_path: format!("/data/books/{book_id}/{:03}.m4a", idx + 1),
            source_path: String::new(),
            needs_faststart: false,
            byte_length: 1000 + idx,
            duration_sec: 60.0 * (idx as f64 + 1.0),
            start_sec: 60.0 * (idx as f64),
            pubdate_epoch: 1_700_000_000 + idx,
        }
    }

    #[test]
    fn open_creates_schema_and_is_empty() {
        let idx = Index::open_in_memory().unwrap();
        assert!(idx.list_books().unwrap().is_empty());
    }

    #[test]
    fn upsert_and_fetch_book() {
        let idx = Index::open_in_memory().unwrap();
        let b = book("b1", "a-book", "A Book");
        idx.upsert_book(&b).unwrap();
        assert_eq!(idx.get_book("b1").unwrap().as_ref(), Some(&b));
        assert_eq!(idx.get_book_by_slug("a-book").unwrap().as_ref(), Some(&b));
        assert_eq!(idx.get_book("missing").unwrap(), None);
    }

    #[test]
    fn upserting_a_book_twice_is_a_no_op() {
        let idx = Index::open_in_memory().unwrap();
        let b = book("b1", "a-book", "A Book");
        idx.upsert_book(&b).unwrap();
        idx.upsert_book(&b).unwrap();
        assert_eq!(idx.list_books().unwrap().len(), 1);
    }

    #[test]
    fn upsert_updates_existing_book() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "Old Title")).unwrap();
        idx.upsert_book(&book("b1", "a-book", "New Title")).unwrap();
        assert_eq!(idx.get_book("b1").unwrap().unwrap().title, "New Title");
        assert_eq!(idx.list_books().unwrap().len(), 1);
    }

    #[test]
    fn episodes_upsert_idempotently_and_return_in_order() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();

        // Insert out of order; upsert one twice.
        for i in [2, 0, 1, 0] {
            idx.upsert_episode(&episode("b1", i)).unwrap();
        }

        let eps = idx.episodes_for_book("b1").unwrap();
        assert_eq!(eps.len(), 3, "duplicate guid must not create a row");
        assert_eq!(
            eps.iter().map(|e| e.idx).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "episodes ordered by idx"
        );
        assert_eq!(
            idx.get_episode("b1-1").unwrap().as_ref(),
            Some(&episode("b1", 1))
        );
        assert_eq!(idx.get_episode("missing").unwrap(), None);
    }

    #[test]
    fn foreign_key_is_enforced() {
        let idx = Index::open_in_memory().unwrap();
        // No such book -> FK violation.
        let err = idx.upsert_episode(&episode("ghost", 0)).unwrap_err();
        assert!(matches!(err, IndexError::Sqlite(_)));
    }

    #[test]
    fn deleting_a_book_cascades_to_episodes() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        idx.upsert_episode(&episode("b1", 0)).unwrap();
        idx.conn
            .execute("DELETE FROM book WHERE id = 'b1'", [])
            .unwrap();
        assert!(idx.episodes_for_book("b1").unwrap().is_empty());
    }

    #[test]
    fn open_persists_to_a_file_across_reopen() {
        let dir = std::env::temp_dir().join("podspine-index-file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("podspine.db");

        let idx = Index::open(&db).unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        drop(idx);

        // Reopen the same file: the row (and schema) survived.
        let reopened = Index::open(&db).unwrap();
        assert_eq!(reopened.get_book("b1").unwrap().unwrap().title, "A Book");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_migrates_a_pre_start_sec_database() {
        // Simulate a database created before `episode.start_sec` existed.
        let dir = std::env::temp_dir().join("podspine-index-migrate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("old.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE book (
                    id TEXT PRIMARY KEY, slug TEXT NOT NULL UNIQUE, feed_id TEXT NOT NULL UNIQUE,
                    title TEXT NOT NULL, author TEXT, cover_path TEXT, source_path TEXT NOT NULL,
                    source_mtime INTEGER NOT NULL, status TEXT NOT NULL);
                 CREATE TABLE episode (
                    guid TEXT PRIMARY KEY, book_id TEXT NOT NULL, idx INTEGER NOT NULL,
                    title TEXT NOT NULL, file_path TEXT NOT NULL, byte_length INTEGER NOT NULL,
                    duration_sec REAL NOT NULL, pubdate_epoch INTEGER NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO book VALUES ('b1','a-book','cap-b1','A Book',NULL,NULL,'/lib/b1.m4b',1,'ready')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO episode VALUES ('b1-0','b1',0,'One','/data/b1/001.m4a',1234,60.0,100)",
                [],
            )
            .unwrap();
        }

        // Opening runs the additive migration: start_sec is added (default 0),
        // existing rows and the capability feed_id survive.
        let idx = Index::open(&db).unwrap();
        let eps = idx.episodes_for_book("b1").unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(
            eps[0].start_sec, 0.0,
            "migrated rows default to start_sec 0"
        );
        assert_eq!(eps[0].byte_length, 1234);
        assert_eq!(
            eps[0].source_path, "",
            "migrated rows default to empty source_path (extracted, not in-place)"
        );
        assert!(
            !eps[0].needs_faststart,
            "migrated rows default to needs_faststart = false"
        );
        assert_eq!(
            idx.get_book_by_feed_id("cap-b1").unwrap().unwrap().id,
            "b1",
            "capability feed_id preserved across migration"
        );

        // Idempotent: reopening an already-migrated DB is a no-op.
        drop(idx);
        assert!(Index::open(&db).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- capability feed ids (v1.5) ----

    #[test]
    fn generated_capability_ids_are_128_bit_url_safe_and_unique() {
        use base64::Engine;
        let id = capability::generate();
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "not url-safe: {id}"
        );
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&id)
            .unwrap();
        assert_eq!(bytes.len(), 16);
        assert_ne!(capability::generate(), capability::generate());
    }

    #[test]
    fn lookup_by_feed_id_resolves_the_book() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        // `book()` sets feed_id = "cap-b1".
        assert_eq!(idx.get_book_by_feed_id("cap-b1").unwrap().unwrap().id, "b1");
        assert_eq!(idx.get_book_by_feed_id("cap-nope").unwrap(), None);
    }

    #[test]
    fn delete_book_removes_it_and_cascades() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        idx.upsert_episode(&episode("b1", 0)).unwrap();
        assert!(idx.delete_book("b1").unwrap());
        assert_eq!(idx.get_book("b1").unwrap(), None);
        assert!(idx.episodes_for_book("b1").unwrap().is_empty());
        // Deleting a missing book is a no-op.
        assert!(!idx.delete_book("b1").unwrap());
    }

    #[test]
    fn feed_id_survives_a_rescan() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();

        // A re-scan supplies a different feed_id; the capability must be preserved
        // (only title/etc update).
        let mut rescan = book("b1", "a-book", "A Book v2");
        rescan.feed_id = "cap-DIFFERENT".to_string();
        idx.upsert_book(&rescan).unwrap();

        let got = idx.get_book("b1").unwrap().unwrap();
        assert_eq!(got.title, "A Book v2", "mutable fields update");
        assert_eq!(got.feed_id, "cap-b1", "capability is stable across rescans");
    }

    #[test]
    fn regenerate_rotates_the_capability_and_kills_the_old_id() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();

        let new_id = idx.regenerate_feed_id("b1").unwrap().unwrap();
        assert_ne!(new_id, "cap-b1");
        assert_eq!(idx.get_book_by_feed_id(&new_id).unwrap().unwrap().id, "b1");
        assert_eq!(
            idx.get_book_by_feed_id("cap-b1").unwrap(),
            None,
            "old capability URL 404s after regenerate"
        );
        // Unknown book → no-op.
        assert_eq!(idx.regenerate_feed_id("ghost").unwrap(), None);
    }

    #[test]
    fn feed_id_is_unique_across_books() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "book-1", "One")).unwrap();
        let mut clash = book("b2", "book-2", "Two");
        clash.feed_id = "cap-b1".to_string(); // collide on purpose
        assert!(idx.upsert_book(&clash).is_err(), "feed_id must be unique");
    }
}
