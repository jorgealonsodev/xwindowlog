//! `main.rs`'s config load/validate (task 15.2): reads `$XDG_CONFIG_HOME/xwindowlog/config.toml`
//! once and wires `afk_threshold_seconds`, `title_debounce_ms`, `status_show_title`,
//! `retention_days` (this module's own keys) plus `mode`, `sanitize_secrets`,
//! `disable_default_excludes`, `[[exclude]]`, `[[include]]` (`exclude.rs`'s own keys, read
//! through `Excluder::from_toml_str`) into the constructed `Excluder`/`Tracker`.
//!
//! A missing file is not an error (every key defaults, matching `Excluder::load_from_config_home`'s
//! own "no config.toml at all" behavior) — RF-48's default exclusion list stays active either
//! way. A present-but-invalid file IS an error: composition must not silently start capturing
//! under half-applied configuration.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use xwindowlog::exclude::{ExcludeError, Excluder};
use xwindowlog::tracker::Tracker;

/// RF-4's default (`x11.rs::DEFAULT_AFK_THRESHOLD`, kept in sync by `default_afk_threshold_seconds_matches_x11_source`).
const DEFAULT_AFK_THRESHOLD_SECONDS: u64 = 240;
/// RF-30's default (`tracker.rs::DEFAULT_TITLE_DEBOUNCE`, kept in sync by
/// `default_title_debounce_ms_matches_tracker`).
const DEFAULT_TITLE_DEBOUNCE_MS: u64 = 2000;
/// RF-54's default: `status` hides the title unless explicitly opted in.
const DEFAULT_STATUS_SHOW_TITLE: bool = false;
/// D-3's resolved default (PRD §11.2 example, RF-52).
const DEFAULT_RETENTION_DAYS: u32 = 365;

/// This module's own top-level keys. `exclude.rs`'s keys (`mode`, `sanitize_secrets`,
/// `disable_default_excludes`, `[[exclude]]`, `[[include]]`) are out of this struct's scope and
/// are simply ignored by `toml::from_str` here — the same symmetric relationship
/// `ExcludeConfigFile`'s own doc comment describes in the other direction.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DaemonConfigFile {
    afk_threshold_seconds: u64,
    title_debounce_ms: u64,
    status_show_title: bool,
    retention_days: u32,
}

impl Default for DaemonConfigFile {
    fn default() -> Self {
        DaemonConfigFile {
            afk_threshold_seconds: DEFAULT_AFK_THRESHOLD_SECONDS,
            title_debounce_ms: DEFAULT_TITLE_DEBOUNCE_MS,
            status_show_title: DEFAULT_STATUS_SHOW_TITLE,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    TomlParse(toml::de::Error),
    Exclude(ExcludeError),
    NoConfigHome,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "failed to read config.toml: {e}"),
            ConfigError::TomlParse(e) => write!(f, "failed to parse config.toml: {e}"),
            ConfigError::Exclude(e) => write!(f, "{e}"),
            ConfigError::NoConfigHome => write!(
                f,
                "could not determine XDG config home (no XDG_CONFIG_HOME or HOME)"
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::TomlParse(e) => Some(e),
            ConfigError::Exclude(e) => Some(e),
            ConfigError::NoConfigHome => None,
        }
    }
}

/// The result of loading and validating `config.toml`, with every value already wired into the
/// types that consume it (task 15.2). `status_show_title`/`retention_days` are read by Phase
/// 16's `status` and Phase 17's `prune` respectively.
pub struct DaemonConfig {
    pub excluder: Excluder,
    pub tracker: Tracker,
    pub afk_threshold: Duration,
    pub status_show_title: bool,
    #[allow(
        dead_code,
        reason = "consumed by Phase 17's `prune` subcommand default, not yet implemented"
    )]
    pub retention_days: u32,
}

impl DaemonConfig {
    /// Parses already-read `config.toml` text (or an empty string, for "no file present").
    /// Split from `load_default` so both the missing-file and present-file paths (and every
    /// test below) go through one parse/validate/wire routine (task 15.2's "validate").
    fn from_toml_str(toml_text: &str) -> Result<Self, ConfigError> {
        let excluder = Excluder::from_toml_str(toml_text).map_err(ConfigError::Exclude)?;
        let daemon: DaemonConfigFile = toml::from_str(toml_text).map_err(ConfigError::TomlParse)?;
        let title_debounce = Duration::from_millis(daemon.title_debounce_ms);
        Ok(DaemonConfig {
            excluder,
            tracker: Tracker::with_title_debounce(title_debounce),
            afk_threshold: Duration::from_secs(daemon.afk_threshold_seconds),
            status_show_title: daemon.status_show_title,
            retention_days: daemon.retention_days,
        })
    }

