//! `RawTitle` → `SafeTitle` (D-7); RF-7, RF-8, RF-47, RF-48 default list, RF-50 allowlist, RF-51
//! redaction. The only constructor of `SafeTitle`.
//!
//! PRD §14.3's boundary: `RawTitle` exists only in memory between the X11 capture layer
//! (`x11.rs`, Phase 9+) and this module. Below this line only `SafeTitle` exists, and that is
//! enforced by the type system, not by discipline: `SafeTitle`'s single field is private and its
//! only constructor (`from_sanitized`) lives in this file, so no other module can construct one
//! without going through sanitization. `RawTitle` deliberately has no `Display` impl anywhere in
//! this crate, and its `Debug` impl is redacting — a `tracing::debug!("{:?}", raw)` anywhere,
//! today or added in six months, cannot print the title content (task 7.16 REFACTOR: verified by
//! `grep -n "impl.*Display for RawTitle" src/exclude.rs` returning nothing).
//!
//! Deviation from design §2 D-7's literal `WindowInfo { app_id: SafeAppId, title: SafeTitle, .. }`
//! snippet: tasks.md 7.1 scopes this phase to `RawTitle`/`SafeTitle` only. `SafeAppId` is not
//! defined here; `app_id` is handled as a plain `String` throughout this module (RF-47's
//! `hide_app` substitutes the literal string `"[hidden]"`, same mechanism as the title). The
//! `WindowInfo` swap to safe types is tasks.md 8.3/8.4's job, per Phase 6's discovery #5.

// Module-level allow, not narrowed to individual items: every public item in this file is
// exercised only by this module's own `#[cfg(test)]` suite so far, because Phase 7 (this phase)
// deliberately does not wire `Excluder` into `main.rs`/`tracker.rs` — that wiring is Phase 8's
// task (design §3's data-flow diagram: `x11.rs -> exclude.rs -> tracker.rs`). This mirrors
// `clock.rs`'s own state at the end of Phase 2, before Phase 3/4/6 gave it real callers; task
// 6.12 narrowed clock.rs's allow once real (non-test) callers existed; the same narrowing is
// expected here once Phase 8 lands, not before.
#![allow(
    dead_code,
    reason = "no non-test consumer of exclude.rs exists yet — Phase 8 wires Excluder into the \
              pipeline (design §3), matching clock.rs's own Phase 2 state before Phase 6 gave it \
              real callers (task 6.12's precedent for narrowing this allow once that happens)"
)]

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

/// A title exactly as the X11 capture layer read it. Exists only in memory, only between capture
/// and this module's `Excluder::evaluate`.
///
/// Deliberately has NO `Display` impl anywhere in this crate (checked by task 7.16's grep), and
/// its `Debug` never prints the content — only a character count.
pub struct RawTitle(String);

impl RawTitle {
    /// The one place a `RawTitle` is built from arbitrary text. Called by `x11.rs` (later
    /// phases) with whatever X11 handed it, and by this module's own tests.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Read-only access for THIS module's own matching/redaction logic. Not `pub`: nothing
    /// outside `exclude.rs` may read the raw text directly, which is exactly the guarantee
    /// `SafeTitle`'s privacy below depends on symmetrically.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RawTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawTitle(<{} chars, redacted>)", self.0.chars().count())
    }
}

/// A title that has passed through `Excluder::evaluate`. The ONLY title type the tracker, the
/// store, and every read path may name (design §2 D-7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeTitle(String);

impl SafeTitle {
    /// The single constructor, and it is **private to this module** — not
    /// `pub(crate)`.
    ///
    /// That distinction is the whole guarantee. With `pub(crate)`, any module
    /// in the crate could call `SafeTitle::from_sanitized(raw_string)` and
    /// mint a "safe" title that had never been near `evaluate`, which is
    /// exactly the bypass RF-7 and §14.3 exist to prevent. Being private,
    /// the only way to obtain a `SafeTitle` anywhere else in this crate is to
    /// put a `RawTitle` through `Excluder::evaluate`. The compile-fail fixture
    /// in `tests/trybuild/fail/` proves it stays that way.
    ///
    /// If a later phase legitimately needs to rebuild a `SafeTitle` from
    /// content already stored on disk, add a separate constructor with a name
    /// that says so and a comment justifying the trust, rather than widening
    /// this one — a generic name hiding a trust assumption is how this kind of
    /// boundary quietly rots.
    fn from_sanitized(s: String) -> Self {
        Self(s)
    }

