//! Per-book `.podspine.toml` overrides (Sprint 6.4).
//!
//! A sidecar beside a single-file book, or inside a folder book, overrides
//! per-book-meaningful server settings for that ONE book — handy for
//! troubleshooting a misbehaving book without touching the whole server.
//! Precedence: **sidecar → CLI/env → global TOML → default**.
//!
//! Sidecar location (supports every library layout):
//! - **single-file book** (top-level `Author - Title.m4b`, or a lone file in its
//!   own folder) → `Author - Title.podspine.toml` beside the audio (mirrors the
//!   `.cue`/`.ffmeta` convention);
//! - **MP3-folder book, or that lone-file-in-a-folder** → `.podspine.toml` inside
//!   the folder;
//! - a top-level file's parent is the library root, so no folder-level file
//!   applies there (it would wrongly cover every top-level book).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::StorageMode;

/// Overrides parsed from a book's `.podspine.toml`. Every field is optional; an
/// unset field falls back to the resolved server config. Server-global keys are
/// accepted (so a stray one doesn't hard-fail the parse) but reported by
/// [`BookOverrides::ignored_global_keys`] and never applied. An unknown key is a
/// parse error, surfaced as a per-book warning (a bad sidecar never aborts a scan).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BookOverrides {
    // --- per-book-meaningful config overrides ---
    /// Ignore `.cue`/`.ffmeta` sidecars for this book.
    pub force_embedded_chapters: Option<bool>,
    /// `full`/`saver` for this book (persisted so serve/evict honor it).
    pub storage_mode: Option<StorageMode>,
    /// Remux this book to faststart if it's a non-faststart whole-file mp4.
    pub remux_non_faststart: Option<bool>,
    /// Feed-level fallback cover for this book.
    pub default_cover_url: Option<String>,
    // --- troubleshooting-only (no global equivalent) ---
    /// Skip this book entirely (removed from the index + every surface).
    pub disabled: Option<bool>,
    /// Override the feed/book title.
    pub title: Option<String>,
    /// Override the author.
    pub author: Option<String>,
    /// Force a re-ingest on the next scan (skips the idempotency early return).
    pub force_reingest: Option<bool>,
    // --- server-global keys: accepted, then ignored with a warning ---
    library: Option<toml::Value>,
    data_dir: Option<toml::Value>,
    bind: Option<toml::Value>,
    base_url: Option<toml::Value>,
    config: Option<toml::Value>,
    cache_size: Option<toml::Value>,
    cache_ttl: Option<toml::Value>,
}

impl BookOverrides {
    /// Names of any server-global keys present in the sidecar (each is ignored).
    /// The caller logs these so a user learns why `bind: …` in a per-book file
    /// did nothing.
    pub fn ignored_global_keys(&self) -> Vec<&'static str> {
        [
            ("library", self.library.is_some()),
            ("data_dir", self.data_dir.is_some()),
            ("bind", self.bind.is_some()),
            ("base_url", self.base_url.is_some()),
            ("config", self.config.is_some()),
            ("cache_size", self.cache_size.is_some()),
            ("cache_ttl", self.cache_ttl.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, present)| present.then_some(name))
        .collect()
    }
}

