//! Daemon configuration: defaults → TOML → env → CLI.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use aa_link::{
    AOA_DEFAULT_PATH, AOA_INTER_CHUNK_DELAY_MS, AOA_MAX_CHUNK, TTY_BAUD, TTY_DEFAULT_PATH,
};
use aa_registers::UnitId;
use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

/// Default bind address (local-only, architecture default).
pub(crate) const DEFAULT_BIND: &str = "127.0.0.1:2026";

/// Default AOA chunk size ([`aa_link::AOA_MAX_CHUNK`]).
pub(crate) const DEFAULT_AOA_CHUNK_SIZE: usize = AOA_MAX_CHUNK;

/// Default inter-chunk delay in milliseconds ([`aa_link::AOA_INTER_CHUNK_DELAY_MS`]).
pub(crate) const DEFAULT_AOA_CHUNK_DELAY_MS: u64 = AOA_INTER_CHUNK_DELAY_MS;

/// Default TTY baud rate ([`aa_link::TTY_BAUD`]).
pub(crate) const DEFAULT_TTY_BAUD: u32 = TTY_BAUD;

/// Default tracing level when `RUST_LOG` is unset.
pub(crate) const DEFAULT_LOG_LEVEL: &str = "info";

const ETC_CONFIG: &str = "/etc/cb-daemon/config.toml";
const CWD_CONFIG: &str = "./config.toml";

/// Link backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// In-memory [`aa_link::MockLink`] + negotiate/dump feeder (never opens accessory).
    Mock,
    /// Raw `/dev/usb_accessory` ([`aa_link::AoaLink`]).
    Aoa,
    /// USB-serial / USB-RS485 TTY ([`aa_link::TtyLink`]).
    Tty,
}

impl FromStr for Backend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_backend(s)
    }
}

impl Backend {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Aoa => "aoa",
            Self::Tty => "tty",
        }
    }
}

fn parse_backend(s: &str) -> Result<Backend> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mock" => Ok(Backend::Mock),
        "aoa" => Ok(Backend::Aoa),
        "tty" => Ok(Backend::Tty),
        other => bail!("unknown backend `{other}`; expected mock, aoa, or tty"),
    }
}

/// Merged runtime configuration for `cb-daemon`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Byte-link backend.
    pub backend: Backend,
    /// Device path for `aoa` / `tty` (crate defaults when omitted).
    pub device: Option<PathBuf>,
    /// HTTP / WebSocket bind address.
    pub bind: SocketAddr,
    /// Optional 20-bit unit id hint (logged at startup; unused by engine in D9).
    pub unit_id_hint: Option<UnitId>,
    /// Tracing filter level when `RUST_LOG` is unset.
    pub log_level: String,
    /// AOA max payload chunk size (≥ 1).
    pub aoa_chunk_size: usize,
    /// AOA inter-chunk delay in milliseconds.
    pub aoa_chunk_delay_ms: u64,
    /// TTY baud rate (> 0).
    pub tty_baud: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Mock,
            device: None,
            bind: default_bind(),
            unit_id_hint: None,
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            aoa_chunk_size: DEFAULT_AOA_CHUNK_SIZE,
            aoa_chunk_delay_ms: DEFAULT_AOA_CHUNK_DELAY_MS,
            tty_baud: DEFAULT_TTY_BAUD,
        }
    }
}

fn default_bind() -> SocketAddr {
    DEFAULT_BIND
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 2026)))
}

/// Optional fields from a TOML config file.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// `mock` | `aoa` | `tty`.
    pub backend: Option<String>,
    /// Device path override.
    pub device: Option<PathBuf>,
    /// Bind address string (e.g. `127.0.0.1:2026`).
    pub bind: Option<String>,
    /// Five-hex unit id hint.
    pub unit_id_hint: Option<String>,
    /// Tracing level.
    pub log_level: Option<String>,
    /// AOA chunk size.
    pub aoa_chunk_size: Option<usize>,
    /// AOA inter-chunk delay (ms).
    pub aoa_chunk_delay_ms: Option<u64>,
    /// TTY baud.
    pub tty_baud: Option<u32>,
}

