//! Configuration loading, validation, and the built-in defaults.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnStuck {
    /// Abandon this tier and hand the whole turn to the next one.
    ///
    /// What a chain of one always gets, since there is nothing below to consult,
    /// and what you ask for when the honest answer really is "someone else should
    /// finish this". Set it with `on_stuck = "escalate"` on a tier, or for the
    /// rest of a session with `/on-stuck escalate`.
    Escalate,
    /// Keep this tier driving, and ask the next one a narrow question about it.
    ///
    /// The default, because escalating is the expensive mistake: the turn goes to
    /// the frontier, and once a tier has spilled the session stays there — one
    /// bad turn costs the cheap model for every turn after it. A consult spends
    /// one question on the tier below and the driver carries on with the answer.
    ///
    /// It only pays off when the answer is something the driver can act on, so
    /// spill escalates anyway when the answer comes back empty, when the consult
    /// fails, or when the cap is spent: a turn can never be stranded by it. See
    /// `consult.rs`.
    #[default]
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
    /// Flags that make this CLI read-only for a single run — a plan mode, a
    /// sandbox policy, a Q&A mode.
    ///
    /// Used only for a consult. A CLI runs its own harness and its own tools,
    /// which spill cannot withhold the way it can for an `openai` endpoint, so
    /// a read-only flag is the only way to keep a consultant from acting
    /// instead of answering. A tier with none is not consulted at all: the
    /// turn escalates instead. The shipped presets set this wherever the CLI
    /// has such a flag.
    #[serde(default)]
    pub read_only_args: Vec<String>,
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
    pub limits: LimitOverrides,
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

/// The limits a tier is judged by at runtime.
///
/// `Default` is the **hosted** profile, and that is deliberate: it is what every
/// tier got before there was more than one, so the call sites that do not care
/// about the distinction — tests, and a tier built by hand — keep the behaviour
/// they had. A tier from configuration gets its class's profile instead, through
/// `LimitOverrides::resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub first_token_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_repeat_run: u32,
}

impl Default for Limits {
    fn default() -> Self {
        TierClass::Hosted.default_limits()
    }
}

/// Where a tier's model actually is, which decides the timeouts it is judged by.
///
/// One profile cannot fit both ends: a 30B loading into a 5090's VRAM may need a
/// minute before its first token, while a local server that goes quiet *once it
/// is streaming* is more likely to have wedged than a hosted one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierClass {
    /// A model server on this machine or the LAN.
    Local,
    /// Reached over the network — a hosted endpoint, or a CLI tier.
    ///
    /// A CLI harness runs locally but reaches a remote model, and its first line
    /// of output includes the harness starting up (`cmd` pays ~15k tokens of its
    /// own prompt on every call). That is slow for reasons a local server is not,
    /// so a CLI keeps the hosted numbers.
    Hosted,
}

impl TierClass {
    /// Classify a tier by where its endpoint lives.
    ///
    /// Loopback, the private ranges, link-local, and mDNS names mean the model is
    /// on this machine or the LAN. Anything else is reached over the network.
    pub fn of_endpoint(base_url: &str) -> Self {
        let host = host_of(base_url);
        if is_local_host(&host) {
            Self::Local
        } else {
            Self::Hosted
        }
    }

