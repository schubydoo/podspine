//! `config` — resolve runtime configuration from CLI flags, environment, and an
//! optional TOML file (in that precedence), and preflight the ffmpeg toolchain.
//!
//! The library path is the only required input; everything else has a default,
//! so `podspine --library ./books` just works. Failures (missing library,
//! unparseable bind address, absent ffmpeg/ffprobe) surface as a clear fatal
//! error at startup — never mid-request. See TAD §4.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::Deserialize;

const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_DATA_DIR: &str = "./data";
/// Default `saver`-mode cache cap when unset: 2 GiB.
const DEFAULT_CACHE_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How a **chaptered** book's per-chapter episodes are produced and stored.
/// Whole-file episodes (MP3-folder tracks, chapterless single files) ignore this
/// — they are streamed in place from the library, never extracted (Sprint 6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    /// Pre-split every chapter to disk at ingest (fast serves, ~2× storage).
    #[default]
    Full,
    /// Split every chapter at ingest to record its byte length, then delete it
    /// and regenerate on demand into a bounded cache (minimal steady-state disk,
    /// a small first-play delay per uncached chapter).
    Saver,
}

/// Command-line / environment inputs. All optional here; required-ness is
/// enforced during [`Config::resolve`] so a value may instead come from TOML.
#[derive(Debug, Default, Parser)]
#[command(
    name = "podspine",
    version,
    about = "Serve audiobooks as per-chapter podcast feeds"
)]
pub struct Cli {
    /// Library root to scan (required, unless set via env/TOML).
    #[arg(long, env = "PODSPINE_LIBRARY")]
    pub library: Option<PathBuf>,
    /// Directory for Podspine-owned data (SQLite index + split episodes).
    #[arg(long, env = "PODSPINE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Address to bind, e.g. `0.0.0.0:8080`.
    #[arg(long, env = "PODSPINE_BIND")]
    pub bind: Option<String>,
    /// External base URL for feed/enclosure links (defaults to the bind address).
    #[arg(long, env = "PODSPINE_BASE_URL")]
    pub base_url: Option<String>,
    /// Feed-level fallback cover image URL, used for books with no embedded art.
    #[arg(long, env = "PODSPINE_DEFAULT_COVER_URL")]
    pub default_cover_url: Option<String>,
    /// Force embedded chapters, ignoring any `.cue`/`.ffmeta` sidecar.
    #[arg(long, env = "PODSPINE_FORCE_EMBEDDED_CHAPTERS")]
    pub force_embedded_chapters: bool,
    /// Remux a non-faststart whole-file mp4 (`moov` after `mdat`) to faststart on
    /// demand so podcast clients seek quickly, instead of serving it in place. The
    /// remux is stream-copied and cache-managed (counts against the `saver` cache
    /// cap), never a pinned duplicate. Default off: serve in place + log a callout.
    #[arg(long, env = "PODSPINE_REMUX_NON_FASTSTART")]
    pub remux_non_faststart: bool,
    /// Chapter storage strategy for **chaptered** books: `full` pre-splits every
    /// chapter to disk (fast, ~2× storage); `saver` still splits at ingest but
    /// keeps chapters in a bounded on-demand cache (minimal steady-state disk, a
    /// small first-play delay). No effect on whole-file episodes (MP3 folders,
    /// chapterless singles), which are served in place from the library.
    #[arg(long, env = "PODSPINE_STORAGE_MODE")]
    pub storage_mode: Option<StorageMode>,
    /// Max disk for the on-demand chapter cache in `saver` mode (e.g. `2GB`,
    /// `500MB`; `0`/`off` = unbounded). Ignored in `full` mode.
    #[arg(long, env = "PODSPINE_CACHE_SIZE")]
    pub cache_size: Option<String>,
    /// TTL for cached chapters in `saver` mode (e.g. `30d`, `12h`; `off` =
    /// size-only eviction). Ignored in `full` mode.
    #[arg(long, env = "PODSPINE_CACHE_TTL")]
    pub cache_ttl: Option<String>,
    /// Optional TOML config file.
    #[arg(long, env = "PODSPINE_CONFIG")]
    pub config: Option<PathBuf>,
}