/// CLI flags (only applied when present).
#[derive(Debug, Clone, Default, Parser, PartialEq, Eq)]
#[command(name = "cb-daemon", about = "Control Box mailbox sync daemon")]
pub struct CliArgs {
    /// Path to TOML config (`CB_DAEMON_CONFIG` when omitted).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Byte-link backend.
    #[arg(long, value_enum)]
    pub backend: Option<Backend>,

    /// Device path for `aoa` / `tty`.
    #[arg(long)]
    pub device: Option<PathBuf>,

    /// HTTP / WebSocket bind address.
    #[arg(long)]
    pub bind: Option<SocketAddr>,

    /// Tracing level (`error`/`warn`/`info`/`debug`/`trace`).
    #[arg(long)]
    pub log_level: Option<String>,

    /// Optional 5-hex unit id hint.
    #[arg(long)]
    pub unit_id_hint: Option<String>,

    /// AOA max chunk size.
    #[arg(long)]
    pub aoa_chunk_size: Option<usize>,

    /// AOA inter-chunk delay in milliseconds.
    #[arg(long)]
    pub aoa_chunk_delay_ms: Option<u64>,

    /// TTY baud rate.
    #[arg(long)]
    pub tty_baud: Option<u32>,
}

/// Environment overlay (manual; clear precedence over clap env feature).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    /// `CB_DAEMON_CONFIG`
    pub config: Option<PathBuf>,
    /// `CB_DAEMON_BACKEND`
    pub backend: Option<String>,
    /// `CB_DAEMON_DEVICE`
    pub device: Option<PathBuf>,
    /// `CB_DAEMON_BIND`
    pub bind: Option<String>,
    /// `CB_DAEMON_LOG_LEVEL`
    pub log_level: Option<String>,
    /// `CB_DAEMON_UNIT_ID_HINT`
    pub unit_id_hint: Option<String>,
    /// `CB_DAEMON_AOA_CHUNK_SIZE`
    pub aoa_chunk_size: Option<String>,
    /// `CB_DAEMON_AOA_CHUNK_DELAY_MS`
    pub aoa_chunk_delay_ms: Option<String>,
    /// `CB_DAEMON_TTY_BAUD`
    pub tty_baud: Option<String>,
}

impl EnvOverrides {
    /// Read overlay values from the process environment.
    #[must_use]
    pub fn from_os() -> Self {
        Self {
            config: env::var_os("CB_DAEMON_CONFIG").map(PathBuf::from),
            backend: env::var("CB_DAEMON_BACKEND").ok(),
            device: env::var_os("CB_DAEMON_DEVICE").map(PathBuf::from),
            bind: env::var("CB_DAEMON_BIND").ok(),
            log_level: env::var("CB_DAEMON_LOG_LEVEL").ok(),
            unit_id_hint: env::var("CB_DAEMON_UNIT_ID_HINT").ok(),
            aoa_chunk_size: env::var("CB_DAEMON_AOA_CHUNK_SIZE").ok(),
            aoa_chunk_delay_ms: env::var("CB_DAEMON_AOA_CHUNK_DELAY_MS").ok(),
            tty_baud: env::var("CB_DAEMON_TTY_BAUD").ok(),
        }
    }
}

/// Parse CLI + load/merge config from file, env, and CLI (CLI wins).
///
/// # Errors
///
/// Returns an error on missing explicit config path, TOML/parse failures, or
/// validation failures.
pub fn load_config() -> Result<Config> {
    let cli = CliArgs::parse();
    load_config_from(&cli, &EnvOverrides::from_os())
}

