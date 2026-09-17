# Privacy Filtering Specification

## Purpose

Prevent sensitive window content from ever being persisted, logged, or
exposed, before it reaches interval tracking or storage. This is the
boundary described in PRD §14.3: a raw title exists only in memory between
capture and this capability; nothing downstream of it may ever see an
unsanitized title.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-7 | Config-driven exclusion rules |
| RF-8 | Hidden title with time preserved |
| RF-9 | Config reload on SIGHUP |
| RF-47 | hide_app for sensitive application identifiers |
| RF-48 | Built-in default exclusion list |
| RF-50 | Allowlist mode inversion |
| RF-51 | Secret sanitization within otherwise-visible titles |
| P3 (PRD §15.5) | Exclusion preserves elapsed time |

## Requirements

### Requirement: Config-driven exclusion rules

**Traces:** RF-7

The system MUST read exclusion rules from
`$XDG_CONFIG_HOME/xwindowlog/config.toml`, each rule matching a regular
expression against `app_id` and/or title. The system MUST guarantee that no
log line, panic message, or debug output ever prints a raw (unsanitized)
title: a title MUST pass through this capability before it is used for any
purpose other than being matched against exclusion rules.

#### Scenario: A configured rule matches on app_id

- GIVEN a config rule `app = "keepassxc"`
- WHEN a window with `app_id = "keepassxc"` is captured
- THEN the window is excluded per the Requirement: Hidden title below

#### Scenario: A configured rule matches on title

- GIVEN a config rule `title = "(?i)banco|bbva|santander|caixa"`
- WHEN a window with title `"BBVA - Online Banking"` is captured
- THEN the window is excluded

#### Scenario: No raw title is ever logged before sanitization

- GIVEN a window title that would match an exclusion rule
- WHEN the daemon logs any diagnostic, warning, or error related to that
  window before or during exclusion evaluation
- THEN the logged text never contains the raw title content

### Requirement: Hidden title with time preserved

**Traces:** RF-8

For a window matching an exclusion rule (and not covered by `hide_app`), the
system MUST store the title as `[hidden]`, MUST continue to record the
elapsed time normally, and MUST keep distinct excluded applications
distinguishable from each other by their (non-hidden) `app_id`.

#### Scenario: Excluded window still contributes to the working day

- GIVEN a window matches an exclusion rule and is active for 90 seconds
- WHEN the interval is recorded
- THEN the interval's title is `[hidden]`
- AND the interval's duration is 90 seconds, unchanged

#### Scenario: Two different excluded apps remain distinguishable

- GIVEN `keepassxc` and `1password` both match exclusion rules
- WHEN both applications are captured as active windows at different times
- THEN the stored intervals show `app_id = "keepassxc"` and
  `app_id = "1password"` respectively, both with title `[hidden]`

### Requirement: Config reload on SIGHUP

**Traces:** RF-9

The system MUST reload `config.toml` (including exclusion rules) upon
receiving `SIGHUP`, without requiring a restart, and MUST apply the reloaded
rules to subsequent events without affecting already-recorded intervals.

#### Scenario: Exclusion rules take effect after SIGHUP

- GIVEN the daemon is running with an initial set of exclusion rules
- WHEN the config file is edited to add a new rule and the daemon receives
  `SIGHUP`
- THEN subsequent captures are evaluated against the updated rule set
- AND intervals recorded before the reload are not modified

### Requirement: hide_app for sensitive application identifiers

**Traces:** RF-47

For an exclusion rule with `hide_app = true`, the system MUST additionally
store the `app_id` as `[hidden]`, not only the title.

#### Scenario: hide_app rule hides both app_id and title

- GIVEN a config rule `app = "acme-corp-portal"` with `hide_app = true`
- WHEN a window with `app_id = "acme-corp-portal"` is captured
- THEN the interval is stored with `app_id = "[hidden]"` and
  `title = "[hidden]"`

#### Scenario: A rule without hide_app leaves app_id visible

- GIVEN a config rule `app = "keepassxc"` with no `hide_app` field (default
  `false`)
- WHEN a window with `app_id = "keepassxc"` is captured
- THEN the interval is stored with `app_id = "keepassxc"` and
  `title = "[hidden]"`

### Requirement: Built-in default exclusion list

**Traces:** RF-48

The system MUST embed a default exclusion list, active even when
`config.toml` does not exist, covering at minimum the following categories.
Each category MUST be individually disableable via
`disable_default_excludes = [<id>, ...]`.

| id | Rule | Match target |
|---|---|---|
| `password-managers` | `(?i)^(keepassxc\|keepassx\|keepass\|bitwarden\|1password\|lastpass\|dashlane\|enpass\|passwordsafe\|gopass)$` | `app_id` |
| `banking-generic` | `(?i)\b(banco\|banca\|bank\|paypal\|stripe dashboard\|coinbase\|binance\|kraken\|revolut\|wise\|n26\|openbank)\b` | title |
| `private-browsing` | `(?i)(private browsing\|navegaci[oó]n privada\|inc[oó]gnito\|incognito\|inprivate\|modo privado)` | title |
| `gpg-ssh-prompts` | `(?i)^(pinentry.*\|ssh-askpass\|x11-ssh-askpass\|lxqt-openssh-askpass)$` | `app_id` |
| `2fa-otp` | `(?i)(two-?factor\|2fa\|verification code\|c[oó]digo de verificaci[oó]n\|one-?time passcode\|\botp\b)`, or `app_id` matching `(?i)^(authy\|gnome-authenticator\|otpclient)$` | title or `app_id` |

