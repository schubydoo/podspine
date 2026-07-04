//! `index` — the `rusqlite` (bundled SQLite) access layer.
//!
//! Owns the schema (TAD §5.1) and provides idempotent upserts keyed on stable
//! ids (`book.id`, `episode.guid`) so re-scans of an unchanged source don't churn
//! the database. `rusqlite::Connection` is not `Sync`, so the server accesses an
//! [`Index`] behind a mutex / `spawn_blocking` (Sprint 2.4); this layer is
//! synchronous.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

/// Schema, created on open. `IF NOT EXISTS` makes open idempotent; the
/// `feed_token` table is defined now (used from Sprint 4) so migrations stay
/// forward-only.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS book (
    id           TEXT PRIMARY KEY,
    slug         TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    author       TEXT,
    cover_path   TEXT,
    source_path  TEXT NOT NULL,
    source_mtime INTEGER NOT NULL,
    status       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS episode (
    guid          TEXT PRIMARY KEY,
    book_id       TEXT NOT NULL REFERENCES book(id) ON DELETE CASCADE,
    idx           INTEGER NOT NULL,
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,
    byte_length   INTEGER NOT NULL,
    duration_sec  REAL NOT NULL,
    pubdate_epoch INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS episode_book_idx ON episode(book_id, idx);
CREATE TABLE IF NOT EXISTS feed_token (
    token_hash TEXT PRIMARY KEY,
    book_id    TEXT NOT NULL REFERENCES book(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
";

/// One audiobook.
#[derive(Debug, Clone, PartialEq)]
pub struct BookRow {
    /// Opaque stable id.
    pub id: String,
    /// URL slug (unique).
    pub slug: String,
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
    /// Path to the split audio file.
    pub file_path: String,
    /// Real output size in bytes.
    pub byte_length: i64,
    /// Duration in seconds.
    pub duration_sec: f64,
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
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Insert or update a book by `id` (idempotent — no duplicate rows).
    pub fn upsert_book(&self, b: &BookRow) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO book
               (id, slug, title, author, cover_path, source_path, source_mtime, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               slug=excluded.slug, title=excluded.title, author=excluded.author,
               cover_path=excluded.cover_path, source_path=excluded.source_path,
               source_mtime=excluded.source_mtime, status=excluded.status",
            params![
                b.id,
                b.slug,
                b.title,
                b.author,
                b.cover_path,
                b.source_path,
                b.source_mtime,
                b.status,
            ],
        )?;
        Ok(())
    }

    /// Insert or update an episode by `guid` (idempotent).
    pub fn upsert_episode(&self, e: &EpisodeRow) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO episode
               (guid, book_id, idx, title, file_path, byte_length, duration_sec, pubdate_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(guid) DO UPDATE SET
               book_id=excluded.book_id, idx=excluded.idx, title=excluded.title,
               file_path=excluded.file_path, byte_length=excluded.byte_length,
               duration_sec=excluded.duration_sec, pubdate_epoch=excluded.pubdate_epoch",
            params![
                e.guid,
                e.book_id,
                e.idx,
                e.title,
                e.file_path,
                e.byte_length,
                e.duration_sec,
                e.pubdate_epoch,
            ],
        )?;
        Ok(())
    }

    /// Fetch a book by id.
    pub fn get_book(&self, id: &str) -> Result<Option<BookRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, slug, title, author, cover_path, source_path, source_mtime, status
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
                "SELECT id, slug, title, author, cover_path, source_path, source_mtime, status
                 FROM book WHERE slug = ?1",
                [slug],
                book_from_row,
            )
            .optional()?)
    }

    /// All books, ordered by title.
    pub fn list_books(&self) -> Result<Vec<BookRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, title, author, cover_path, source_path, source_mtime, status
             FROM book ORDER BY title",
        )?;
        let rows = stmt.query_map([], book_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Episodes for a book, ordered by chapter index (chapter 1 first).
    pub fn episodes_for_book(&self, book_id: &str) -> Result<Vec<EpisodeRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT guid, book_id, idx, title, file_path, byte_length, duration_sec, pubdate_epoch
             FROM episode WHERE book_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([book_id], episode_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch an episode by guid.
    pub fn get_episode(&self, guid: &str) -> Result<Option<EpisodeRow>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT guid, book_id, idx, title, file_path, byte_length, duration_sec, pubdate_epoch
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
        title: row.get(2)?,
        author: row.get(3)?,
        cover_path: row.get(4)?,
        source_path: row.get(5)?,
        source_mtime: row.get(6)?,
        status: row.get(7)?,
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
        pubdate_epoch: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(id: &str, slug: &str, title: &str) -> BookRow {
        BookRow {
            id: id.to_string(),
            slug: slug.to_string(),
            title: title.to_string(),
            author: Some("Author".to_string()),
            cover_path: None,
            source_path: format!("/library/{id}.m4b"),
            source_mtime: 1_700_000_000,
            status: "ready".to_string(),
        }
    }

    fn episode(book_id: &str, idx: i64) -> EpisodeRow {
        EpisodeRow {
            guid: format!("{book_id}-{idx}"),
            book_id: book_id.to_string(),
            idx,
            title: format!("Chapter {}", idx + 1),
            file_path: format!("/data/books/{book_id}/{:03}.m4a", idx + 1),
            byte_length: 1000 + idx,
            duration_sec: 60.0 * (idx as f64 + 1.0),
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
}