    /// Read access for downstream consumers (`tracker.rs`, `store.rs`, later phases).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `mode = "denylist"` (default) or `mode = "allowlist"` (RF-50). Same evaluation and storage
/// behavior either way — allowlist just inverts which windows count as excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Denylist,
    Allowlist,
}

/// One `[[exclude]]` table from `config.toml` (RF-7). At least one of `app`/`title` is expected;
/// a rule with neither matches nothing (defensive default, not a config error — RF-7 does not
/// require rejecting a degenerate rule).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawExcludeRule {
    pub app: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub hide_app: bool,
}

/// One `[[include]]` table from `config.toml`, used only in `mode = "allowlist"` (RF-50).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawIncludeRule {
    pub app: Option<String>,
    pub title: Option<String>,
}

/// The parsed shape of `$XDG_CONFIG_HOME/xwindowlog/config.toml`'s exclusion-relevant fields
/// (RF-7). Other top-level keys the daemon also reads (`afk_threshold_seconds`, and so on) are
/// out of this module's scope and are simply ignored by `toml::from_str` here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExcludeConfigFile {
    pub mode: Mode,
    /// `None` means "not set in config" so `Excluder` can apply RF-51's documented `true`
    /// default; `toml` cannot distinguish "absent" from "false" once this is a plain `bool`.
    pub sanitize_secrets: Option<bool>,
    pub disable_default_excludes: Vec<String>,
    pub exclude: Vec<RawExcludeRule>,
    pub include: Vec<RawIncludeRule>,
}

impl ExcludeConfigFile {
    fn parse(toml_text: &str) -> Result<Self, ExcludeError> {
        toml::from_str(toml_text).map_err(ExcludeError::TomlParse)
    }
}

/// Everything that can go wrong loading or compiling an exclusion configuration. Deliberately not
/// `thiserror` (task 7.3: not a Phase 1 dependency per design §4's Cargo.toml list) — this crate's
/// error types are hand-written per the rust-systems skill's "carries what the caller needs to
/// decide" rule, and there is nothing here a derive macro would meaningfully shorten.
#[derive(Debug)]
pub enum ExcludeError {
    Io(io::Error),
    TomlParse(toml::de::Error),
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
    NoConfigHome,
}

impl fmt::Display for ExcludeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExcludeError::Io(e) => write!(f, "failed to read config.toml: {e}"),
            ExcludeError::TomlParse(e) => write!(f, "failed to parse config.toml: {e}"),
            ExcludeError::InvalidRegex { pattern, source } => {
                write!(f, "invalid exclusion regex {pattern:?}: {source}")
            }
            ExcludeError::NoConfigHome => {
                write!(
                    f,
                    "could not determine XDG config home (no XDG_CONFIG_HOME or HOME)"
                )
            }
        }
    }
}

impl std::error::Error for ExcludeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExcludeError::Io(e) => Some(e),
            ExcludeError::TomlParse(e) => Some(e),
            ExcludeError::InvalidRegex { source, .. } => Some(source),
            ExcludeError::NoConfigHome => None,
        }
    }
}

/// A denylist rule after its patterns have been compiled and validated (rust-systems skill:
/// "validate any regex from user configuration with `Regex::new` before storing or applying it,
/// and cache compiled expressions"). `id` is `Some` only for a built-in RF-48 default rule —
/// that is what `disable_default_excludes` matches against; user rules from `config.toml` are
/// never individually named or disabled.
struct CompiledRule {
    id: Option<&'static str>,
    app: Option<Regex>,
    title: Option<Regex>,
    hide_app: bool,
}

/// A compiled `[[include]]` rule (RF-50, allowlist mode only).
struct CompiledInclude {
    app: Option<Regex>,
    title: Option<Regex>,
}

/// The result of evaluating one captured window against the active rule set. `app_id` is a plain
/// `String` (see module doc's deviation note) that is either the real value or the literal
/// `"[hidden]"` (RF-47); `title` is always a `SafeTitle`, constructed only here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluatedWindow {
    pub app_id: String,
    pub title: SafeTitle,
}

