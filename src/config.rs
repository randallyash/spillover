//! Configuration loading, validation, and the built-in defaults.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::provider::dialect::Dialect;

/// The only configuration schema this build understands.
const SUPPORTED_SCHEMA: u32 = 1;

/// How many consults a tier gets per turn unless it says otherwise.
///
/// One source of truth, because the config default and the programmatic
/// construction of a tier have to agree: a tier built with a different cap from
/// the one its config file implies would be a difference nobody could see.
pub const DEFAULT_CONSULTS_PER_TURN: u32 = 2;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine a configuration directory for your platform")]
    NoConfigDir,

    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid TOML:\n{source}{hint}")]
    Parse {
        path: PathBuf,
        // Boxed: the parser's error is large, and this variant is returned by
        // every entry point that loads configuration.
        #[source]
        source: Box<toml::de::Error>,
        /// Extra guidance, or empty. Static because it is one of a few fixed
        /// sentences.
        hint: &'static str,
    },

    #[error("{0}")]
    Invalid(String),
}

/// Guidance for the errors people actually hit.
///
/// The phrasing is matched against the TOML parser's own wording, which differs
/// by what follows the backslash: `\U` reports too few unicode digits, while
/// `\m` reports a missing escaped value.
fn parse_hint(error: &toml::de::Error) -> &'static str {
    let message = error.to_string();
    if message.contains("escaped value") || message.contains("unicode value digits") {
        "\n\nHint: inside double quotes a backslash starts an escape, so a Windows path needs \
         either single quotes — workspace = 'C:\\Users\\me' — or doubled backslashes."
    } else {
        ""
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub general: General,
    #[serde(default, rename = "tier")]
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct General {
    #[serde(default = "default_workspace")]
    pub workspace: String,
    #[serde(default = "default_true")]
    pub sticky_fallback: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            sticky_fallback: true,
        }
    }
}

impl General {
    /// The workspace as an absolute path, expanding a leading `~`.
    pub fn workspace_path(&self) -> PathBuf {
        let raw = self.workspace.trim();
        if raw == "~" {
            return home().unwrap_or_else(|| PathBuf::from(raw));
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = home() {
                return home.join(rest);
            }
        }
        PathBuf::from(raw)
    }
}

fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TierKind {
    /// Any OpenAI-compatible `/chat/completions` endpoint.
    OpenAi,
    /// Any agent CLI that answers a single prompt non-interactively.
    Cli,
}

impl fmt::Display for TierKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAi => f.write_str("openai"),
            Self::Cli => f.write_str("cli"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnStuck {
    /// Abandon this tier and hand the whole turn to the next one. The default,
    /// because it is the behaviour that always works.
    #[default]
    Escalate,
    /// Keep this tier driving, and ask the next one a narrow question about it.
    ///
    /// Cheaper in frontier quota — the expensive model answers one question
    /// instead of inheriting the turn — but it only pays off when the answer is
    /// something the driver can act on. See `consult.rs`.
    Consult,
}

impl fmt::Display for OnStuck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Escalate => f.write_str("escalate"),
            Self::Consult => f.write_str("consult"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tier {
    pub id: String,
    pub kind: TierKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Name of a shipped preset, resolved to `bin`/dialect/flags at runtime.
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub bin: Option<String>,
    /// Argument templates for a `cli` tier. `{prompt}`, `{model}` and
    /// `{workspace}` are substituted, each into its own argument, and the
    /// command is never run through a shell.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub approve_args: Vec<String>,
    #[serde(default)]
    pub workdir_args: Vec<String>,
    /// Flags that open a session under an id spill chooses, with `{session}`
    /// substituted. Omit for a CLI that mints its own session id.
    #[serde(default)]
    pub session_args: Vec<String>,
    /// Flags that continue the session spill is following, with `{session}`
    /// substituted. Setting these turns on session continuity for this tier:
    /// the CLI keeps the conversation and spill sends only the new turn,
    /// instead of flattening the whole transcript into every prompt.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Overrides the dialect the preset would use.
    #[serde(default)]
    pub dialect: Option<Dialect>,
    /// Opt in to running this tier's CLI without its own permission prompts.
    #[serde(default)]
    pub approve_all: bool,
    /// What happens when this tier gets stuck.
    #[serde(default)]
    pub on_stuck: OnStuck,
    /// How many times this tier may consult within a single turn.
    ///
    /// The cap exists for the same reason the step limit does: a driver stuck in
    /// a loop would otherwise be able to spin the frontier indefinitely, and each
    /// consult is a frontier call. Once it is spent, the turn escalates, which is
    /// the behaviour that always terminates.
    #[serde(
        default = "default_consults_per_turn",
        deserialize_with = "consults_per_turn"
    )]
    pub consults_per_turn: u32,
    #[serde(default)]
    pub limits: Limits,
}

