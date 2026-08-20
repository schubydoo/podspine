//! `config` — resolve runtime configuration from CLI flags, environment, and an
//! optional TOML file (in that precedence), and preflight the ffmpeg toolchain.
//!
//! The library path is the only required input; everything else has a default,
//! so `podspine --library ./books` just works. Failures (missing library,
//! unparsable bind address, absent ffmpeg/ffprobe) surface as a clear fatal
//! error at startup, never mid-request. See TAD §4.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::Deserialize;

pub mod book_overrides;
pub use book_overrides::BookOverrides;

const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_DATA_DIR: &str = "./data";
/// Default `saver`-mode cache cap when unset: 2 GiB.
const DEFAULT_CACHE_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How a **chaptered** book's per-chapter episodes are produced and stored.
/// Whole-file episodes (MP3-folder tracks, chapterless single files) ignore
/// this: they are streamed in place from the library, never extracted
/// (Sprint 6.2).
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

impl StorageMode {
    /// The canonical label for this mode (`"full"`/`"saver"`): the TOML/CLI
    /// spelling, and the TEXT the index persists in `book.storage_mode`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            StorageMode::Full => "full",
            StorageMode::Saver => "saver",
        }
    }

    /// Inverse of [`StorageMode::label`]; `None` for anything else.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "full" => Some(StorageMode::Full),
            "saver" => Some(StorageMode::Saver),
            _ => None,
        }
    }
}

/// Whether to re-encode sources that no podcatcher can play, and to what
/// (Task 5.2).
///
/// Off by default: Podspine is copy-first, and a re-encode both costs CPU at
/// ingest and loses quality. Turn it on for a library of FLAC/Ogg/Opus/ALAC
/// books that clients refuse to play. Podcast-safe sources (MP3, AAC) are
/// never re-encoded, independent of this setting.
///
/// A transcoded book is always stored `full`, even under
/// `storage_mode = "saver"`: a re-encode is not byte-reproducible across
/// ffmpeg builds, so a chapter regenerated on demand could serve bytes whose
/// length no longer matches the `enclosure length` already published in the
/// feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscodeMode {
    /// Never re-encode: serve every source stream-copied (the default).
    #[default]
    Off,
    /// Re-encode non-podcast-safe sources to AAC 128 kbps (`.m4a`).
    Aac,
    /// Re-encode non-podcast-safe sources to MP3 128 kbps: the fallback for
    /// clients that still do not play AAC. This needs an ffmpeg built with
    /// `libmp3lame`.
    Mp3,
}

impl TranscodeMode {
    /// Whether this mode re-encodes anything at all.
    #[must_use]
    pub fn is_on(self) -> bool {
        !matches!(self, TranscodeMode::Off)
    }