/// Compiles a config-supplied regex pattern, or returns `None` for an absent field. Every
/// pattern that reaches this function was typed by the user (or is one of the five RF-48
/// defaults) and MUST be validated before it is ever matched against real data.
fn compile_opt(pattern: Option<&str>) -> Result<Option<Regex>, ExcludeError> {
    match pattern {
        None => Ok(None),
        Some(p) => Regex::new(p)
            .map(Some)
            .map_err(|source| ExcludeError::InvalidRegex {
                pattern: p.to_string(),
                source,
            }),
    }
}

/// `app`/`title` match with OR semantics across whichever fields a rule sets: RF-48's own
/// `2fa-otp` default rule table row is explicit ("title or `app_id`"), and no default or
/// documented rule in this crate ever requires both fields to match simultaneously.
fn rule_matches(app: Option<&Regex>, title: Option<&Regex>, app_id: &str, raw_title: &str) -> bool {
    let app_hit = app.is_some_and(|r| r.is_match(app_id));
    let title_hit = title.is_some_and(|r| r.is_match(raw_title));
    app_hit || title_hit
}

/// The RF-48 built-in default exclusion list, embedded in the binary and active even when no
/// `config.toml` exists at all. Patterns are copied verbatim from `specs/privacy-filtering/
/// spec.md`'s table (itself copied from PRD.md §11.2), including the Spanish alternatives in
/// `private-browsing` and `2fa-otp` — those match real window titles on a Spanish-locale desktop
/// and are not a translation left to be finished.
const DEFAULT_RULES: &[(&str, Option<&str>, Option<&str>, bool)] = &[
    (
        "password-managers",
        Some(
            r"(?i)^(keepassxc|keepassx|keepass|bitwarden|1password|lastpass|dashlane|enpass|passwordsafe|gopass)$",
        ),
        None,
        false,
    ),
    (
        "banking-generic",
        None,
        Some(
            r"(?i)\b(banco|banca|bank|paypal|stripe dashboard|coinbase|binance|kraken|revolut|wise|n26|openbank)\b",
        ),
        false,
    ),
    (
        "private-browsing",
        None,
        Some(
            r"(?i)(private browsing|navegaci[oó]n privada|inc[oó]gnito|incognito|inprivate|modo privado)",
        ),
        false,
    ),
    (
        "gpg-ssh-prompts",
        Some(r"(?i)^(pinentry.*|ssh-askpass|x11-ssh-askpass|lxqt-openssh-askpass)$"),
        None,
        false,
    ),
    (
        "2fa-otp",
        Some(r"(?i)^(authy|gnome-authenticator|otpclient)$"),
        Some(
            r"(?i)(two-?factor|2fa|verification code|c[oó]digo de verificaci[oó]n|one-?time passcode|\botp\b)",
        ),
        false,
    ),
];

/// RF-51: known provider token prefixes, long hex, base64-shaped strings, and email addresses.
/// Alternation order matters — the four provider-prefixed patterns are listed before the more
/// generic hex/base64 patterns so a `ghp_...` token is redacted as ONE fragment (the whole
/// token) rather than the generic base64-shaped pattern racing it (the `regex` crate resolves
/// alternation with leftmost-first, Perl-like semantics: whichever alternative is listed first
/// wins when several match at the same starting position).
static SECRET_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"sk-[A-Za-z0-9]{16,}",
        r"|ghp_[A-Za-z0-9]{36,}",
        r"|xox[a-z]-[A-Za-z0-9-]{10,}",
        r"|AKIA[A-Z0-9]{16}",
        r"|\b[0-9a-fA-F]{32,}\b",
        r"|[A-Za-z0-9+/]{24,}={0,2}",
        r"|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
    ))
    .expect(
        "SECRET_PATTERN is a fixed, hand-verified regex literal covered by this module's own \
         tests (provider_prefixed_token_fragment_is_redacted, long_hex_fragment_is_redacted, \
         base64_shaped_fragment_is_redacted, email_address_fragment_is_redacted) — a syntax \
         error here would fail every one of them at test time, not silently in production",
    )
});