    /// The timeouts a tier of this class is judged by, before any overrides.
    pub fn default_limits(self) -> Limits {
        match self {
            // Patient at the start, impatient once running: weights take time to
            // load, but a local server that has gone quiet mid-answer has wedged.
            Self::Local => Limits {
                first_token_timeout_ms: 120_000,
                idle_timeout_ms: 30_000,
                max_repeat_run: default_max_repeat_run(),
            },
            // A hosted endpoint that says nothing for half a minute is broken;
            // once it is answering, network variance is real and is forgiven.
            Self::Hosted => Limits {
                first_token_timeout_ms: 30_000,
                idle_timeout_ms: 60_000,
                max_repeat_run: default_max_repeat_run(),
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }
}

/// The limits a tier's config asks for.
///
/// Every field is optional so an omitted value falls through to its *class*
/// default rather than to one global number. A tier that writes only
/// `idle_timeout_ms` keeps the patient first-token budget its locality implies,
/// which is the whole point — that value is the one a slow 30B needs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LimitOverrides {
    #[serde(default)]
    pub first_token_timeout_ms: Option<u64>,
    #[serde(default)]
    pub idle_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_repeat_run: Option<u32>,
}

impl LimitOverrides {
    /// The limits this tier runs under: its class's defaults, then whatever it
    /// said itself.
    pub fn resolve(&self, class: TierClass) -> Limits {
        let base = class.default_limits();
        Limits {
            first_token_timeout_ms: self
                .first_token_timeout_ms
                .unwrap_or(base.first_token_timeout_ms),
            idle_timeout_ms: self.idle_timeout_ms.unwrap_or(base.idle_timeout_ms),
            max_repeat_run: self.max_repeat_run.unwrap_or(base.max_repeat_run),
        }
    }
}

/// The host part of a base URL, lowercased.
///
/// Hand-rolled rather than pulled from a URL crate: `base_url` is whatever the
/// user wrote, it may have no scheme at all, and the only question being asked is
/// whether the host is on this machine or the LAN.
fn host_of(base_url: &str) -> String {
    let rest = match base_url.find("://") {
        Some(at) => &base_url[at + 3..],
        None => base_url,
    };

    // Authority ends at the first path, query, or fragment.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@') // credentials, if any, are not the host
        .next()
        .unwrap_or_default();

    let host = if let Some(rest) = authority.strip_prefix('[') {
        // A bracketed IPv6 literal: the port, if any, is outside the brackets.
        match rest.find(']') {
            Some(close) => &rest[..close],
            None => rest,
        }
    } else {
        match authority.rfind(':') {
            Some(colon) => &authority[..colon],
            None => authority,
        }
    };

    host.trim().trim_end_matches('.').to_lowercase()
}

/// Whether a host names this machine or something on the LAN.
fn is_local_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if host == "localhost" || host == "0.0.0.0" || host.ends_with(".localhost") {
        return true;
    }
    // mDNS names resolve on the local link by definition.
    if host.ends_with(".local") {
        return true;
    }
    if host == "host.docker.internal" || host == "host.containers.internal" {
        return true;
    }

    // IPv6: loopback, or unique-local (fc00::/7).
    if host.contains(':') {
        let first = host.split(':').next().unwrap_or_default();
        return host == "::1"
            || host.starts_with("::ffff:127.")
            || first.starts_with("fd")
            || first.starts_with("fc");
    }

    let octets: Vec<u8> = match host
        .split('.')
        .map(|part| part.parse::<u8>())
        .collect::<Result<_, _>>()
    {
        Ok(octets) => octets,
        // Not an IPv4 literal, so a name: only the names already matched above
        // are known to be local, and a bare hostname could resolve anywhere.
        Err(_) => return false,
    };