    /// The canonical label for this mode (`"off"`/`"aac"`/`"mp3"`): the
    /// TOML/CLI spelling, and the TEXT the index persists in `book.transcode`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            TranscodeMode::Off => "off",
            TranscodeMode::Aac => "aac",
            TranscodeMode::Mp3 => "mp3",
        }
    }

    /// Inverse of [`TranscodeMode::label`]; `None` for anything else.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "off" => Some(TranscodeMode::Off),
            "aac" => Some(TranscodeMode::Aac),
            "mp3" => Some(TranscodeMode::Mp3),
            _ => None,
        }
    }
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
    /// Remux a non-faststart whole-file mp4 (`moov` after `mdat`) to faststart
    /// on demand, so that podcast clients seek quickly, instead of serving it
    /// in place. The remux is stream-copied and cache-managed (it counts
    /// against the `saver` cache cap), never a pinned duplicate. Default off:
    /// serve in place and log a notice.
    #[arg(long, env = "PODSPINE_REMUX_NON_FASTSTART")]
    pub remux_non_faststart: bool,
    /// Chapter storage strategy for **chaptered** books: `full` pre-splits every
    /// chapter to disk (fast, ~2× storage); `saver` still splits at ingest but
    /// keeps chapters in a bounded on-demand cache (minimal steady-state disk, a
    /// small first-play delay). No effect on whole-file episodes (MP3 folders,
    /// chapterless singles), which are served in place from the library.
    #[arg(long, env = "PODSPINE_STORAGE_MODE")]
    pub storage_mode: Option<StorageMode>,
    /// Re-encode sources that podcatchers cannot play (FLAC/Ogg/Opus/ALAC) to
    /// `aac` 128k or `mp3` 128k; `off` (default) serves every source
    /// stream-copied. MP3/AAC sources are never re-encoded. A transcoded book
    /// is always stored `full` (a re-encode cannot be regenerated
    /// byte-for-byte on demand).
    #[arg(long, env = "PODSPINE_TRANSCODE")]
    pub transcode: Option<TranscodeMode>,
    /// Max disk for the on-demand chapter cache in `saver` mode (e.g. `2GB`,
    /// `500MB`; `0`/`off` = unbounded). Ignored in `full` mode.
    #[arg(long, env = "PODSPINE_CACHE_SIZE")]
    pub cache_size: Option<String>,
    /// TTL for cached chapters in `saver` mode (e.g. `30d`, `12h`; `off` =
    /// size-only eviction). Ignored in `full` mode.
    #[arg(long, env = "PODSPINE_CACHE_TTL")]
    pub cache_ttl: Option<String>,
    /// Serve Prometheus metrics on this address, e.g. `127.0.0.1:9090`. Unset
    /// means no metrics endpoint and no recorder (the instrumentation compiles
    /// to no-ops). This is deliberately a *separate* listener from `--bind`:
    /// metrics expose operator data (library size, error counts) that has no
    /// business on the same surface as internet-facing feeds. Bind it to
    /// loopback or the LAN.
    #[arg(long, env = "PODSPINE_METRICS_BIND")]
    pub metrics_bind: Option<String>,
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
    /// Re-encode non-podcast-safe sources (`off` | `aac` | `mp3`).
    pub transcode: Option<TranscodeMode>,
    /// On-demand cache size cap (e.g. `2GB`; `0`/`off` = unbounded).
    pub cache_size: Option<String>,
    /// On-demand cache TTL (e.g. `30d`; `off` = size-only eviction).
    pub cache_ttl: Option<String>,
    /// Address for the separate Prometheus metrics listener (unset = disabled).
    pub metrics_bind: Option<String>,
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
    /// Remux a non-faststart whole-file mp4 (`moov` after `mdat`) to faststart
    /// on demand, instead of serving it in place, so that podcast clients seek
    /// quickly. The remuxed copy is stream-copied and cache-managed (evicted
    /// under the `saver` cap, regenerated on demand), never a pinned
    /// duplicate. Default off: such books serve in place, and the ingest logs
    /// a one-line notice. There is no effect on faststart mp4, MP3/OGG/FLAC,
    /// or chaptered books (Sprint 6.3).
    pub remux_non_faststart: bool,
    /// How **chaptered** books are produced/stored: `full` (pre-split every
    /// chapter to disk) or `saver` (split at ingest, then cache regenerations
    /// on demand). This applies library-wide to chaptered books, which
    /// materialize under `data_dir`. Whole-file episodes (MP3-folder tracks,
    /// chapterless single files) are unaffected: they stream in place from the
    /// library (Sprint 6.2).
    pub storage_mode: StorageMode,
    /// Whether non-podcast-safe sources (FLAC/Vorbis/Opus/ALAC) are re-encoded
    /// at ingest, and to what (Task 5.2). `Off` by default: Podspine is
    /// copy-first. MP3/AAC sources are never re-encoded. A transcoded book is
    /// materialized `full`, independent of `storage_mode`; see
    /// [`TranscodeMode`].
    pub transcode: TranscodeMode,
    /// Cache size cap in bytes for `saver` mode (`None` = unbounded).
    pub cache_size_bytes: Option<u64>,
    /// TTL for cached chapters in `saver` mode (`None` = size-only eviction).
    pub cache_ttl: Option<Duration>,
    /// Address for the Prometheus metrics listener (`None` = metrics disabled,
    /// no recorder installed). Always a second listener, never `bind`; see the
    /// `Cli::metrics_bind` note on why this surface is kept separate.
    pub metrics_bind: Option<SocketAddr>,
}