/// The lowest-precedence layer: an optional TOML file. Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Library root.
    pub library: Option<PathBuf>,
    /// Data directory.
    pub data_dir: Option<PathBuf>,
    /// Bind address.
    pub bind: Option<String>,
    /// External base URL.
    pub base_url: Option<String>,
    /// Feed-level fallback cover image URL.
    pub default_cover_url: Option<String>,
    /// Force embedded chapters, ignoring sidecars.
    pub force_embedded_chapters: Option<bool>,
    /// Remux non-faststart whole-file mp4 to faststart on demand (cache-managed).
    pub remux_non_faststart: Option<bool>,
    /// Chapter storage strategy (`full` | `saver`).
    pub storage_mode: Option<StorageMode>,
    /// On-demand cache size cap (e.g. `2GB`; `0`/`off` = unbounded).
    pub cache_size: Option<String>,
    /// On-demand cache TTL (e.g. `30d`; `off` = size-only eviction).
    pub cache_ttl: Option<String>,
}

/// Fully resolved, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Library root (validated to exist and be a directory).
    pub library: PathBuf,
    /// Data directory (created if missing).
    pub data_dir: PathBuf,
    /// Socket address to bind.
    pub bind: SocketAddr,
    /// External base URL, no trailing slash.
    pub base_url: String,
    /// Feed-level fallback cover image URL for books with no embedded art
    /// (`None` = emit no `itunes:image` when a book has no cover).
    pub default_cover_url: Option<String>,
    /// Ignore `.cue`/`.ffmeta` sidecars and always use embedded chapters.
    pub force_embedded_chapters: bool,
    /// Remux a non-faststart whole-file mp4 (`moov` after `mdat`) to faststart on
    /// demand rather than serving it in place — so podcast clients seek quickly.
    /// The remuxed copy is stream-copied and cache-managed (evicted under the
    /// `saver` cap, regenerated on demand), never a pinned duplicate. Default off:
    /// such books serve in place and a one-line callout is logged at ingest. No
    /// effect on faststart mp4, MP3/OGG/FLAC, or chaptered books (Sprint 6.3).
    pub remux_non_faststart: bool,
    /// How **chaptered** books are produced/stored: `full` (pre-split every
    /// chapter to disk) or `saver` (split at ingest, then cache regenerations on
    /// demand). Applies library-wide to chaptered books, which materialize under
    /// `data_dir`. Whole-file episodes (MP3-folder tracks, chapterless single
    /// files) are unaffected — they stream in place from the library (Sprint 6.2).
    pub storage_mode: StorageMode,
    /// Cache size cap in bytes for `saver` mode (`None` = unbounded).
    pub cache_size_bytes: Option<u64>,
    /// TTL for cached chapters in `saver` mode (`None` = size-only eviction).
    pub cache_ttl: Option<Duration>,
}

/// Configuration failures — all fatal, all reported at startup.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No library path from any source.
    #[error("no library path provided (use --library, PODSPINE_LIBRARY, or a config file)")]
    MissingLibrary,
    /// The library path does not exist.
    #[error("library path does not exist: {0}")]
    LibraryNotFound(PathBuf),
    /// The library path is not a directory.
    #[error("library path is not a directory: {0}")]
    LibraryNotDir(PathBuf),
    /// The bind address could not be parsed.
    #[error("invalid bind address {value:?}: {source}")]
    BadBind {
        /// The offending value.
        value: String,
        /// Parse error.
        source: std::net::AddrParseError,
    },
    /// The data directory could not be created.
    #[error("could not create data dir {path}: {source}")]
    DataDir {
        /// The path.
        path: PathBuf,
        /// I/O error.
        source: std::io::Error,
    },
    /// A required external tool is missing from PATH.
    #[error("`{0}` not found on PATH (install ffmpeg)")]
    ToolMissing(&'static str),
    /// The cache size value could not be parsed.
    #[error("invalid cache size {value:?}: {reason}")]
    BadCacheSize {
        /// The offending value.
        value: String,
        /// Why it failed.
        reason: String,
    },
    /// The cache TTL value could not be parsed.
    #[error("invalid cache TTL {value:?}: {reason}")]
    BadCacheTtl {
        /// The offending value.
        value: String,
        /// Why it failed.
        reason: String,
    },
    /// The config file could not be read.
    #[error("could not read config {path}: {source}")]
    ReadConfig {
        /// The path.
        path: PathBuf,
        /// I/O error.
        source: std::io::Error,
    },
    /// The config file could not be parsed as TOML.
    #[error("could not parse config {path}: {source}")]
    ParseConfig {
        /// The path.
        path: PathBuf,
        /// TOML error (boxed — `toml::de::Error` is large, keeping
        /// `ConfigError`/`Result` small; see clippy `result_large_err`).
        source: Box<toml::de::Error>,
    },
}