    match octets.as_slice() {
        // 127.0.0.0/8 is a whole loopback block, not just .0.1.
        [127, ..] => true,
        [10, ..] => true,
        // 172.16.0.0/12 — the third octet decides, so 172.32 is public.
        [172, second, ..] => (16..=31).contains(second),
        [192, 168, ..] => true,
        // Link-local.
        [169, 254, ..] => true,
        _ => false,
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

            // Checked against the override rather than the resolved value: a zero
            // is a mistake whoever wrote it, and the class defaults are never
            // zero, so only something the user typed can fail here.
            if tier.limits.first_token_timeout_ms == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "tier \"{}\" has first_token_timeout_ms = 0, which would fail instantly; omit \
                     it to accept the default for its class",
                    tier.id
                )));
            }
            if tier.limits.idle_timeout_ms == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "tier \"{}\" has idle_timeout_ms = 0, which would fail on the first quiet \
                     moment; omit it to accept the default for its class",
                    tier.id
                )));
            }
            if tier.limits.max_repeat_run == Some(0) {
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

        // A working config rather than a template: two tiers, in order, and the
        // second one actually configured — because one tier is not the product.
        // "Local until it isn't" needs a tier to spill to, and an example that
        // ships with the fallback commented out is an example of half a thing.
        assert_eq!(config.tiers.len(), 2);
        assert_eq!(config.tiers[0].id, "local");
        assert_eq!(config.tiers[0].kind, TierKind::OpenAi);
        assert_eq!(config.tiers[1].id, "grok");
        assert_eq!(config.tiers[1].kind, TierKind::Cli);

        // The local tier consults rather than escalating, and the comment in the
        // example says why: escalating costs the frontier the whole conversation
        // and then every remaining turn of the session. It is also what the tier
        // would get by saying nothing — see `the_stuck_policy_defaults_to_consulting`
        // — so the line is there to be read rather than to change anything.
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Consult);
        assert_eq!(config.tiers[0].consults_per_turn, 2);

        // The second tier says nothing and takes the default with it.
        assert_eq!(config.tiers[1].on_stuck, OnStuck::Consult);
    }

    #[test]
    fn the_documented_example_leaves_a_local_model_alone_to_use_what_is_loaded() {
        // `model = ""` is deliberate and worth pinning: it is the difference
        // between a config that survives restarting LM Studio with a different
        // model and one that 404s until it is edited.
        let config = parse(include_str!("../config.example.toml")).expect("valid");
        assert_eq!(config.tiers[0].model.as_deref(), Some(""));
    }

    #[test]
    fn the_stuck_policy_defaults_to_consulting() {
        // The default is the cheap mistake, not the tidy one. Escalating throws
        // the whole turn at the tier below, and once the session has spilled it
        // stays there, so one bad turn costs the local model every turn after it.
        // A consult asks one question and the driver keeps its job.
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            "#,
        )
        .expect("a tier with no on_stuck should parse");
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Consult);
        assert_eq!(config.tiers[0].consults_per_turn, 2);
    }

    #[test]
    fn escalating_is_still_available_on_a_tier() {
        // The override, and the reason it has to keep working: a chain of one
        // has nothing to consult, and a tier whose failures are its own fault
        // is better handed over than talked to.
        let config = parse(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            on_stuck = "escalate"
            "#,
        )
        .expect("a tier that asks to escalate should parse");
        assert_eq!(config.tiers[0].on_stuck, OnStuck::Escalate);
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

    // ---- tier class and the timeouts it implies ---------------------------

    #[test]
    fn an_endpoint_on_this_machine_or_the_lan_is_local() {
        for endpoint in [
            "http://localhost:1234/v1",
            "http://localhost/v1",
            "http://127.0.0.1:1234/v1",
            // The whole 127/8 block is loopback, not just .0.1.
            "http://127.9.9.9:11434/v1",
            "http://[::1]:1234/v1",
            "http://0.0.0.0:8080/v1",
            "http://10.0.0.5:1234/v1",
            // 172.16.0.0/12 runs to 172.31.
            "http://172.16.0.9:1234/v1",
            "http://172.31.255.254:1234/v1",
            "http://192.168.1.50:1234/v1",
            "http://169.254.10.1/v1",
            "http://my-box.local:1234/v1",
            "http://host.docker.internal:1234/v1",
            // Defensive: config rejects a scheme-less base_url today, but the
            // classifier should not be the thing that gets that wrong.
            "localhost:1234/v1",
        ] {
            assert_eq!(
                TierClass::of_endpoint(endpoint),
                TierClass::Local,
                "{endpoint} should be local"
            );
        }
    }

    #[test]
    fn anything_reached_over_the_network_is_hosted() {
        for endpoint in [
            "https://openrouter.ai/api/v1",
            "https://api.x.ai/v1",
            "http://192.0.2.10:1234/v1",
            // Just outside the private /12 — the third octet decides.
            "http://172.32.0.9:1234/v1",
            "http://172.15.0.9:1234/v1",
            "http://11.0.0.5:1234/v1",
            "http://169.253.1.1/v1",
            // A bare hostname could resolve anywhere, so it is not assumed local.
            "http://my-server:1234/v1",
            "not a url at all",
            "",
        ] {
            assert_eq!(
                TierClass::of_endpoint(endpoint),
                TierClass::Hosted,
                "{endpoint:?} should be hosted"
            );
        }
    }

    #[test]
    fn a_urls_credentials_and_port_are_not_mistaken_for_the_host() {
        assert_eq!(
            TierClass::of_endpoint("http://user:pass@localhost:1234/v1"),
            TierClass::Local
        );
        assert_eq!(
            TierClass::of_endpoint("https://user:pass@openrouter.ai/api/v1"),
            TierClass::Hosted
        );
    }

    #[test]
    fn a_local_tier_is_patient_to_start_and_impatient_once_running() {
        // The two halves of "stop a runaway local model without false spills": a
        // 30B gets time to load its weights, and a server that goes quiet once it
        // is streaming is given up on sooner than a hosted one would be.
        let limits = TierClass::Local.default_limits();

        assert_eq!(limits.first_token_timeout_ms, 120_000);
        assert_eq!(limits.idle_timeout_ms, 30_000);
        assert!(
            limits.first_token_timeout_ms
                > TierClass::Hosted.default_limits().first_token_timeout_ms,
            "a local model must get more time to start than a hosted one"
        );
        assert!(
            limits.idle_timeout_ms < TierClass::Hosted.default_limits().idle_timeout_ms,
            "and less patience once it has started"
        );
    }

    #[test]
    fn a_tier_that_says_nothing_takes_its_class_defaults() {
        let overrides = LimitOverrides::default();

        assert_eq!(
            overrides.resolve(TierClass::Local),
            TierClass::Local.default_limits()
        );
        assert_eq!(
            overrides.resolve(TierClass::Hosted),
            Limits::default(),
            "the hosted profile is what every tier got before classes existed"
        );
    }

    #[test]
    fn one_written_value_does_not_drag_the_others_to_a_global_default() {
        // The case this design exists for: a local tier that sets only its idle
        // budget must keep the patient first-token budget its locality implies,
        // or the false spill it was trying to fix comes straight back.
        let overrides = LimitOverrides {
            idle_timeout_ms: Some(5_000),
            ..LimitOverrides::default()
        };
        let limits = overrides.resolve(TierClass::Local);

        assert_eq!(limits.idle_timeout_ms, 5_000, "what was written");
        assert_eq!(
            limits.first_token_timeout_ms, 120_000,
            "what was not written still follows the class, not a global 30s"
        );
    }

    #[test]
    fn a_written_timeout_wins_over_the_class_default() {
        let overrides = LimitOverrides {
            first_token_timeout_ms: Some(7_000),
            idle_timeout_ms: Some(8_000),
            max_repeat_run: Some(9),
        };
        let limits = overrides.resolve(TierClass::Hosted);

        assert_eq!(limits.first_token_timeout_ms, 7_000);
        assert_eq!(limits.idle_timeout_ms, 8_000);
        assert_eq!(limits.max_repeat_run, 9);
    }

    #[test]
    fn the_shipped_example_writes_one_override_and_inherits_the_rest() {
        // The example's [tier.limits] block sets two fields and leaves the
        // timeouts alone, so it exercises the merge rather than a wholesale
        // replacement: a field the class provides and the block does not mention
        // has to survive. Writing the class's own defaults into the example
        // would have hidden that, and hidden the mechanism with it.
        let config = parse(include_str!("../config.example.toml")).expect("valid");
        let limits = config.tiers[0].limits.resolve(TierClass::Local);

        assert_eq!(limits.idle_timeout_ms, 45_000, "the override applies");
        assert_eq!(
            limits.first_token_timeout_ms, 120_000,
            "and the local class's own first-token budget still gets through"
        );
        assert_eq!(limits.max_repeat_run, 4);
    }
}