/// RF-51: replaces each matching fragment with `[REDACTED]`, leaving the rest of the title
/// unchanged. Independent of, and applied in addition to, exclusion-rule matching.
fn redact_secrets(raw: &str) -> String {
    SECRET_PATTERN.replace_all(raw, "[REDACTED]").into_owned()
}

/// Holds the compiled, active rule set. Built once at startup, replaced wholesale on `SIGHUP`
/// (RF-9, task 7.15) — a hot-swap, not a mutation of individual rules.
pub struct Excluder {
    mode: Mode,
    sanitize_secrets: bool,
    rules: Vec<CompiledRule>,
    includes: Vec<CompiledInclude>,
}

impl Excluder {
    /// Compiles an `Excluder` from an already-parsed config. User rules are compiled first, then
    /// the active (non-disabled) RF-48 defaults are appended — a window can therefore match a
    /// user rule OR a default rule, whichever comes first in evaluation order.
    pub fn from_config(config: &ExcludeConfigFile) -> Result<Self, ExcludeError> {
        let mut rules = Vec::with_capacity(config.exclude.len() + DEFAULT_RULES.len());
        for rule in &config.exclude {
            rules.push(CompiledRule {
                id: None,
                app: compile_opt(rule.app.as_deref())?,
                title: compile_opt(rule.title.as_deref())?,
                hide_app: rule.hide_app,
            });
        }
        for &(id, app, title, hide_app) in DEFAULT_RULES {
            let disabled = config.disable_default_excludes.iter().any(|d| d == id);
            if disabled {
                continue;
            }
            rules.push(CompiledRule {
                id: Some(id),
                app: compile_opt(app)?,
                title: compile_opt(title)?,
                hide_app,
            });
        }

        let mut includes = Vec::with_capacity(config.include.len());
        for rule in &config.include {
            includes.push(CompiledInclude {
                app: compile_opt(rule.app.as_deref())?,
                title: compile_opt(rule.title.as_deref())?,
            });
        }

        Ok(Self {
            mode: config.mode,
            sanitize_secrets: config.sanitize_secrets.unwrap_or(true),
            rules,
            includes,
        })
    }

    /// Parses `config.toml`'s text and compiles it. Pure — no filesystem access — so it is the
    /// function every unit test in this module exercises directly.
    pub fn from_toml_str(toml_text: &str) -> Result<Self, ExcludeError> {
        let config = ExcludeConfigFile::parse(toml_text)?;
        Self::from_config(&config)
    }