/// The sidecar path for a book whose `source` is a file (single-file book) or a
/// directory (MP3 folder), or `None` if none exists. Both `source` and
/// `library_root` should be canonical/absolute (the scanner canonicalizes them).
pub fn sidecar_path(source: &Path, library_root: &Path) -> Option<PathBuf> {
    if source.is_dir() {
        let p = source.join(".podspine.toml");
        return p.is_file().then_some(p);
    }
    // Single-file book → the stem sibling first (matches the `.cue` convention).
    let stem = source.with_extension("podspine.toml");
    if stem.is_file() {
        return Some(stem);
    }
    // ...then a folder-level file, but only when the file lives in its own
    // subfolder (never the library root, which every top-level book shares).
    let parent = source.parent()?;
    if parent != library_root {
        let p = parent.join(".podspine.toml");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Read + parse a book's per-book overrides. `Ok(None)` when there is no sidecar;
/// `Err(msg)` on a read/parse failure — the caller logs `msg` and proceeds with
/// no overrides (a bad sidecar is a per-book warning, never fatal to the scan).
pub fn load(source: &Path, library_root: &Path) -> Result<Option<BookOverrides>, String> {
    let Some(path) = sidecar_path(source, library_root) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    toml::from_str::<BookOverrides>(&text)
        .map(Some)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("podspine-bookoverrides-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Canonicalize so the `parent != library_root` check compares like-for-like.
        d.canonicalize().unwrap()
    }

    #[test]
    fn stem_sidecar_for_a_top_level_file() {
        let lib = scratch("stem");
        let audio = lib.join("Author - Title.m4b");
        std::fs::write(&audio, b"x").unwrap();
        // No folder-level file applies at the library root.
        std::fs::write(lib.join(".podspine.toml"), b"disabled = true").unwrap();
        assert_eq!(sidecar_path(&audio, &lib), None, "no sidecar yet (stem)");

        let side = lib.join("Author - Title.podspine.toml");
        std::fs::write(&side, b"storage_mode = \"saver\"").unwrap();
        assert_eq!(sidecar_path(&audio, &lib), Some(side));
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn folder_sidecar_for_an_mp3_folder_and_a_lone_file_in_a_subfolder() {
        let lib = scratch("folder");
        // MP3-folder book: source is the dir.
        let book = lib.join("A Folder Book");
        std::fs::create_dir_all(&book).unwrap();
        std::fs::write(book.join(".podspine.toml"), b"storage_mode = \"full\"").unwrap();
        assert_eq!(
            sidecar_path(&book, &lib),
            Some(book.join(".podspine.toml")),
            "mp3 folder → inside file"
        );

        // A lone file in its own subfolder: folder-level file applies (no stem one).
        let sub = lib.join("Single");
        std::fs::create_dir_all(&sub).unwrap();
        let audio = sub.join("audio.m4b");
        std::fs::write(&audio, b"x").unwrap();
        std::fs::write(sub.join(".podspine.toml"), b"disabled = true").unwrap();
        assert_eq!(
            sidecar_path(&audio, &lib),
            Some(sub.join(".podspine.toml")),
            "lone file in a subfolder → folder-level file applies"
        );
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn parses_overrides_and_reports_ignored_global_keys() {
        let lib = scratch("parse");
        let audio = lib.join("Book.m4a");
        std::fs::write(&audio, b"x").unwrap();
        std::fs::write(
            lib.join("Book.podspine.toml"),
            b"storage_mode = \"saver\"\nforce_embedded_chapters = true\ntitle = \"Fixed Title\"\ndisabled = false\nbind = \"0.0.0.0:9\"\ncache_size = \"1GB\"\n",
        )
        .unwrap();

        let o = load(&audio, &lib).unwrap().unwrap();
        assert_eq!(o.storage_mode, Some(StorageMode::Saver));
        assert_eq!(o.force_embedded_chapters, Some(true));
        assert_eq!(o.title.as_deref(), Some("Fixed Title"));
        assert_eq!(o.disabled, Some(false));
        // Server-global keys parse but are reported for the caller to warn + drop.
        let mut ignored = o.ignored_global_keys();
        ignored.sort_unstable();
        assert_eq!(ignored, vec!["bind", "cache_size"]);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn no_sidecar_is_ok_none_and_a_typo_is_an_err() {
        let lib = scratch("errs");
        let audio = lib.join("Book.m4b");
        std::fs::write(&audio, b"x").unwrap();
        assert_eq!(load(&audio, &lib).unwrap(), None, "no sidecar → Ok(None)");

        // An unknown key (typo) is a parse error → per-book warning, not fatal.
        std::fs::write(lib.join("Book.podspine.toml"), b"stroage_mode = \"saver\"").unwrap();
        assert!(load(&audio, &lib).is_err(), "unknown key → Err");
        let _ = std::fs::remove_dir_all(&lib);
    }
}