/// Load/merge with explicit CLI and env overlays (tests + binary).
///
/// # Errors
///
/// Same as [`load_config`].
pub fn load_config_from(cli: &CliArgs, env: &EnvOverrides) -> Result<Config> {
    let path = resolve_config_path(cli.config.as_deref(), env.config.as_deref())?;
    let file = match path {
        Some(ref p) => Some(load_file_config(p)?),
        None => None,
    };
    let mut cfg = Config::default();
    if let Some(ref file) = file {
        apply_file(&mut cfg, file)?;
    }
    apply_env(&mut cfg, env)?;
    apply_cli(&mut cfg, cli)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Resolve config file path per search rules.
///
/// - Explicit (`--config` or `CB_DAEMON_CONFIG`): must exist.
/// - Else first existing of `/etc/cb-daemon/config.toml` then `./config.toml`.
/// - Neither → `Ok(None)` (defaults only).
///
/// # Errors
///
/// Explicit path missing → error.
pub fn resolve_config_path(
    cli_config: Option<&Path>,
    env_config: Option<&Path>,
) -> Result<Option<PathBuf>> {
    resolve_config_path_with(cli_config, env_config, Path::is_file)
}

/// Resolve config path using injectable existence checks (unit tests).
fn resolve_config_path_with<F>(
    cli_config: Option<&Path>,
    env_config: Option<&Path>,
    exists: F,
) -> Result<Option<PathBuf>>
where
    F: Fn(&Path) -> bool,
{
    if let Some(path) = cli_config.or(env_config) {
        if exists(path) {
            return Ok(Some(path.to_path_buf()));
        }
        bail!(
            "config file not found: {} (set via --config or CB_DAEMON_CONFIG)",
            path.display()
        );
    }
    for candidate in [ETC_CONFIG, CWD_CONFIG] {
        let path = Path::new(candidate);
        if exists(path) {
            return Ok(Some(path.to_path_buf()));
        }
    }
    Ok(None)
}

fn load_file_config(path: &Path) -> Result<FileConfig> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse config TOML {}", path.display()))
}

fn apply_file(cfg: &mut Config, file: &FileConfig) -> Result<()> {
    if let Some(ref backend) = file.backend {
        cfg.backend =
            parse_backend(backend).with_context(|| format!("config file backend `{backend}`"))?;
    }
    if let Some(ref device) = file.device {
        cfg.device = Some(device.clone());
    }
    if let Some(ref bind) = file.bind {
        cfg.bind = bind
            .parse()
            .with_context(|| format!("config file bind `{bind}`"))?;
    }
    if let Some(ref hint) = file.unit_id_hint {
        cfg.unit_id_hint = Some(parse_unit_id_hint(hint).context("config file unit_id_hint")?);
    }
    if let Some(ref level) = file.log_level {
        cfg.log_level.clone_from(level);
    }
    if let Some(size) = file.aoa_chunk_size {
        cfg.aoa_chunk_size = size;
    }
    if let Some(delay) = file.aoa_chunk_delay_ms {
        cfg.aoa_chunk_delay_ms = delay;
    }
    if let Some(baud) = file.tty_baud {
        cfg.tty_baud = baud;
    }
    Ok(())
}

fn apply_env(cfg: &mut Config, env: &EnvOverrides) -> Result<()> {
    if let Some(ref backend) = env.backend {
        cfg.backend =
            parse_backend(backend).with_context(|| format!("CB_DAEMON_BACKEND `{backend}`"))?;
    }
    if let Some(ref device) = env.device {
        cfg.device = Some(device.clone());
    }
    if let Some(ref bind) = env.bind {
        cfg.bind = bind
            .parse()
            .with_context(|| format!("CB_DAEMON_BIND `{bind}`"))?;
    }
    if let Some(ref hint) = env.unit_id_hint {
        cfg.unit_id_hint = Some(parse_unit_id_hint(hint).context("CB_DAEMON_UNIT_ID_HINT")?);
    }
    if let Some(ref level) = env.log_level {
        cfg.log_level.clone_from(level);
    }
    if let Some(ref raw) = env.aoa_chunk_size {
        cfg.aoa_chunk_size = raw
            .parse()
            .with_context(|| format!("CB_DAEMON_AOA_CHUNK_SIZE `{raw}`"))?;
    }
    if let Some(ref raw) = env.aoa_chunk_delay_ms {
        cfg.aoa_chunk_delay_ms = raw
            .parse()
            .with_context(|| format!("CB_DAEMON_AOA_CHUNK_DELAY_MS `{raw}`"))?;
    }
    if let Some(ref raw) = env.tty_baud {
        cfg.tty_baud = raw
            .parse()
            .with_context(|| format!("CB_DAEMON_TTY_BAUD `{raw}`"))?;
    }
    Ok(())
}