/// Configuration failures: all fatal, all reported at startup.
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
    /// The metrics bind address could not be parsed.
    #[error("invalid metrics bind address {value:?}: {source}")]
    BadMetricsBind {
        /// The offending value.
        value: String,
        /// Parse error.
        source: std::net::AddrParseError,
    },
    /// The metrics listener would contend with the feed server for a port.
    #[error(
        "metrics bind address {metrics} collides with the feed server's {bind}; \
         give metrics its own port (e.g. 127.0.0.1:9090)"
    )]
    MetricsBindConflict {
        /// The requested metrics address.
        metrics: SocketAddr,
        /// The feed server's address.
        bind: SocketAddr,
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
        /// TOML error (boxed: `toml::de::Error` is large, and the box keeps
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

    /// Merge CLI/env over TOML over defaults (pure: no filesystem or process
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

        let transcode = cli.transcode.or(file.transcode).unwrap_or_default();

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

        let metrics_bind = match cli
            .metrics_bind
            .clone()
            .or_else(|| file.metrics_bind.clone())
        {
            Some(s) => Some(
                s.parse()
                    .map_err(|source| ConfigError::BadMetricsBind { value: s, source })?,
            ),
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
            transcode,
            cache_size_bytes,
            cache_ttl,
            metrics_bind,
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
        // Catch the metrics/feed surface collision here, not as an opaque
        // "address in use" from the second listener half a second into
        // startup.
        if let Some(metrics) = self.metrics_bind
            && binds_collide(metrics, self.bind)
        {
            return Err(ConfigError::MetricsBindConflict {
                metrics,
                bind: self.bind,
            });
        }
        std::fs::create_dir_all(&self.data_dir).map_err(|source| ConfigError::DataDir {
            path: self.data_dir.clone(),
            source,
        })?;
        Ok(())
    }
}

/// Load a TOML config file. `None` yields an empty config; an explicit path
/// that cannot be read or parsed is a fatal error.
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

/// Whether two listeners would fight over the same port.
///
/// Equality is not enough, for two reasons:
///
/// - A wildcard bind claims every interface, so `0.0.0.0:8080` and
///   `127.0.0.1:8080` collide even though the addresses differ.
/// - Rust compares `IpAddr` by variant, so the IPv4-mapped form of an address
///   (`[::ffff:127.0.0.1]` vs `127.0.0.1`) reads as different despite being the
///   same socket to the kernel.
///
/// Either miss produces the bare "address already in use" that this check
/// exists to replace, so normalize mapped addresses first, then treat a shared
/// port as a collision when the addresses match or either side is a wildcard.
/// Deliberately conservative: refusing an exotic-but-legal pair costs the
/// operator one flag change, while missing one costs them a confusing crash.
fn binds_collide(a: SocketAddr, b: SocketAddr) -> bool {
    let (ip_a, ip_b) = (canonical_ip(a.ip()), canonical_ip(b.ip()));
    a.port() == b.port() && (ip_a == ip_b || ip_a.is_unspecified() || ip_b.is_unspecified())
}