/// A missing cap takes the default; a present zero is refused.
///
/// Zero would mean "consult is configured but never happens", which is more
/// likely a mistake than a request. Leaving the key out is how you accept the
/// default, and `on_stuck = "escalate"` is how you turn consult off.
fn consults_per_turn<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "consults_per_turn = 0 would mean consult never happens; omit it to accept the \
             default, or use on_stuck = \"escalate\"",
        ));
    }
    Ok(value)
}

impl Tier {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    #[serde(default = "default_first_token_timeout_ms")]
    pub first_token_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_max_repeat_run")]
    pub max_repeat_run: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            first_token_timeout_ms: default_first_token_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            max_repeat_run: default_max_repeat_run(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: default_schema(),
            general: General::default(),
            tiers: Vec::new(),
        }
    }
}

fn default_schema() -> u32 {
    SUPPORTED_SCHEMA
}
fn default_true() -> bool {
    true
}
fn default_workspace() -> String {
    "~".to_string()
}
fn default_first_token_timeout_ms() -> u64 {
    30_000
}
fn default_idle_timeout_ms() -> u64 {
    60_000
}
fn default_max_repeat_run() -> u32 {
    4
}
fn default_consults_per_turn() -> u32 {
    DEFAULT_CONSULTS_PER_TURN
}