fn apply_cli(cfg: &mut Config, cli: &CliArgs) -> Result<()> {
    if let Some(backend) = cli.backend {
        cfg.backend = backend;
    }
    if let Some(ref device) = cli.device {
        cfg.device = Some(device.clone());
    }
    if let Some(bind) = cli.bind {
        cfg.bind = bind;
    }
    if let Some(ref hint) = cli.unit_id_hint {
        cfg.unit_id_hint = Some(parse_unit_id_hint(hint).context("--unit-id-hint")?);
    }
    if let Some(ref level) = cli.log_level {
        cfg.log_level.clone_from(level);
    }
    if let Some(size) = cli.aoa_chunk_size {
        cfg.aoa_chunk_size = size;
    }
    if let Some(delay) = cli.aoa_chunk_delay_ms {
        cfg.aoa_chunk_delay_ms = delay;
    }
    if let Some(baud) = cli.tty_baud {
        cfg.tty_baud = baud;
    }
    Ok(())
}

fn parse_unit_id_hint(raw: &str) -> Result<UnitId> {
    UnitId::from_hex(raw.trim()).map_err(|err| {
        anyhow::anyhow!(
            "unit_id_hint `{raw}` must be exactly 5 hex digits (aa-registers UnitId): {err:?}"
        )
    })
}

fn validate(cfg: &Config) -> Result<()> {
    if let Some(ref device) = cfg.device
        && !device.is_absolute()
    {
        bail!(
            "device path must be absolute, got `{}` (backend {})",
            device.display(),
            cfg.backend.as_str()
        );
    }

    match cfg.backend {
        Backend::Aoa | Backend::Tty => {
            let effective = effective_device_path(cfg);
            if !effective.is_absolute() {
                bail!(
                    "effective device path for backend {} must be absolute, got `{}`",
                    cfg.backend.as_str(),
                    effective.display()
                );
            }
        }
        Backend::Mock => {}
    }

    if cfg.aoa_chunk_size < 1 {
        bail!("aoa_chunk_size must be >= 1, got {}", cfg.aoa_chunk_size);
    }
    if cfg.tty_baud == 0 {
        bail!("tty_baud must be > 0");
    }
    Ok(())
}

fn effective_device_path(cfg: &Config) -> PathBuf {
    if let Some(ref device) = cfg.device {
        return device.clone();
    }
    match cfg.backend {
        Backend::Aoa => PathBuf::from(AOA_DEFAULT_PATH),
        Backend::Tty => PathBuf::from(TTY_DEFAULT_PATH),
        Backend::Mock => PathBuf::new(),
    }
}

/// Build a tracing [`EnvFilter`] from merged `log_level`.
///
/// When `RUST_LOG` is set, uses that directive string instead of `log_level`.
///
/// # Errors
///
/// Returns an error if the filter directive is invalid.
pub fn build_env_filter(log_level: &str) -> Result<EnvFilter> {
    env::var("RUST_LOG").map_or_else(
        |_| build_env_filter_inner(log_level, None),
        |directives| build_env_filter_inner(log_level, Some(directives.as_str())),
    )
}

fn build_env_filter_inner(log_level: &str, rust_log: Option<&str>) -> Result<EnvFilter> {
    rust_log.map_or_else(
        || {
            EnvFilter::try_new(log_level)
                .with_context(|| format!("invalid log_level `{log_level}`"))
        },
        |directives| EnvFilter::try_new(directives).context("parse RUST_LOG"),
    )
}