/// Fold an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) down to its IPv4 form so
/// the two spellings of one address compare equal. Everything else is returned
/// unchanged.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// Verify that `ffmpeg` and `ffprobe` are on PATH: exec `-version`. This
/// fails fast, so a missing toolchain is a startup error, not a mid-request
/// surprise.
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
    // A positive but tiny value that rounds to 0 bytes is a mistake, not
    // "unbounded".
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
    use podspine_test_support::scratch;

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
        // Unset everywhere gives `None`.
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
    fn metrics_are_off_unless_a_bind_is_given() {
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert_eq!(c.metrics_bind, None);
    }

    #[test]
    fn metrics_bind_is_parsed_and_cli_beats_toml() {
        let mut c = cli(Some("/books"));
        c.metrics_bind = Some("127.0.0.1:9090".to_string());
        let file = FileConfig {
            metrics_bind: Some("0.0.0.0:9999".to_string()),
            ..FileConfig::default()
        };
        let resolved = Config::resolve(&c, &file).unwrap();
        assert_eq!(
            resolved.metrics_bind,
            Some("127.0.0.1:9090".parse().unwrap())
        );
    }

    #[test]
    fn metrics_bind_falls_back_to_toml() {
        let file = FileConfig {
            metrics_bind: Some("127.0.0.1:9090".to_string()),
            ..FileConfig::default()
        };
        let resolved = Config::resolve(&cli(Some("/books")), &file).unwrap();
        assert_eq!(
            resolved.metrics_bind,
            Some("127.0.0.1:9090".parse().unwrap())
        );
    }

    #[test]
    fn bad_metrics_bind_address_is_rejected() {
        let mut c = cli(Some("/books"));
        c.metrics_bind = Some("nope".to_string());
        assert!(matches!(
            Config::resolve(&c, &FileConfig::default()),
            Err(ConfigError::BadMetricsBind { .. })
        ));
    }

    /// `validate` needs a real library dir; give each case its own.
    fn validate_binds(dir: &str, bind: &str, metrics: &str) -> Result<(), ConfigError> {
        let tmp = scratch(dir);
        let mut c = cli(Some(tmp.to_str().unwrap()));
        c.bind = Some(bind.to_string());
        c.metrics_bind = Some(metrics.to_string());
        let resolved = Config::resolve(&c, &FileConfig::default()).unwrap();
        resolved.validate()
    }

    #[test]
    fn metrics_bind_may_not_reuse_the_feed_servers_address() {
        assert!(matches!(
            validate_binds("cfg-collide-exact", "127.0.0.1:8080", "127.0.0.1:8080"),
            Err(ConfigError::MetricsBindConflict { .. })
        ));
    }

    #[test]
    fn a_wildcard_feed_bind_collides_with_a_specific_metrics_bind() {
        // 0.0.0.0:8080 already claims 127.0.0.1:8080. Without this check, the
        // second listener dies with a bare "address already in use" at
        // startup.
        assert!(matches!(
            validate_binds("cfg-collide-wild", "0.0.0.0:8080", "127.0.0.1:8080"),
            Err(ConfigError::MetricsBindConflict { .. })
        ));
    }

    #[test]
    fn a_wildcard_metrics_bind_collides_with_a_specific_feed_bind() {
        assert!(matches!(
            validate_binds("cfg-collide-wild-rev", "127.0.0.1:8080", "0.0.0.0:8080"),
            Err(ConfigError::MetricsBindConflict { .. })
        ));
    }

    #[test]
    fn different_ports_never_collide() {
        assert!(
            validate_binds("cfg-collide-ok", "0.0.0.0:8080", "0.0.0.0:9090").is_ok(),
            "distinct ports must be allowed, wildcard or not"
        );
    }

    #[test]
    fn distinct_interfaces_on_one_port_are_allowed() {
        // Legal and occasionally deliberate: two specific, different interfaces.
        assert!(validate_binds("cfg-collide-ifaces", "127.0.0.1:8080", "192.0.2.1:8080").is_ok());
    }

    #[test]
    fn collision_check_is_symmetric_and_port_scoped() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let wildcard: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let other_port: SocketAddr = "0.0.0.0:9090".parse().unwrap();
        let v6_wildcard: SocketAddr = "[::]:8080".parse().unwrap();

        assert!(binds_collide(loopback, wildcard));
        assert!(binds_collide(wildcard, loopback));
        assert!(binds_collide(loopback, loopback));
        assert!(binds_collide(v6_wildcard, loopback));
        assert!(!binds_collide(loopback, other_port));
        assert!(!binds_collide(wildcard, other_port));
    }

    #[test]
    fn ipv4_mapped_ipv6_is_the_same_address_as_its_ipv4_form() {
        // `IpAddr` compares by variant, so these read as different addresses in
        // Rust while the kernel binds them to one socket.
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:8080".parse().unwrap();
        assert!(binds_collide(mapped, loopback));
        assert!(binds_collide(loopback, mapped));

        // The mapped wildcard is still a wildcard once folded.
        let mapped_wildcard: SocketAddr = "[::ffff:0.0.0.0]:8080".parse().unwrap();
        assert!(binds_collide(mapped_wildcard, loopback));

        // Folding must not make unrelated addresses collide.
        let elsewhere: SocketAddr = "[::ffff:192.0.2.1]:8080".parse().unwrap();
        assert!(!binds_collide(elsewhere, loopback));
        // It must also not override the port check.
        let mapped_other_port: SocketAddr = "[::ffff:127.0.0.1]:9090".parse().unwrap();
        assert!(!binds_collide(mapped_other_port, loopback));
    }

    #[test]
    fn a_real_ipv6_address_is_left_alone() {
        // Only the `::ffff:` mapped range folds; genuine IPv6 stays distinct.
        let v6: SocketAddr = "[2001:db8::1]:8080".parse().unwrap();
        let v4: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(!binds_collide(v6, v4));
        assert!(binds_collide(v6, v6));
    }

    #[test]
    fn mapped_metrics_bind_is_rejected_end_to_end() {
        assert!(matches!(
            validate_binds(
                "cfg-collide-mapped",
                "127.0.0.1:8080",
                "[::ffff:127.0.0.1]:8080"
            ),
            Err(ConfigError::MetricsBindConflict { .. })
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
            transcode: TranscodeMode::Off,
            cache_size_bytes: Some(DEFAULT_CACHE_SIZE_BYTES),
            cache_ttl: None,
            metrics_bind: None,
        };
        assert!(matches!(c.validate(), Err(ConfigError::LibraryNotFound(_))));
    }

    #[test]
    fn validate_accepts_a_real_dir_and_creates_data_dir() {
        let tmp = scratch("cfg-validate");
        let data = tmp.join("data");
        let c = Config {
            library: tmp.to_path_buf(),
            data_dir: data.clone(),
            bind: "0.0.0.0:8080".parse().unwrap(),
            base_url: "http://localhost:8080".to_string(),
            default_cover_url: None,
            force_embedded_chapters: false,
            remux_non_faststart: false,
            storage_mode: StorageMode::Full,
            transcode: TranscodeMode::Off,
            cache_size_bytes: Some(DEFAULT_CACHE_SIZE_BYTES),
            cache_ttl: None,
            metrics_bind: None,
        };
        c.validate().unwrap();
        assert!(data.is_dir(), "data dir created");
    }

    #[test]
    fn preflight_matches_ffmpeg_availability() {
        // ffmpeg/ffprobe are not on every runner (e.g. the informational
        // Windows leg, or a bare dev box), so do not hard-require them.
        // Instead, assert that preflight's result MATCHES whether they are
        // actually invocable.
        let have = ["ffmpeg", "ffprobe"].iter().all(|t| {
            std::process::Command::new(t)
                .arg("-version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
        match preflight() {
            Ok(()) => assert!(
                have,
                "preflight passed but ffmpeg/ffprobe are not both present"
            ),
            Err(ConfigError::ToolMissing(_)) => {
                assert!(
                    !have,
                    "preflight reported a tool missing but both are present"
                )
            }
            Err(e) => panic!("unexpected preflight error: {e:?}"),
        }
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
    fn transcode_defaults_off_and_resolves_cli_over_file() {
        let c = Config::resolve(&cli(Some("/books")), &FileConfig::default()).unwrap();
        assert_eq!(c.transcode, TranscodeMode::Off, "copy-first by default");
        assert!(!c.transcode.is_on());

        let file = FileConfig {
            transcode: Some(TranscodeMode::Aac),
            ..Default::default()
        };
        let c = Config::resolve(&cli(Some("/books")), &file).unwrap();
        assert_eq!(c.transcode, TranscodeMode::Aac, "set from the file layer");
        assert!(c.transcode.is_on());

        // CLI/env wins over the file layer.
        let mut cl = cli(Some("/books"));
        cl.transcode = Some(TranscodeMode::Mp3);
        assert_eq!(
            Config::resolve(&cl, &file).unwrap().transcode,
            TranscodeMode::Mp3
        );
    }

    #[test]
    fn mode_labels_are_pinned_and_round_trip() {
        // These labels are persisted in the index (`book.storage_mode` /
        // `book.transcode`), so a change to one would re-ingest every library
        // on upgrade. Pin all values.
        for (mode, label) in [(StorageMode::Full, "full"), (StorageMode::Saver, "saver")] {
            assert_eq!(mode.label(), label);
            assert_eq!(StorageMode::from_label(label), Some(mode));
        }
        for (mode, label) in [
            (TranscodeMode::Off, "off"),
            (TranscodeMode::Aac, "aac"),
            (TranscodeMode::Mp3, "mp3"),
        ] {
            assert_eq!(mode.label(), label);
            assert_eq!(TranscodeMode::from_label(label), Some(mode));
        }
        // Unknown or empty labels do not parse (the index reads them as None).
        assert_eq!(StorageMode::from_label(""), None);
        assert_eq!(StorageMode::from_label("Saver"), None);
        assert_eq!(TranscodeMode::from_label(""), None);
        assert_eq!(TranscodeMode::from_label("flac"), None);
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
    fn validate_rejects_a_missing_or_non_dir_library() {
        // Own subdir: it must NOT share a fixed path with any sibling test.
        // Otherwise the parallel runner races `scratch()`'s wipe-and-recreate
        // against `validate_accepts_a_real_dir_and_creates_data_dir` (which
        // uses `-validate`).
        let tmp = scratch("cfg-validate-reject");

        // --library points at a real FILE (not a directory), which gives
        // `LibraryNotDir`.
        let as_file = tmp.join("not-a-dir");
        std::fs::write(&as_file, b"x").unwrap();
        let c = Config::resolve(
            &cli(Some(as_file.to_str().unwrap())),
            &FileConfig::default(),
        )
        .unwrap();
        assert!(matches!(c.validate(), Err(ConfigError::LibraryNotDir(_))));

        // A missing --library gives `LibraryNotFound`.
        let missing = tmp.join("nope");
        let c2 = Config::resolve(
            &cli(Some(missing.to_str().unwrap())),
            &FileConfig::default(),
        )
        .unwrap();
        assert!(matches!(
            c2.validate(),
            Err(ConfigError::LibraryNotFound(_))
        ));

        // A valid dir library validates OK and creates the data dir (kept under
        // tmp so the test never writes `./data` into the repo).
        let mut cl = cli(Some(tmp.to_str().unwrap()));
        cl.data_dir = Some(tmp.join("data"));
        let ok = Config::resolve(&cl, &FileConfig::default()).unwrap();
        assert!(ok.validate().is_ok());
        assert!(ok.data_dir.is_dir(), "data dir created by validate");
    }

    #[test]
    fn validate_reports_data_dir_when_the_path_is_unwritable() {
        // library is a real dir (passes exists/is_dir), but data_dir sits
        // UNDER a regular file, so create_dir_all fails: `ConfigError::DataDir`.
        let tmp = scratch("cfg-datadir");
        let blocker = tmp.join("iam-a-file");
        std::fs::write(&blocker, b"x").unwrap();
        let mut cl = cli(Some(tmp.to_str().unwrap()));
        cl.data_dir = Some(blocker.join("data")); // parent is a file -> create fails
        let c = Config::resolve(&cl, &FileConfig::default()).unwrap();
        assert!(
            matches!(c.validate(), Err(ConfigError::DataDir { .. })),
            "{:?}",
            c.validate()
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
        let dir = scratch("cfg-load");
        let path = dir.join("podspine.toml");
        std::fs::write(&path, "library = \"/books\"\nbind = \"0.0.0.0:3000\"\n").unwrap();
        let f = load_file(Some(&path)).unwrap();
        assert_eq!(f.library, Some(PathBuf::from("/books")));
        assert_eq!(f.bind.as_deref(), Some("0.0.0.0:3000"));
    }

    #[test]
    fn load_file_missing_path_is_a_read_error() {
        let err = load_file(Some(Path::new("/no/such/dir/podspine.toml"))).unwrap_err();
        assert!(matches!(err, ConfigError::ReadConfig { .. }));
    }

    #[test]
    fn load_file_malformed_is_a_parse_error() {
        let dir = scratch("cfg-bad");
        let path = dir.join("bad.toml");
        std::fs::write(&path, "this is = not valid = toml").unwrap();
        let err = load_file(Some(&path)).unwrap_err();
        assert!(matches!(err, ConfigError::ParseConfig { .. }));
    }
}
