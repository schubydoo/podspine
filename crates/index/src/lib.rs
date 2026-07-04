//! `index` — the `rusqlite` (bundled SQLite) access layer.
//!
//! Owns the schema (TAD §5.1) and provides idempotent upserts keyed on stable
//! ids (`book.id`, `episode.guid`) so re-scans of an unchanged source don't churn
//! the database. `rusqlite::Connection` is not `Sync`, so the server accesses an
//! [`Index`] behind a mutex / `spawn_blocking` (Sprint 2.4); this layer is
//! synchronous.
//!
//! Feed tokens (v1.5, per-book): a token is 128 bits of OS randomness rendered
//! URL-safe. Only its BLAKE3 hash is ever persisted; the raw token is shown to
//! the user once at mint time and is not recoverable. Validation hashes the
//! presented token and looks it up by hash, so the raw secret is never compared
//! directly (no timing side-channel) — see [`token`] and [`Index::token_book`].

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

/// Feed-token primitives: generate a fresh token, and hash one for storage or
/// lookup. Pure and side-effect-free; persistence lives on [`Index`].
pub mod token {
    use base64::Engine;

    /// Token entropy in bytes (128 bits — NFR-S2).
    const TOKEN_BYTES: usize = 16;

    /// Generate a fresh, URL-safe feed token (128-bit, base64url, unpadded).
    ///
    /// Panics only if the OS RNG is unavailable, which on a running server means
    /// the platform is broken and there's no sensible recovery.
    pub fn generate() -> String {
        let mut buf = [0u8; TOKEN_BYTES];
        getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    }

    /// Hash a raw token for storage/lookup. Deterministic; the raw token is
    /// never stored, so only this hash is compared.
    pub fn hash(raw: &str) -> String {
        blake3::hash(raw.as_bytes()).to_hex().to_string()
    }
}

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

/// A stored feed token's metadata — never the raw token, which is shown once at
/// mint time and not recoverable. Suitable for a revocation UI (show the hash
/// prefix as an identifier).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    /// BLAKE3 hash of the raw token (the stored primary key).
    pub token_hash: String,
    /// Owning book id.
    pub book_id: String,
    /// Mint time (epoch seconds).
    pub created_at: i64,
    /// Revocation time (epoch seconds), or `None` if still active.
    pub revoked_at: Option<i64>,
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

    /// Mint a fresh per-book feed token. Returns the **raw** token (show it once —
    /// it is not recoverable); only its BLAKE3 hash is stored. `now` is the
    /// mint epoch (seconds).
    pub fn mint_token(&self, book_id: &str, now: i64) -> Result<String, IndexError> {
        let raw = token::generate();
        self.conn.execute(
            "INSERT INTO feed_token (token_hash, book_id, created_at, revoked_at)
             VALUES (?1, ?2, ?3, NULL)",
            params![token::hash(&raw), book_id, now],
        )?;
        Ok(raw)
    }

    /// Resolve a raw token to its still-active book id, or `None` if the token is
    /// unknown or revoked. The lookup is by hash, so the raw secret is never
    /// compared directly.
    pub fn token_book(&self, raw: &str) -> Result<Option<String>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT book_id FROM feed_token
                 WHERE token_hash = ?1 AND revoked_at IS NULL",
                [token::hash(raw)],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Revoke a token by its hash, effective immediately. Idempotent: returns
    /// `true` if an active token was revoked, `false` if it was unknown or
    /// already revoked. `now` is the revocation epoch (seconds).
    pub fn revoke_token(&self, token_hash: &str, now: i64) -> Result<bool, IndexError> {
        let n = self.conn.execute(
            "UPDATE feed_token SET revoked_at = ?2
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![token_hash, now],
        )?;
        Ok(n > 0)
    }

    /// A book's tokens (active and revoked), newest first — for the revocation UI.
    pub fn list_tokens(&self, book_id: &str) -> Result<Vec<TokenRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT token_hash, book_id, created_at, revoked_at
             FROM feed_token WHERE book_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([book_id], token_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

fn token_from_row(row: &Row) -> rusqlite::Result<TokenRow> {
    Ok(TokenRow {
        token_hash: row.get(0)?,
        book_id: row.get(1)?,
        created_at: row.get(2)?,
        revoked_at: row.get(3)?,
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

    // ---- feed tokens (Task 4.1) ----

    #[test]
    fn generated_tokens_are_128_bit_url_safe_and_unique() {
        use base64::Engine;
        let t = token::generate();
        // URL-safe base64, no padding, no `+`/`/`/`=`.
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "not url-safe: {t}"
        );
        // Decodes to exactly 16 bytes (128 bits).
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&t)
            .unwrap();
        assert_eq!(bytes.len(), 16);
        // Two mints don't collide (CSPRNG).
        assert_ne!(token::generate(), token::generate());
    }

    #[test]
    fn mint_returns_raw_but_persists_only_the_hash() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();

        let raw = idx.mint_token("b1", 1_700_000_000).unwrap();

        // The raw token never lands in the table; only its hash does.
        let stored: String = idx
            .conn
            .query_row("SELECT token_hash FROM feed_token", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, raw, "raw token must not be stored");
        assert_eq!(stored, token::hash(&raw));
        // And it resolves back to the book.
        assert_eq!(idx.token_book(&raw).unwrap().as_deref(), Some("b1"));
    }

    #[test]
    fn unknown_token_resolves_to_none() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        assert_eq!(idx.token_book("nope-not-a-real-token").unwrap(), None);
    }

    #[test]
    fn revoked_token_stops_resolving_but_is_still_listed() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        let raw = idx.mint_token("b1", 1_700_000_000).unwrap();
        let hash = token::hash(&raw);

        assert!(idx.revoke_token(&hash, 1_700_000_100).unwrap());
        assert_eq!(idx.token_book(&raw).unwrap(), None, "revoked → no access");

        // Still visible to the UI, now marked revoked.
        let listed = idx.list_tokens("b1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].token_hash, hash);
        assert_eq!(listed[0].revoked_at, Some(1_700_000_100));

        // Revoking again is a no-op.
        assert!(!idx.revoke_token(&hash, 1_700_000_200).unwrap());
    }

    #[test]
    fn multiple_tokens_per_book_all_resolve_and_list_newest_first() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        let t1 = idx.mint_token("b1", 1_700_000_000).unwrap();
        let t2 = idx.mint_token("b1", 1_700_000_500).unwrap();

        assert_eq!(idx.token_book(&t1).unwrap().as_deref(), Some("b1"));
        assert_eq!(idx.token_book(&t2).unwrap().as_deref(), Some("b1"));

        let listed = idx.list_tokens("b1").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].created_at, 1_700_000_500, "newest first");
        assert_eq!(listed[1].created_at, 1_700_000_000);
    }

    #[test]
    fn tokens_cascade_on_book_delete() {
        let idx = Index::open_in_memory().unwrap();
        idx.upsert_book(&book("b1", "a-book", "A Book")).unwrap();
        let raw = idx.mint_token("b1", 1_700_000_000).unwrap();
        idx.conn
            .execute("DELETE FROM book WHERE id = 'b1'", [])
            .unwrap();
        assert_eq!(idx.token_book(&raw).unwrap(), None);
        assert!(idx.list_tokens("b1").unwrap().is_empty());
    }
}