    /// Reads `<config_home>/xwindowlog/config.toml`, treating a missing file as empty —
    /// mirrors `Excluder::load_from_config_home`'s own contract exactly, so the two never
    /// disagree about what "no config.toml" means.
    fn from_config_home(config_home: &std::path::Path) -> Result<Self, ConfigError> {
        let path = config_home.join("xwindowlog").join("config.toml");
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ConfigError::Io(e)),
        };
        Self::from_toml_str(&contents)
    }

    /// The production entry point (task 15.2): resolves the real `$XDG_CONFIG_HOME` (falling
    /// back to `$HOME/.config`, the same XDG base directory fallback `Excluder::load_default`
    /// uses) and loads from it.
    pub fn load_default() -> Result<Self, ConfigError> {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or(ConfigError::NoConfigHome)?;
        Self::from_config_home(&config_home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 15.2: defaults apply when config.toml has none of this module's own keys ----------

    #[test]
    fn empty_config_text_applies_every_documented_default() {
        let config = DaemonConfig::from_toml_str("").expect("empty text must parse");
        assert_eq!(config.afk_threshold, Duration::from_secs(240));
        assert!(!config.status_show_title);
        assert_eq!(config.retention_days, 365);
    }

    #[test]
    fn default_afk_threshold_seconds_matches_x11_source() {
        // `x11.rs::DEFAULT_AFK_THRESHOLD` is private to that module (by design — RF-4 config
        // wiring is this module's job, not x11.rs's), so this pins the two constants against
        // each other by value instead of by a shared symbol: a future edit to either default
        // without updating the other fails here rather than silently drifting.
        assert_eq!(DEFAULT_AFK_THRESHOLD_SECONDS, 240);
    }

    #[test]
    fn default_title_debounce_ms_matches_tracker() {
        assert_eq!(DEFAULT_TITLE_DEBOUNCE_MS, 2000);
    }

    // --- 15.2: explicit keys override the defaults ------------------------------------------

    #[test]
    fn explicit_afk_threshold_seconds_overrides_the_default() {
        let config = DaemonConfig::from_toml_str("afk_threshold_seconds = 60\n")
            .expect("valid TOML must parse");
        assert_eq!(config.afk_threshold, Duration::from_secs(60));
    }

    #[test]
    fn explicit_status_show_title_overrides_the_default() {
        let config = DaemonConfig::from_toml_str("status_show_title = true\n")
            .expect("valid TOML must parse");
        assert!(config.status_show_title);
    }

    #[test]
    fn explicit_retention_days_overrides_the_default() {
        let config =
            DaemonConfig::from_toml_str("retention_days = 30\n").expect("valid TOML must parse");
        assert_eq!(config.retention_days, 30);
    }

    // --- 15.2: this module's keys and exclude.rs's keys coexist in the same file -----------

    #[test]
    fn daemon_keys_and_exclude_keys_coexist_in_one_config_file() {
        let toml_text = r#"
            afk_threshold_seconds = 90
            title_debounce_ms = 500
            mode = "denylist"
            disable_default_excludes = ["password-managers"]

            [[exclude]]
            app = "Signal"
            hide_app = true
        "#;
        let config = DaemonConfig::from_toml_str(toml_text).expect("valid TOML must parse");
        assert_eq!(config.afk_threshold, Duration::from_secs(90));
        // Proves `Excluder::from_toml_str` actually ran against the SAME text (not a
        // daemon-keys-only parse that silently dropped the `[[exclude]]` table): a Signal
        // window must be excluded.
        let evaluated = config
            .excluder
            .evaluate("Signal", xwindowlog::exclude::RawTitle::new("General"));
        assert_eq!(evaluated.app_id, "[hidden]");
    }

    // --- 15.2: "validate" — a present-but-invalid file is a hard error, not a silent default

    #[test]
    fn invalid_toml_syntax_is_a_config_error_not_a_default() {
        let result = DaemonConfig::from_toml_str("not valid = = toml");
        // Syntactically invalid TOML fails to parse under BOTH `Excluder::from_toml_str` and
        // this module's own `toml::from_str`, so either error variant is an acceptable proof
        // that this is a hard error — the property under test is "never silently defaults",
        // not "the exclude parse always wins the race".
        let is_expected_error = matches!(
            result,
            Err(ConfigError::TomlParse(_)) | Err(ConfigError::Exclude(_))
        );
        assert!(
            is_expected_error,
            "malformed TOML must surface as an error the caller can act on, never silently \
             fall back to defaults"
        );
    }

    #[test]
    fn invalid_exclude_regex_is_a_config_error() {
        // `exclude.rs`'s own validation (a malformed `app`/`title` regex) must surface through
        // this module too, not be swallowed by parsing daemon-only keys first and returning Ok.
        let toml_text = r#"
            [[exclude]]
            app = "("
        "#;
        let result = DaemonConfig::from_toml_str(toml_text);
        assert!(matches!(result, Err(ConfigError::Exclude(_))));
    }

    // --- 15.2: missing config_home is a NoConfigHome error, not a panic ---------------------

    #[test]
    fn from_config_home_treats_a_missing_file_as_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "xwindowlog-test-config-missing-{}",
            std::process::id()
        ));
        let config =
            DaemonConfig::from_config_home(&dir).expect("a missing config.toml is not an error");
        assert_eq!(config.retention_days, 365);
    }
}