    /// Reads `<config_home>/xwindowlog/config.toml`, or treats a missing file as an empty
    /// config (RF-48: defaults stay active with no `config.toml` present at all).
    pub fn load_from_config_home(config_home: &Path) -> Result<Self, ExcludeError> {
        let path = config_home.join("xwindowlog").join("config.toml");
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ExcludeError::Io(e)),
        };
        Self::from_toml_str(&contents)
    }

    /// Resolves the real `$XDG_CONFIG_HOME` (falling back to `$HOME/.config` per the XDG base
    /// directory spec) and loads from it. The production entry point; `main.rs` (Phase 15)
    /// calls this at startup and again on `SIGHUP` via `reload_from_toml_str`/`reload_default`.
    pub fn load_default() -> Result<Self, ExcludeError> {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or(ExcludeError::NoConfigHome)?;
        Self::load_from_config_home(&config_home)
    }

    /// RF-9 / task 7.14-7.15: replaces the active rule set wholesale. Already-recorded intervals
    /// live in `store.rs` and are never touched by this call; only subsequent `evaluate` calls
    /// see the new rules.
    pub fn reload_from_toml_str(&mut self, toml_text: &str) -> Result<(), ExcludeError> {
        *self = Self::from_toml_str(toml_text)?;
        Ok(())
    }

    /// Returns `Some(hide_app)` if the window is excluded (denylist: a rule matched; allowlist:
    /// no `[[include]]` rule matched), `None` if it is not.
    fn is_excluded(&self, app_id: &str, raw_title: &str) -> Option<bool> {
        match self.mode {
            Mode::Denylist => self
                .rules
                .iter()
                .find(|rule| {
                    rule_matches(rule.app.as_ref(), rule.title.as_ref(), app_id, raw_title)
                })
                .map(|rule| rule.hide_app),
            Mode::Allowlist => {
                let included = self.includes.iter().any(|inc| {
                    rule_matches(inc.app.as_ref(), inc.title.as_ref(), app_id, raw_title)
                });
                // RF-50: same evaluation/storage behavior as denylist, just inverted — an
                // allowlist miss is an exclusion with `hide_app = false` (no `[[include]]` rule
                // carries a `hide_app` field to invert).
                if included {
                    None
                } else {
                    Some(false)
                }
            }
        }
    }

    /// The evaluation entry point (§14.3's boundary). Consumes the `RawTitle` — once evaluated,
    /// the only title that exists is the returned `SafeTitle`.
    pub fn evaluate(&self, app_id: &str, title: RawTitle) -> EvaluatedWindow {
        let raw_text = title.as_str();
        match self.is_excluded(app_id, raw_text) {
            Some(hide_app) => EvaluatedWindow {
                app_id: if hide_app {
                    "[hidden]".to_string()
                } else {
                    app_id.to_string()
                },
                title: SafeTitle::from_sanitized("[hidden]".to_string()),
            },
            None => {
                let sanitized = if self.sanitize_secrets {
                    redact_secrets(raw_text)
                } else {
                    raw_text.to_string()
                };
                EvaluatedWindow {
                    app_id: app_id.to_string(),
                    title: SafeTitle::from_sanitized(sanitized),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RF-7 scenario "A configured rule matches on app_id".
    #[test]
    fn exclude_rule_matches_app_id() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "keepassxc"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate("keepassxc", RawTitle::new("KeePassXC - vault.kdbx"));

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
        assert_eq!(result.app_id, "keepassxc");
    }

    /// RF-7 scenario "A configured rule matches on title".
    #[test]
    fn exclude_rule_matches_title() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            title = "(?i)banco|bbva|santander|caixa"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate("firefox", RawTitle::new("BBVA - Online Banking"));

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-7 scenario "No raw title is ever logged before sanitization": `RawTitle`'s `Debug`
    /// never contains the title content, so no diagnostic/warning/error path that happens to
    /// `{:?}`-format a `RawTitle` (accidentally, today or in a future edit) can leak it.
    #[test]
    fn raw_title_debug_never_contains_the_title_content() {
        let raw = RawTitle::new("BBVA - my account 1234-5678-90 balance");

        let formatted = format!("{raw:?}");

        assert!(!formatted.contains("BBVA"));
        assert!(!formatted.contains("1234-5678-90"));
        assert_eq!(formatted, "RawTitle(<38 chars, redacted>)");
    }

    // ---- RF-8: hidden title with time preserved (duration itself is store.rs/tracker.rs's
    // concern, proven at pipeline level in Phase 8 — this module only owns app_id/title). ----

    /// RF-8 scenario "Two different excluded apps remain distinguishable".
    #[test]
    fn two_different_excluded_apps_remain_distinguishable() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "keepassxc"

            [[exclude]]
            app = "1password"
            "#,
        )
        .expect("valid config compiles");

        let keepass = excluder.evaluate("keepassxc", RawTitle::new("KeePassXC - vault.kdbx"));
        let onepassword = excluder.evaluate("1password", RawTitle::new("1Password - Vault"));

        assert_eq!(keepass.app_id, "keepassxc");
        assert_eq!(onepassword.app_id, "1password");
        assert_eq!(
            keepass.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
        assert_eq!(
            onepassword.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    // ---- RF-47: hide_app for sensitive application identifiers ----

    /// RF-47 scenario "hide_app rule hides both app_id and title".
    #[test]
    fn hide_app_rule_hides_both_app_id_and_title() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "acme-corp-portal"
            hide_app = true
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate(
            "acme-corp-portal",
            RawTitle::new("Acme Corp — Client Portal"),
        );

        assert_eq!(result.app_id, "[hidden]");
        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-47 scenario "A rule without hide_app leaves app_id visible" (default `false`).
    #[test]
    fn rule_without_hide_app_leaves_app_id_visible() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "keepassxc"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate("keepassxc", RawTitle::new("KeePassXC - vault.kdbx"));

        assert_eq!(result.app_id, "keepassxc");
        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    // ---- RF-48: built-in default exclusion list, one case table per category ----

    /// A tempdir standing in for `$XDG_CONFIG_HOME`. `config_toml: None` leaves no
    /// `xwindowlog/config.toml` file at all, exercising the real `NotFound` path RF-48 depends
    /// on. Cleans up best-effort; a leaked temp dir does not affect test correctness.
    fn temp_config_home(config_toml: Option<&str>) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("xwindowlog-exclude-test-{nanos}-{n}"));

        if let Some(contents) = config_toml {
            let xwl_dir = dir.join("xwindowlog");
            fs::create_dir_all(&xwl_dir).expect("create temp XDG_CONFIG_HOME/xwindowlog");
            fs::write(xwl_dir.join("config.toml"), contents).expect("write temp config.toml");
        } else {
            fs::create_dir_all(&dir).expect("create temp XDG_CONFIG_HOME with no config.toml");
        }
        dir
    }

    /// RF-48: the default list is active even when `config.toml` does not exist at all — this is
    /// the specific claim the standing instruction calls out as the one v1.2 broke.
    #[test]
    fn defaults_active_with_no_config_file_present() {
        let config_home = temp_config_home(None);

        let excluder = Excluder::load_from_config_home(&config_home)
            .expect("no file => empty config, still valid");
        let result = excluder.evaluate("keepassxc", RawTitle::new("KeePassXC - vault.kdbx"));

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
        let _ = fs::remove_dir_all(&config_home);
    }

    /// task 7.3: `load_from_config_home` genuinely reads bytes off disk, not just parses a
    /// string handed to it in-process.
    #[test]
    fn load_from_config_home_reads_a_real_user_rule_from_disk() {
        let config_home = temp_config_home(Some(
            r#"
            [[exclude]]
            app = "myinternalapp"
            "#,
        ));

        let excluder = Excluder::load_from_config_home(&config_home).expect("valid config on disk");
        let result = excluder.evaluate(
            "myinternalapp",
            RawTitle::new("My Internal App — dashboard"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
        let _ = fs::remove_dir_all(&config_home);
    }

    /// 7.8a `password-managers`. Case table over several of RF-48's listed alternatives, not
    /// just the first one — a typo in the regex alternation would only surface this way.
    #[test]
    fn default_password_managers_case_table() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        for app_id in ["keepassxc", "bitwarden", "1password", "lastpass", "gopass"] {
            let result = excluder.evaluate(app_id, RawTitle::new("vault"));
            assert_eq!(
                result.title,
                SafeTitle::from_sanitized("[hidden]".to_string()),
                "expected {app_id:?} to match the password-managers default rule"
            );
        }
    }

    /// 7.8b `banking-generic`.
    #[test]
    fn default_banking_generic_case_table() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        for title in [
            "Revolut - Account Overview",
            "PayPal - Send Money",
            "Mi Banca Online",
        ] {
            let result = excluder.evaluate("firefox", RawTitle::new(title));
            assert_eq!(
                result.title,
                SafeTitle::from_sanitized("[hidden]".to_string()),
                "expected {title:?} to match the banking-generic default rule"
            );
        }
    }

    /// 7.8c `private-browsing`, including the Spanish alternative (real behavior on a
    /// Spanish-locale desktop, not a translation left unfinished).
    #[test]
    fn default_private_browsing_case_table() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        for title in [
            "New Incognito Tab",
            "Navegación privada - Firefox",
            "InPrivate browsing",
        ] {
            let result = excluder.evaluate("firefox", RawTitle::new(title));
            assert_eq!(
                result.title,
                SafeTitle::from_sanitized("[hidden]".to_string()),
                "expected {title:?} to match the private-browsing default rule"
            );
        }
    }

    /// 7.8d `gpg-ssh-prompts`.
    #[test]
    fn default_gpg_ssh_prompts_case_table() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        for app_id in [
            "pinentry-gtk-2",
            "pinentry-qt",
            "ssh-askpass",
            "lxqt-openssh-askpass",
        ] {
            let result = excluder.evaluate(app_id, RawTitle::new("Enter passphrase"));
            assert_eq!(
                result.title,
                SafeTitle::from_sanitized("[hidden]".to_string()),
                "expected {app_id:?} to match the gpg-ssh-prompts default rule"
            );
        }
    }

    /// 7.8e `2fa-otp`, matching on EITHER `app_id` OR title (RF-48's own table row says "title
    /// or app_id") — exercised as two distinct cases, not folded into one.
    #[test]
    fn default_2fa_otp_case_table() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let by_app = excluder.evaluate("authy", RawTitle::new("Authy"));
        assert_eq!(
            by_app.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );

        let by_title =
            excluder.evaluate("firefox", RawTitle::new("Your verification code is 123456"));
        assert_eq!(
            by_title.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-48 scenario "A default rule is individually disabled".
    #[test]
    fn a_default_rule_is_individually_disabled() {
        let excluder = Excluder::from_toml_str(
            r#"
            disable_default_excludes = ["banking-generic"]
            "#,
        )
        .expect("valid config compiles");

        let banking = excluder.evaluate("firefox", RawTitle::new("Revolut - Account Overview"));
        assert_eq!(banking.app_id, "firefox");
        assert_eq!(
            banking.title,
            SafeTitle::from_sanitized("Revolut - Account Overview".to_string())
        );

        // The other defaults stay active.
        let password_manager = excluder.evaluate("keepassxc", RawTitle::new("vault"));
        assert_eq!(
            password_manager.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-48 scenario "A window matching no rule is captured in full".
    #[test]
    fn unmatched_window_is_captured_in_full() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let result = excluder.evaluate("firefox", RawTitle::new("GitHub - foo/bar"));

        assert_eq!(result.app_id, "firefox");
        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("GitHub - foo/bar".to_string())
        );
    }

    // ---- RF-50: allowlist mode inversion ----

    /// RF-50 scenario "Allowlist mode excludes an unlisted application".
    #[test]
    fn allowlist_mode_excludes_an_unlisted_application() {
        let excluder = Excluder::from_toml_str(
            r#"
            mode = "allowlist"

            [[include]]
            app = "vscode"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate("firefox", RawTitle::new("GitHub - foo/bar"));

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-50 scenario "Allowlist mode includes a listed application".
    #[test]
    fn allowlist_mode_includes_a_listed_application() {
        let excluder = Excluder::from_toml_str(
            r#"
            mode = "allowlist"

            [[include]]
            app = "vscode"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate("vscode", RawTitle::new("main.rs - xwindowlog - VS Code"));

        assert_eq!(result.app_id, "vscode");
        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("main.rs - xwindowlog - VS Code".to_string())
        );
    }

    // ---- RF-51: secret sanitization within otherwise-visible titles ----

    /// RF-51 scenario "A GitHub token fragment is redacted", plus a case table over RF-51's
    /// other three named provider prefixes (`sk-`, `xox*-`, `AKIA`) — the spec scenario names
    /// only GitHub explicitly, but the requirement text names all four.
    #[test]
    fn provider_prefixed_token_fragment_is_redacted() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let github = excluder.evaluate(
            "firefox",
            RawTitle::new("Deploy failed: token ghp_1234567890abcdef1234567890abcdef1234 invalid"),
        );
        assert_eq!(
            github.title,
            SafeTitle::from_sanitized("Deploy failed: token [REDACTED] invalid".to_string())
        );

        let openai = excluder.evaluate(
            "firefox",
            RawTitle::new("Key sk-abcdefghijklmnopqrstuvwx leaked"),
        );
        assert_eq!(
            openai.title,
            SafeTitle::from_sanitized("Key [REDACTED] leaked".to_string())
        );

        let slack = excluder.evaluate(
            "firefox",
            RawTitle::new("Webhook xoxb-1234567890-abcdefghij posted"),
        );
        assert_eq!(
            slack.title,
            SafeTitle::from_sanitized("Webhook [REDACTED] posted".to_string())
        );

        let aws = excluder.evaluate(
            "firefox",
            RawTitle::new("Access key AKIAIOSFODNN7EXAMPLE exposed"),
        );
        assert_eq!(
            aws.title,
            SafeTitle::from_sanitized("Access key [REDACTED] exposed".to_string())
        );
    }

    /// RF-51 scenario "An email address fragment is redacted".
    #[test]
    fn email_address_fragment_is_redacted() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let result = excluder.evaluate(
            "firefox",
            RawTitle::new("Invite sent to jane.doe@example.com"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("Invite sent to [REDACTED]".to_string())
        );
    }

    /// RF-51: hexadecimal strings of 32+ characters are redacted (true positive not explicitly
    /// named as its own spec scenario, but explicitly required by RF-51's requirement text).
    #[test]
    fn long_hex_fragment_is_redacted() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let result = excluder.evaluate(
            "firefox",
            RawTitle::new("Commit a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6 pushed"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("Commit [REDACTED] pushed".to_string())
        );
    }

    /// RF-51: base64-shaped strings of 24+ characters are redacted.
    #[test]
    fn base64_shaped_fragment_is_redacted() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let result = excluder.evaluate(
            "firefox",
            RawTitle::new("Secret: QWxhZGRpbjpvcGVuIHNlc2FtZQ== copied"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("Secret: [REDACTED] copied".to_string())
        );
    }

    /// RF-51 scenario "sanitize_secrets disabled leaves fragments intact".
    #[test]
    fn sanitize_secrets_disabled_leaves_fragments_intact() {
        let excluder =
            Excluder::from_toml_str("sanitize_secrets = false").expect("valid config compiles");

        let result = excluder.evaluate(
            "firefox",
            RawTitle::new("Invite sent to jane.doe@example.com"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("Invite sent to jane.doe@example.com".to_string())
        );
    }

    /// RF-51 scenario "Secret sanitization and exclusion both apply where relevant" — exclusion
    /// wins outright; redaction is not separately visible because the whole title is `[hidden]`.
    #[test]
    fn exclusion_and_secret_redaction_both_apply_where_relevant() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "keepassxc"
            "#,
        )
        .expect("valid config compiles");

        let result = excluder.evaluate(
            "keepassxc",
            RawTitle::new("KeePassXC — jane.doe@example.com"),
        );

        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );
    }

    /// RF-51's own documented accepted false positive: a long ticket identifier gets redacted
    /// too, because it happens to be hex/base64-shaped. This is not a bug; `sanitize_secrets =
    /// false` is the documented escape hatch (covered by the test above).
    #[test]
    fn accepted_false_positive_long_ticket_identifier_is_redacted() {
        let excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let result = excluder.evaluate(
            "firefox",
            RawTitle::new("PROJ-1234567890123456789012345 - Jira"),
        );

        // The 31-character digit run after "PROJ-" is base64-shaped-enough (24+ alnum chars) to
        // be caught, exactly as RF-51 documents and accepts.
        assert_eq!(
            result.title,
            SafeTitle::from_sanitized("PROJ-[REDACTED] - Jira".to_string())
        );
    }

    // ---- RF-9: config reload on SIGHUP (unit-level hot-swap; full E2E is task 15.7) ----

    /// RF-9 scenario "Exclusion rules take effect after SIGHUP", at the `Excluder` unit level:
    /// reloading swaps the active rule set for SUBSEQUENT `evaluate` calls; it cannot and does
    /// not touch anything already returned, since `EvaluatedWindow` is an owned value with no
    /// link back to the `Excluder` that produced it.
    #[test]
    fn reload_applies_updated_rules_to_subsequent_events_only() {
        let mut excluder =
            Excluder::from_toml_str("").expect("empty config still compiles with defaults");

        let before_reload = excluder.evaluate(
            "myinternalapp",
            RawTitle::new("My Internal App — dashboard"),
        );
        assert_eq!(
            before_reload.title,
            SafeTitle::from_sanitized("My Internal App — dashboard".to_string()),
            "not excluded yet — no rule for myinternalapp"
        );

        excluder
            .reload_from_toml_str(
                r#"
                [[exclude]]
                app = "myinternalapp"
                "#,
            )
            .expect("valid reload config compiles");

        let after_reload = excluder.evaluate(
            "myinternalapp",
            RawTitle::new("My Internal App — dashboard"),
        );
        assert_eq!(
            after_reload.title,
            SafeTitle::from_sanitized("[hidden]".to_string())
        );

        // The pre-reload result, standing in for an already-recorded interval, is untouched.
        assert_eq!(
            before_reload.title,
            SafeTitle::from_sanitized("My Internal App — dashboard".to_string())
        );
    }
}