#### Scenario: password-managers default rule matches KeePassXC

- GIVEN no `config.toml` exists and default excludes are active
- WHEN a window with `app_id = "keepassxc"` is captured
- THEN the window is excluded under the `password-managers` default rule

#### Scenario: banking-generic default rule matches a bank title

- GIVEN no `config.toml` exists and default excludes are active
- WHEN a window with title `"Revolut - Account Overview"` is captured
- THEN the window is excluded under the `banking-generic` default rule

#### Scenario: private-browsing default rule matches an incognito window title

- GIVEN default excludes are active
- WHEN a window with title `"New Incognito Tab"` is captured
- THEN the window is excluded under the `private-browsing` default rule

#### Scenario: gpg-ssh-prompts default rule matches a pinentry dialog

- GIVEN default excludes are active
- WHEN a window with `app_id = "pinentry-gtk-2"` is captured
- THEN the window is excluded under the `gpg-ssh-prompts` default rule

#### Scenario: 2fa-otp default rule matches an authenticator app

- GIVEN default excludes are active
- WHEN a window with `app_id = "authy"` is captured
- THEN the window is excluded under the `2fa-otp` default rule

#### Scenario: A default rule is individually disabled

- GIVEN `disable_default_excludes = ["banking-generic"]` is set in
  `config.toml`
- WHEN a window with title `"Revolut - Account Overview"` is captured
- THEN the window is NOT excluded by the `banking-generic` rule (it may
  still be excluded by another active rule)
- AND the other default rules (for example `password-managers`) remain
  active

#### Scenario: A window matching no rule is captured in full

- GIVEN default excludes are active and no user-configured rule exists
- WHEN a window with `app_id = "firefox"` and title `"GitHub - foo/bar"` is
  captured
- THEN the interval is stored with the real `app_id` and title, unmodified

### Requirement: Allowlist mode inversion

**Traces:** RF-50

With `mode = "allowlist"`, the system MUST treat any window that does NOT
match an `[[include]]` rule as excluded, using the same evaluation and
storage behavior as the denylist path (title/app_id hiding per the rules
above), rather than a separate mechanism.

#### Scenario: Allowlist mode excludes an unlisted application

- GIVEN `mode = "allowlist"` with a single `[[include]] app = "vscode"` rule
- WHEN a window with `app_id = "firefox"` is captured
- THEN the window is excluded (its title is stored as `[hidden]`)

#### Scenario: Allowlist mode includes a listed application

- GIVEN `mode = "allowlist"` with a single `[[include]] app = "vscode"` rule
- WHEN a window with `app_id = "vscode"` is captured
- THEN the window is captured with its real `app_id` and title, not excluded

### Requirement: Secret sanitization within otherwise-visible titles

**Traces:** RF-51

With `sanitize_secrets` enabled (default `true`), the system MUST replace,
with `[REDACTED]`, only the matching fragments — not the whole title — of:
tokens with a known provider prefix (`sk-`, `ghp_`, `xox*-`, `AKIA`),
hexadecimal strings of 32 or more characters, base64-shaped strings of 24 or
more characters, and email addresses. This sanitization MUST apply
independently of, and in addition to, exclusion-rule matching.

#### Scenario: A GitHub token fragment is redacted

- GIVEN `sanitize_secrets = true` and a title
  `"Deploy failed: token ghp_1234567890abcdef1234567890abcdef1234 invalid"`
- WHEN the title is sanitized
- THEN the stored title has the token fragment replaced with `[REDACTED]`
  and the surrounding text unchanged

#### Scenario: An email address fragment is redacted

- GIVEN `sanitize_secrets = true` and a title containing
  `"Invite sent to jane.doe@example.com"`
- WHEN the title is sanitized
- THEN the email address is replaced with `[REDACTED]` and the rest of the
  title is preserved

#### Scenario: sanitize_secrets disabled leaves fragments intact

- GIVEN `sanitize_secrets = false` and a title containing an email address
- WHEN the title is sanitized
- THEN the title is stored unmodified, with the email address still present

#### Scenario: Secret sanitization and exclusion both apply where relevant

- GIVEN a title that both matches an exclusion rule and contains a
  redactable secret fragment
- WHEN the window is captured
- THEN the interval's title is `[hidden]` (exclusion takes full effect); the
  secret sanitization is not separately visible because the whole title is
  already hidden

### Requirement: Exclusion preserves elapsed time (Property P3)

**Traces:** P3 (PRD §15.5)

For any sequence of events in which some windows match exclusion rules and
others do not, the system MUST produce a total recorded duration equal to
the total duration that would have been recorded with no exclusion rules at
all: only titles and application identifiers change, never durations.

#### Scenario: Mixed excluded and non-excluded windows sum correctly

- GIVEN a scripted sequence of window changes where some windows match
  exclusion rules and others do not, spanning a fixed total duration `D`
- WHEN the sequence is processed with exclusion rules active, and separately
  with no exclusion rules active
- THEN the sum of all recorded interval durations equals `D` in both cases