impl Config {
    /// Parse the process arguments/env, load any config file, resolve, validate,
    /// and preflight ffmpeg. This is the entry point for `main`.
    pub fn load() -> Result<Self, ConfigError> {
        let cli = Cli::parse();
        let file = load_file(cli.config.as_deref())?;
        let config = Self::resolve(&cli, &file)?;
        config.validate()?;
        preflight()?;
        Ok(config)
    }

    /// Merge CLI/env over TOML over defaults (pure — no filesystem or process
    /// checks). `validate`/`preflight` do the environment-touching work.
    pub fn resolve(cli: &Cli, file: &FileConfig) -> Result<Self, ConfigError> {
        let library = cli
            .library
            .clone()
            .or_else(|| file.library.clone())
            .ok_or(ConfigError::MissingLibrary)?;

        let data_dir = cli
            .data_dir
            .clone()
            .or_else(|| file.data_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));

        let bind_str = cli
            .bind
            .clone()
            .or_else(|| file.bind.clone())
            .unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_str.parse().map_err(|source| ConfigError::BadBind {
            value: bind_str.clone(),
            source,
        })?;

        let base_url = cli
            .base_url
            .clone()
            .or_else(|| file.base_url.clone())
            .unwrap_or_else(|| format!("http://localhost:{}", bind.port()))
            .trim_end_matches('/')
            .to_string();

        let default_cover_url = cli
            .default_cover_url
            .clone()
            .or_else(|| file.default_cover_url.clone());

        let force_embedded_chapters =
            cli.force_embedded_chapters || file.force_embedded_chapters.unwrap_or(false);

        let remux_non_faststart =
            cli.remux_non_faststart || file.remux_non_faststart.unwrap_or(false);

        let storage_mode = cli.storage_mode.or(file.storage_mode).unwrap_or_default();

        let cache_size_bytes = match cli.cache_size.clone().or_else(|| file.cache_size.clone()) {
            Some(s) => {
                parse_size(&s).map_err(|reason| ConfigError::BadCacheSize { value: s, reason })?
            }
            None => Some(DEFAULT_CACHE_SIZE_BYTES),
        };

        let cache_ttl = match cli.cache_ttl.clone().or_else(|| file.cache_ttl.clone()) {
            Some(s) => parse_duration(&s)
                .map_err(|reason| ConfigError::BadCacheTtl { value: s, reason })?,
            None => None,
        };

        Ok(Self {
            library,
            data_dir,
            bind,
            base_url,
            default_cover_url,
            force_embedded_chapters,
            remux_non_faststart,
            storage_mode,
            cache_size_bytes,
            cache_ttl,
        })
    }

    /// Check the library exists and is a directory, and create the data dir.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.library.exists() {
            return Err(ConfigError::LibraryNotFound(self.library.clone()));
        }
        if !self.library.is_dir() {
            return Err(ConfigError::LibraryNotDir(self.library.clone()));
        }
        std::fs::create_dir_all(&self.data_dir).map_err(|source| ConfigError::DataDir {
            path: self.data_dir.clone(),
            source,
        })?;
        Ok(())
    }
}