/// Initialise the global tracing subscriber from merged config.
///
/// # Errors
///
/// Returns an error if the filter cannot be built.
pub fn init_tracing(log_level: &str) -> Result<()> {
    let filter = build_env_filter(log_level)?;
    // LineWriter so Magisk `control.sh` redirects to cb-daemon.log flush per line
    // (stderr is fully buffered when not a TTY).
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(|| std::io::LineWriter::new(std::io::stderr()))
        .init();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_toml(body: &str) -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "cb-daemon-config-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn defaults_are_mock_and_local_bind() {
        let cfg = Config::default();
        assert_eq!(cfg.backend, Backend::Mock);
        assert_eq!(cfg.bind, "127.0.0.1:2026".parse().unwrap());
        assert!(cfg.device.is_none());
        assert!(cfg.unit_id_hint.is_none());
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.aoa_chunk_size, DEFAULT_AOA_CHUNK_SIZE);
        assert_eq!(cfg.aoa_chunk_delay_ms, DEFAULT_AOA_CHUNK_DELAY_MS);
        assert_eq!(cfg.tty_baud, DEFAULT_TTY_BAUD);
        assert_eq!(DEFAULT_AOA_CHUNK_SIZE, AOA_MAX_CHUNK);
        assert_eq!(DEFAULT_AOA_CHUNK_DELAY_MS, AOA_INTER_CHUNK_DELAY_MS);
        assert_eq!(DEFAULT_TTY_BAUD, TTY_BAUD);
    }

    #[test]
    fn mock_is_not_aoa_backend() {
        assert_ne!(Backend::Mock, Backend::Aoa);
        assert_ne!(Backend::Mock, Backend::Tty);
    }

    #[test]
    fn merge_precedence_file_env_cli() {
        let path = write_temp_toml(
            r#"
backend = "aoa"
device = "/dev/from-file"
bind = "127.0.0.1:1111"
log_level = "warn"
aoa_chunk_size = 10
aoa_chunk_delay_ms = 2
tty_baud = 9600
unit_id_hint = "abcde"
"#,
        );
        let file = load_file_config(&path).unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &file).unwrap();

        let env = EnvOverrides {
            backend: Some("tty".into()),
            device: Some(PathBuf::from("/dev/from-env")),
            bind: Some("127.0.0.1:2222".into()),
            log_level: Some("debug".into()),
            unit_id_hint: Some("00001".into()),
            aoa_chunk_size: Some("20".into()),
            aoa_chunk_delay_ms: Some("3".into()),
            tty_baud: Some("19200".into()),
            config: None,
        };
        apply_env(&mut cfg, &env).unwrap();

        let cli = CliArgs {
            backend: Some(Backend::Mock),
            device: Some(PathBuf::from("/dev/from-cli")),
            bind: Some("127.0.0.1:3333".parse().unwrap()),
            log_level: Some("trace".into()),
            unit_id_hint: Some("fffff".into()),
            aoa_chunk_size: Some(30),
            aoa_chunk_delay_ms: Some(4),
            tty_baud: Some(38400),
            config: None,
        };
        apply_cli(&mut cfg, &cli).unwrap();
        validate(&cfg).unwrap();

        assert_eq!(cfg.backend, Backend::Mock);
        assert_eq!(cfg.device.as_deref(), Some(Path::new("/dev/from-cli")));
        assert_eq!(cfg.bind, "127.0.0.1:3333".parse().unwrap());
        assert_eq!(cfg.log_level, "trace");
        assert_eq!(cfg.unit_id_hint, Some(UnitId::from_hex("fffff").unwrap()));
        assert_eq!(cfg.aoa_chunk_size, 30);
        assert_eq!(cfg.aoa_chunk_delay_ms, 4);
        assert_eq!(cfg.tty_baud, 38400);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn optional_search_miss_uses_defaults() {
        let path = resolve_config_path_with(None, None, |_| false).unwrap();
        assert!(path.is_none());
        let cfg = load_config_from(&CliArgs::default(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn explicit_missing_config_errors() {
        let missing = PathBuf::from("/no/such/cb-daemon-config-xyz.toml");
        let err = resolve_config_path(Some(&missing), None).unwrap_err();
        assert!(
            err.to_string().contains("config file not found"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn search_prefers_etc_then_cwd() {
        let etc = Path::new(ETC_CONFIG);
        let cwd = Path::new(CWD_CONFIG);
        let chosen = resolve_config_path_with(None, None, |p| p == etc).unwrap();
        assert_eq!(chosen.as_deref(), Some(etc));

        let chosen = resolve_config_path_with(None, None, |p| p == cwd).unwrap();
        assert_eq!(chosen.as_deref(), Some(cwd));

        let chosen = resolve_config_path_with(None, None, |p| p == etc || p == cwd).unwrap();
        assert_eq!(chosen.as_deref(), Some(etc));
    }

    #[test]
    fn unknown_backend_from_file_fails() {
        let mut cfg = Config::default();
        let file = FileConfig {
            backend: Some("usb".into()),
            ..FileConfig::default()
        };
        let err = apply_file(&mut cfg, &file).unwrap_err();
        let full = format!("{err:#}");
        assert!(
            full.contains("unknown backend") || full.contains("usb"),
            "{full}"
        );
    }

    #[test]
    fn relative_device_for_aoa_fails() {
        let cfg = Config {
            backend: Backend::Aoa,
            device: Some(PathBuf::from("relative/dev")),
            ..Config::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn bad_unit_id_hint_fails() {
        let err = parse_unit_id_hint("abcd").unwrap_err();
        assert!(err.to_string().contains("5 hex"), "{err}");
    }

    #[test]
    fn zero_chunk_size_fails() {
        let cfg = Config {
            aoa_chunk_size: 0,
            ..Config::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("aoa_chunk_size"), "{err}");
    }

    #[test]
    fn zero_tty_baud_fails() {
        let cfg = Config {
            tty_baud: 0,
            ..Config::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("tty_baud"), "{err}");
    }

    #[test]
    fn aoa_default_device_path_is_absolute() {
        let cfg = Config {
            backend: Backend::Aoa,
            device: None,
            ..Config::default()
        };
        validate(&cfg).unwrap();
    }

    #[test]
    fn load_from_explicit_toml_file() {
        let path = write_temp_toml(
            r#"
backend = "mock"
bind = "127.0.0.1:4040"
log_level = "debug"
"#,
        );
        let cli = CliArgs {
            config: Some(path.clone()),
            ..CliArgs::default()
        };
        let cfg = load_config_from(&cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:4040".parse().unwrap());
        assert_eq!(cfg.log_level, "debug");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rust_log_overrides_log_level_filter() {
        let from_rust = build_env_filter_inner("info", Some("cb_daemon=trace")).unwrap();
        let rendered = from_rust.to_string();
        assert!(
            rendered.contains("trace") || rendered.contains("cb_daemon"),
            "expected RUST_LOG-driven filter, got {rendered}"
        );

        let from_level = build_env_filter_inner("warn", None).unwrap();
        let rendered = from_level.to_string();
        assert!(
            rendered.contains("warn"),
            "expected log_level warn when RUST_LOG unset, got {rendered}"
        );
    }

    #[test]
    fn cli_absent_leaves_file_value() {
        let mut cfg = Config::default();
        apply_file(
            &mut cfg,
            &FileConfig {
                log_level: Some("debug".into()),
                backend: Some("aoa".into()),
                device: Some(PathBuf::from("/dev/usb_accessory")),
                ..FileConfig::default()
            },
        )
        .unwrap();
        apply_cli(&mut cfg, &CliArgs::default()).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.backend, Backend::Aoa);
    }
}