/// `~/.config/spill/config.toml` (or the platform equivalent).
pub fn default_path() -> Result<PathBuf, ConfigError> {
    let dirs = directories::ProjectDirs::from("", "", "spill").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Load configuration, falling back to defaults when no file exists.
    ///
    /// An explicit path that does not exist is an error, so a typo is never
    /// silently ignored. A missing default path is not: it means "not set up
    /// yet", which the app reports in its own way.
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_path()?,
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) => {
                if explicit.is_none() && source.kind() == std::io::ErrorKind::NotFound {
                    return Ok(Self::default());
                }
                return Err(ConfigError::Read { path, source });
            }
        };

        Self::parse(&path, &text)
    }

    pub fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            hint: parse_hint(&source),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        // A lone `\x` is a valid escape, so a path like C:\x86\bin parses into a
        // string holding a control character rather than failing. Nothing would
        // then work, and the cause would be invisible, so it is caught here.
        if self.general.workspace.chars().any(char::is_control) {
            return Err(ConfigError::Invalid(
                "the workspace path in [general] contains a control character, which usually \
                 means a backslash was read as an escape. Write it in single quotes — \
                 workspace = 'C:\\Users\\me' — or double the backslashes."
                    .to_string(),
            ));
        }

        if self.schema != SUPPORTED_SCHEMA {
            return Err(ConfigError::Invalid(format!(
                "this config declares schema {}, but this build understands schema {}; upgrade \
                 spill, or remove the schema line to accept the current one",
                self.schema, SUPPORTED_SCHEMA
            )));
        }

        let mut seen: Vec<&str> = Vec::new();
        for tier in &self.tiers {
            if tier.id.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "every tier needs a non-empty id, e.g. id = \"local\"".to_string(),
                ));
            }
            if seen.contains(&tier.id.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "two tiers share the id \"{}\"; tier ids must be unique because the active \
                     tier is reported by id",
                    tier.id
                )));
            }
            seen.push(&tier.id);

            match tier.kind {
                TierKind::OpenAi => {
                    let has_preset = tier
                        .preset
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|preset| !preset.is_empty());
                    let base_url = tier.base_url.as_deref().unwrap_or("").trim();

                    if base_url.is_empty() && !has_preset {
                        return Err(ConfigError::Invalid(format!(
                            "tier \"{}\" is kind = \"openai\" but has neither a base_url nor a \
                             preset; set preset = \"lmstudio\", or give the API root such as \
                             http://localhost:1234/v1",
                            tier.id
                        )));
                    }
                    if !base_url.is_empty()
                        && !base_url.starts_with("http://")
                        && !base_url.starts_with("https://")
                    {
                        return Err(ConfigError::Invalid(format!(
                            "tier \"{}\" has base_url = \"{}\", which is not an http(s) address",
                            tier.id, base_url
                        )));
                    }
                }
                TierKind::Cli => {
                    let has_preset = tier
                        .preset
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|preset| !preset.is_empty());
                    let has_bin = tier
                        .bin
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|bin| !bin.is_empty());

                    if !has_preset && !has_bin {
                        return Err(ConfigError::Invalid(format!(
                            "tier \"{}\" is kind = \"cli\" but names neither a preset nor a bin; \
                             set preset = \"grok\", or bin = \"grok\"",
                            tier.id
                        )));
                    }
                    // Without this there is no way to hand the prompt over, and
                    // the failure would only show up when the tier ran.
                    if !has_preset && tier.args.is_empty() {
                        return Err(ConfigError::Invalid(format!(
                            "tier \"{}\" has no args, so the prompt has no way to reach {}. Set \
                             preset = \"grok\", or give args = [\"-p\", \"{{prompt}}\"]",
                            tier.id,
                            tier.bin.as_deref().unwrap_or("the CLI")
                        )));
                    }
                }
            }

            if tier.limits.first_token_timeout_ms == 0 {
                return Err(ConfigError::Invalid(format!(
                    "tier \"{}\" has first_token_timeout_ms = 0, which would fail instantly; omit \
                     it to accept the default",
                    tier.id
                )));
            }
            if tier.limits.idle_timeout_ms == 0 {
                return Err(ConfigError::Invalid(format!(
                    "tier \"{}\" has idle_timeout_ms = 0, which would fail on the first quiet \
                     moment; omit it to accept the default",
                    tier.id
                )));
            }
            if tier.limits.max_repeat_run == 0 {
                return Err(ConfigError::Invalid(format!(
                    "tier \"{}\" has max_repeat_run = 0, which disables repetition detection; omit \
                     it to accept the default",
                    tier.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(&PathBuf::from("test.toml"), text)
    }

    #[test]
    fn empty_config_is_valid_and_has_no_tiers() {
        let config = parse("").expect("empty config should parse");
        assert_eq!(config.schema, 1);
        assert!(config.tiers.is_empty());
        assert!(config.general.sticky_fallback);
        assert_eq!(config.general.workspace, "~");
    }

    #[test]
    fn accepts_the_documented_example() {
        let text = include_str!("../config.example.toml");
        let config = parse(text).expect("config.example.toml must stay valid");
        assert_eq!(config.tiers.len(), 1);
        assert_eq!(config.tiers[0].id, "local");
        assert_eq!(config.tiers[0].kind, TierKind::OpenAi);
        assert_eq!(config.tiers[0].limits.max_repeat_run, 4);
        // The example documents consult but does not turn it on: the default has
        // to stay the behaviour that always works.
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Escalate);
        assert_eq!(config.tiers[0].consults_per_turn, DEFAULT_CONSULTS_PER_TURN);
    }

    #[test]
    fn the_stuck_policy_defaults_to_escalating() {
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            "#,
        )
        .expect("a tier with no on_stuck should parse");
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Escalate);
        assert_eq!(config.tiers[0].consults_per_turn, 2);
    }

    #[test]
    fn consult_can_be_turned_on_and_the_cap_set() {
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            on_stuck = "consult"
            consults_per_turn = 5
            "#,
        )
        .expect("consult should parse");
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Consult);
        assert_eq!(config.tiers[0].consults_per_turn, 5);
    }

    #[test]
    fn a_consult_cap_of_zero_is_refused_with_the_reason() {
        // Zero would mean "configured but never happens", which is far more
        // likely a mistake than a request.
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            consults_per_turn = 0
            "#,
        )
        .expect_err("zero must be refused");
        let message = err.to_string();
        assert!(message.contains("consults_per_turn"), "got: {message}");
        assert!(
            message.contains("escalate"),
            "it should say how to turn consult off: {message}"
        );
    }

    #[test]
    fn an_unknown_stuck_policy_is_a_parse_error() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            on_stuck = "panic"
            "#,
        )
        .expect_err("an unknown policy must not be silently accepted");
        assert!(err.to_string().contains("not valid TOML"), "got: {err}");
    }

    #[test]
    fn parses_a_cli_tier_by_preset() {
        let config = parse(
            r#"
            [[tier]]
            id = "grok"
            kind = "cli"
            preset = "grok"
            model = "grok-4.6"
            "#,
        )
        .expect("cli tier with a preset should parse");
        assert_eq!(config.tiers[0].kind, TierKind::Cli);
        assert_eq!(config.tiers[0].display_name(), "grok");
    }

    #[test]
    fn display_name_prefers_name_over_id() {
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            name = "Local (LM Studio)"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            "#,
        )
        .expect("tier with a name should parse");
        assert_eq!(config.tiers[0].display_name(), "Local (LM Studio)");
    }

    #[test]
    fn rejects_openai_tier_without_base_url() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            "#,
        )
        .expect_err("missing base_url must fail");
        assert!(
            err.to_string().contains("neither a base_url nor a preset"),
            "got: {err}"
        );
    }

    #[test]
    fn accepts_an_openai_tier_that_names_a_preset_instead_of_a_url() {
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            preset = "lmstudio"
            "#,
        )
        .expect("a preset is a valid alternative to a base_url");
        assert_eq!(config.tiers[0].preset.as_deref(), Some("lmstudio"));
    }

    #[test]
    fn rejects_base_url_without_a_scheme() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "localhost:1234/v1"
            "#,
        )
        .expect_err("scheme-less base_url must fail");
        assert!(
            err.to_string().contains("not an http(s) address"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_cli_tier_without_bin_or_preset() {
        let err = parse(
            r#"
            [[tier]]
            id = "grok"
            kind = "cli"
            "#,
        )
        .expect_err("cli tier needs a bin or preset");
        assert!(
            err.to_string().contains("neither a preset nor a bin"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_tier_ids() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"

            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:5678/v1"
            "#,
        )
        .expect_err("duplicate ids must fail");
        assert!(err.to_string().contains("unique"), "got: {err}");
    }

    #[test]
    fn rejects_zero_value_limits() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            [tier.limits]
            max_repeat_run = 0
            "#,
        )
        .expect_err("max_repeat_run = 0 must fail");
        assert!(err.to_string().contains("max_repeat_run"), "got: {err}");
    }

    #[test]
    fn rejects_zero_idle_timeout() {
        let err = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            [tier.limits]
            idle_timeout_ms = 0
            "#,
        )
        .expect_err("idle_timeout_ms = 0 must fail");
        assert!(err.to_string().contains("idle_timeout_ms"), "got: {err}");
    }

    #[test]
    fn rejects_an_unknown_schema() {
        let err = parse("schema = 99").expect_err("unknown schema must fail");
        assert!(err.to_string().contains("schema 99"), "got: {err}");
    }

    #[test]
    fn a_windows_path_in_single_quotes_parses() {
        // A literal TOML string takes backslashes as-is, which is the easy way
        // to write a Windows path.
        let config = parse(
            r#"
            [general]
            workspace = 'C:\Users\me\project'
            "#,
        )
        .expect("a literal string should accept backslashes");
        assert_eq!(config.general.workspace, r"C:\Users\me\project");
    }

    #[test]
    fn a_windows_path_in_double_quotes_fails_with_a_hint() {
        // `\U` reads as the start of a unicode escape, which is why this is the
        // error people will actually meet.
        let err = parse(
            r#"
            [general]
            workspace = "C:\Users\me\project"
            "#,
        )
        .expect_err("an unescaped backslash is not valid TOML");

        let message = err.to_string();
        assert!(message.contains("not valid TOML"), "got: {message}");
        assert!(
            message.contains("single quotes"),
            "the error should say how to fix it: {message}"
        );
    }

    #[test]
    fn a_backslash_before_a_letter_also_gets_the_hint() {
        // A different wording from the parser, and a different sentence from us.
        let err = parse(
            r#"
            [general]
            workspace = "C:\mystuff"
            "#,
        )
        .expect_err("an unescaped backslash is not valid TOML");
        assert!(err.to_string().contains("single quotes"), "got: {err}");
    }

    #[test]
    fn a_backslash_that_parses_silently_is_caught_as_a_control_character() {
        // `\x86` IS a valid escape, so this parses happily into a string holding
        // a control character. Left alone it surfaces much later as "file not
        // found", so it is rejected here with an explanation instead.
        let err = parse(
            r#"
            [general]
            workspace = "C:\x86"
            "#,
        )
        .expect_err("a control character in the workspace must be refused");

        let message = err.to_string();
        assert!(message.contains("control character"), "got: {message}");
        assert!(
            message.contains("single quotes"),
            "the error should say how to fix it: {message}"
        );
    }

    #[test]
    fn an_unrelated_toml_error_gets_no_path_hint() {
        // The advice is specific, so it should not appear on a plain syntax slip.
        let err = parse("[[tier]\nid = ").expect_err("bad toml must fail");
        assert!(!err.to_string().contains("single quotes"), "got: {err}");
    }

    #[test]
    fn reports_malformed_toml_with_the_path() {
        let err = parse("[[tier]\nid = ").expect_err("bad toml must fail");
        assert!(err.to_string().contains("test.toml"), "got: {err}");
        assert!(err.to_string().contains("not valid TOML"), "got: {err}");
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        let err = Config::load(Some(&PathBuf::from("/nonexistent/spill-config.toml")))
            .expect_err("explicit missing path must fail");
        assert!(err.to_string().contains("cannot read"), "got: {err}");
    }
}