/// Load a TOML config file. `None` yields an empty config; an explicit path that
/// can't be read or parsed is a fatal error.
fn load_file(path: Option<&Path>) -> Result<FileConfig, ConfigError> {
    let Some(path) = path else {
        return Ok(FileConfig::default());
    };
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::ParseConfig {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Verify `ffmpeg` and `ffprobe` are on PATH by execing `-version`. Fails fast so
/// a missing toolchain is a startup error, not a mid-request surprise.
pub fn preflight() -> Result<(), ConfigError> {
    for tool in ["ffmpeg", "ffprobe"] {
        let ran = Command::new(tool)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ran {
            return Err(ConfigError::ToolMissing(tool));
        }
    }
    Ok(())
}

/// Split a value like `2gb` / `30d` into its numeric prefix and unit suffix.
fn split_number_unit(t: &str) -> (&str, &str) {
    let at = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (num, unit) = t.split_at(at);
    (num.trim(), unit.trim())
}

/// Parse a human byte size (`2GB`, `500mb`, `1048576`, `2 gib`) into bytes.
/// Units are binary (1024-based). `0`/`off`/`none`/`unbounded` → `None`
/// (no cap). Used for the `saver`-mode cache.
fn parse_size(s: &str) -> Result<Option<u64>, String> {
    let t = s.trim().to_ascii_lowercase();
    if matches!(
        t.as_str(),
        "" | "0" | "off" | "none" | "unbounded" | "unlimited"
    ) {
        return Ok(None);
    }
    let (num, unit) = split_number_unit(&t);
    let value: f64 = num.parse().map_err(|_| format!("not a number: {num:?}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("not a positive size: {s:?}"));
    }
    let mult: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => return Err(format!("unknown size unit {other:?} (use B/KB/MB/GB/TB)")),
    };
    let bytes = (value * mult as f64) as u64;
    // A positive-but-tiny value rounding to 0 bytes is a mistake, not "unbounded".
    Ok(if bytes == 0 { None } else { Some(bytes) })
}

/// Parse a human duration (`30d`, `12h`, `45m`, `90s`, `2w`) into a `Duration`.
/// `0`/`off`/`none`/`never` → `None` (no TTL). Used for `saver`-mode cache
/// eviction. Bare numbers are seconds.
fn parse_duration(s: &str) -> Result<Option<Duration>, String> {
    let t = s.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "" | "0" | "off" | "none" | "never") {
        return Ok(None);
    }
    let (num, unit) = split_number_unit(&t);
    let value: u64 = num
        .parse()
        .map_err(|_| format!("not a whole number: {num:?}"))?;
    let secs = match unit {
        "" | "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value.saturating_mul(60),
        "h" | "hr" | "hrs" => value.saturating_mul(3600),
        "d" | "day" | "days" => value.saturating_mul(86_400),
        "w" | "week" | "weeks" => value.saturating_mul(604_800),
        other => return Err(format!("unknown duration unit {other:?} (use s/m/h/d/w)")),
    };
    Ok(if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(library: Option<&str>) -> Cli {
        Cli {
            library: library.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn library_only_uses_defaults() {
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert_eq!(c.library, PathBuf::from("/books"));
        assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(c.bind, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.base_url, "http://localhost:8080");
    }

    #[test]
    fn missing_library_from_all_sources_errors() {
        let err = Config::resolve(&cli(None), &FileConfig::default()).unwrap_err();
        assert!(matches!(err, ConfigError::MissingLibrary));
    }

    #[test]
    fn library_can_come_from_the_file_layer() {
        let file = FileConfig {
            library: Some(PathBuf::from("/from-toml")),
            ..Default::default()
        };
        let c = Config::resolve(&cli(None), &file).unwrap();
        assert_eq!(c.library, PathBuf::from("/from-toml"));
    }

    #[test]
    fn cli_overrides_file() {
        let file = FileConfig {
            library: Some(PathBuf::from("/from-toml")),
            bind: Some("127.0.0.1:9000".to_string()),
            ..Default::default()
        };
        let mut c = cli(Some("/from-cli"));
        c.bind = Some("0.0.0.0:7000".to_string());
        let resolved = Config::resolve(&c, &file).unwrap();
        assert_eq!(resolved.library, PathBuf::from("/from-cli"));
        assert_eq!(resolved.bind, "0.0.0.0:7000".parse().unwrap());
    }

    #[test]
    fn base_url_defaults_to_the_bind_port_and_trims_slash() {
        let mut c = cli(Some("/books"));
        c.bind = Some("0.0.0.0:1234".to_string());
        assert_eq!(
            Config::resolve(&c, &FileConfig::default())
                .unwrap()
                .base_url,
            "http://localhost:1234"
        );

        c.base_url = Some("https://podspine.example.com/".to_string());
        assert_eq!(
            Config::resolve(&c, &FileConfig::default())
                .unwrap()
                .base_url,
            "https://podspine.example.com"
        );
    }

    #[test]
    fn default_cover_url_resolves_from_cli_over_file_and_defaults_none() {
        // Unset everywhere -> None.
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert_eq!(c.default_cover_url, None);

        // CLI wins over the TOML layer.
        let file = FileConfig {
            default_cover_url: Some("http://toml/cover.png".to_string()),
            ..Default::default()
        };
        let mut cl = cli(Some("/books"));
        cl.default_cover_url = Some("http://cli/cover.png".to_string());
        let resolved = Config::resolve(&cl, &file).unwrap();
        assert_eq!(
            resolved.default_cover_url.as_deref(),
            Some("http://cli/cover.png")
        );
    }

    #[test]
    fn bad_bind_address_is_rejected() {
        let mut c = cli(Some("/books"));
        c.bind = Some("not-an-address".to_string());
        assert!(matches!(
            Config::resolve(&c, &FileConfig::default()),
            Err(ConfigError::BadBind { .. })
        ));
    }

    #[test]
    fn toml_parses_a_partial_config() {
        let file: FileConfig = toml::from_str("bind = \"0.0.0.0:3000\"\n").unwrap();
        assert_eq!(file.bind.as_deref(), Some("0.0.0.0:3000"));
        assert!(file.library.is_none());
    }

    #[test]
    fn validate_rejects_a_missing_library() {
        let c = Config {
            library: PathBuf::from("/definitely/does/not/exist/12345"),
            data_dir: std::env::temp_dir().join("podspine-cfg-test"),
            bind: "0.0.0.0:8080".parse().unwrap(),
            base_url: "http://localhost:8080".to_string(),
            default_cover_url: None,
            force_embedded_chapters: false,
            remux_non_faststart: false,
            storage_mode: StorageMode::Full,
            cache_size_bytes: Some(DEFAULT_CACHE_SIZE_BYTES),
            cache_ttl: None,
        };
        assert!(matches!(c.validate(), Err(ConfigError::LibraryNotFound(_))));
    }

    #[test]
    fn validate_accepts_a_real_dir_and_creates_data_dir() {
        let tmp = std::env::temp_dir().join("podspine-cfg-validate");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let data = tmp.join("data");
        let c = Config {
            library: tmp.clone(),
            data_dir: data.clone(),
            bind: "0.0.0.0:8080".parse().unwrap(),
            base_url: "http://localhost:8080".to_string(),
            default_cover_url: None,
            force_embedded_chapters: false,
            remux_non_faststart: false,
            storage_mode: StorageMode::Full,
            cache_size_bytes: Some(DEFAULT_CACHE_SIZE_BYTES),
            cache_ttl: None,
        };
        c.validate().unwrap();
        assert!(data.is_dir(), "data dir created");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn preflight_finds_ffmpeg() {
        // CI and dev both have ffmpeg on PATH.
        preflight().expect("ffmpeg/ffprobe present");
    }

    #[test]
    fn resolve_takes_data_dir_bind_base_url_from_the_file_layer() {
        let file = FileConfig {
            library: Some(PathBuf::from("/lib")),
            data_dir: Some(PathBuf::from("/from-toml-data")),
            bind: Some("127.0.0.1:9999".to_string()),
            base_url: Some("https://toml.example/".to_string()),
            default_cover_url: Some("https://toml/cover.png".to_string()),
            force_embedded_chapters: Some(true),
            ..Default::default()
        };
        let c = Config::resolve(&cli(None), &file).unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/from-toml-data"));
        assert_eq!(c.bind, "127.0.0.1:9999".parse().unwrap());
        assert_eq!(c.base_url, "https://toml.example"); // trailing slash trimmed
        assert_eq!(
            c.default_cover_url.as_deref(),
            Some("https://toml/cover.png")
        );
        assert!(c.force_embedded_chapters);
    }

    #[test]
    fn storage_mode_defaults_to_full_and_cache_defaults_apply() {
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert_eq!(c.storage_mode, StorageMode::Full);
        assert_eq!(c.cache_size_bytes, Some(DEFAULT_CACHE_SIZE_BYTES));
        assert_eq!(c.cache_ttl, None);
    }

    #[test]
    fn remux_non_faststart_defaults_off_and_resolves_from_either_layer() {
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert!(!c.remux_non_faststart, "default off");

        let file = FileConfig {
            remux_non_faststart: Some(true),
            ..Default::default()
        };
        assert!(
            Config::resolve(&cli(Some("/books")), &file)
                .unwrap()
                .remux_non_faststart,
            "enabled from the file layer"
        );

        let mut cl = cli(Some("/books"));
        cl.remux_non_faststart = true;
        assert!(
            Config::resolve(&cl, &FileConfig::default())
                .unwrap()
                .remux_non_faststart,
            "enabled from the CLI flag"
        );
    }

    #[test]
    fn storage_knobs_resolve_from_cli_over_file() {
        let file = FileConfig {
            storage_mode: Some(StorageMode::Full),
            cache_size: Some("1GB".to_string()),
            cache_ttl: Some("7d".to_string()),
            ..Default::default()
        };
        let mut cl = cli(Some("/books"));
        cl.storage_mode = Some(StorageMode::Saver);
        cl.cache_size = Some("512MB".to_string());
        cl.cache_ttl = Some("30d".to_string());
        let c = Config::resolve(&cl, &file).unwrap();
        assert_eq!(c.storage_mode, StorageMode::Saver);
        assert_eq!(c.cache_size_bytes, Some(512 * 1024 * 1024));
        assert_eq!(c.cache_ttl, Some(Duration::from_secs(30 * 86_400)));
    }

    #[test]
    fn storage_knobs_resolve_from_the_file_layer() {
        let file = FileConfig {
            storage_mode: Some(StorageMode::Saver),
            cache_size: Some("off".to_string()),
            cache_ttl: Some("12h".to_string()),
            ..Default::default()
        };
        let c = Config::resolve(&cli(Some("/books")), &file).unwrap();
        assert_eq!(c.storage_mode, StorageMode::Saver);
        assert_eq!(c.cache_size_bytes, None); // "off" = unbounded
        assert_eq!(c.cache_ttl, Some(Duration::from_secs(12 * 3600)));
    }

    #[test]
    fn parse_size_handles_units_and_unbounded() {
        assert_eq!(parse_size("2GB").unwrap(), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("500 mb").unwrap(), Some(500 * 1024 * 1024));
        assert_eq!(parse_size("1048576").unwrap(), Some(1_048_576));
        assert_eq!(parse_size("1.5gb").unwrap(), Some(1_610_612_736));
        for unbounded in ["0", "off", "none", "unbounded", ""] {
            assert_eq!(parse_size(unbounded).unwrap(), None, "{unbounded:?}");
        }
        assert!(parse_size("banana").is_err());
        assert!(parse_size("10 zb").is_err());
        assert!(parse_size("-5gb").is_err(), "negative size rejected");
    }

    #[test]
    fn parse_duration_handles_units_and_off() {
        assert_eq!(
            parse_duration("30d").unwrap(),
            Some(Duration::from_secs(2_592_000))
        );
        assert_eq!(
            parse_duration("12h").unwrap(),
            Some(Duration::from_secs(43_200))
        );
        assert_eq!(parse_duration("90").unwrap(), Some(Duration::from_secs(90)));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Some(Duration::from_secs(1_209_600))
        );
        for off in ["0", "off", "none", "never", ""] {
            assert_eq!(parse_duration(off).unwrap(), None, "{off:?}");
        }
        assert!(parse_duration("5 fortnights").is_err());
        assert!(parse_duration("1.5h").is_err()); // whole numbers only
        assert_eq!(
            parse_duration("0h").unwrap(),
            None,
            "zero with a unit is no-TTL"
        );
    }

    #[test]
    fn bad_cache_size_and_ttl_are_config_errors() {
        let mut cl = cli(Some("/books"));
        cl.cache_size = Some("lots".to_string());
        assert!(matches!(
            Config::resolve(&cl, &FileConfig::default()),
            Err(ConfigError::BadCacheSize { .. })
        ));
        let mut cl = cli(Some("/books"));
        cl.cache_ttl = Some("soon".to_string());
        assert!(matches!(
            Config::resolve(&cl, &FileConfig::default()),
            Err(ConfigError::BadCacheTtl { .. })
        ));
    }

    #[test]
    fn load_file_none_is_the_empty_default() {
        let f = load_file(None).unwrap();
        assert!(f.library.is_none() && f.bind.is_none());
    }

    #[test]
    fn load_file_reads_a_toml_file() {
        let dir = std::env::temp_dir().join("podspine-cfg-load");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("podspine.toml");
        std::fs::write(&path, "library = \"/books\"\nbind = \"0.0.0.0:3000\"\n").unwrap();
        let f = load_file(Some(&path)).unwrap();
        assert_eq!(f.library, Some(PathBuf::from("/books")));
        assert_eq!(f.bind.as_deref(), Some("0.0.0.0:3000"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_file_missing_path_is_a_read_error() {
        let err = load_file(Some(Path::new("/no/such/dir/podspine.toml"))).unwrap_err();
        assert!(matches!(err, ConfigError::ReadConfig { .. }));
    }

    #[test]
    fn load_file_malformed_is_a_parse_error() {
        let dir = std::env::temp_dir().join("podspine-cfg-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "this is = not valid = toml").unwrap();
        let err = load_file(Some(&path)).unwrap_err();
        assert!(matches!(err, ConfigError::ParseConfig { .. }));
        let _ = std::fs::remove_file(&path);
    }
}
