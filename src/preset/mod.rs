//! The shipped preset library: known endpoints and agent CLIs.
//!
//! Presets exist so that setting up a tier is picking a name rather than
//! knowing a base URL or an argument vector. Everything here is compiled into
//! the binary, so the wizard works with no network at all.

use serde::Deserialize;
use thiserror::Error;

use crate::config::Tier;
use crate::provider::cli::CliSpec;
use crate::provider::dialect::Dialect;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct PresetError(String);

#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub local: Vec<Endpoint>,
    #[serde(default)]
    pub online: Vec<Endpoint>,
    #[serde(default)]
    pub cli: Vec<Cli>,
}

/// An OpenAI-compatible endpoint that a tier can point at.
#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// The environment variable holding the key, when one is needed.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cli {
    pub id: String,
    pub name: String,
    pub bin: String,
    #[serde(default)]
    pub dialect: Dialect,
    pub args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub approve_args: Vec<String>,
    /// Flags that make this CLI read-only for one run, used only when the tier
    /// is a consultant: spill cannot withhold tools from a harness it does not
    /// run, so a flag is the only way to stop a consultant acting. Empty means
    /// "this CLI cannot be consulted", and the turn escalates instead.
    #[serde(default)]
    pub read_only_args: Vec<String>,
    #[serde(default)]
    pub workdir_args: Vec<String>,
    /// Flags that open a session under an id spill chooses, `{session}`
    /// substituted. Empty when the CLI mints its own session id.
    #[serde(default)]
    pub session_args: Vec<String>,
    /// Flags that continue a session, `{session}` substituted.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Models worth offering in the wizard. Empty means "the CLI's own default".
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Library {
    /// The library that ships with this build.
    pub fn embedded() -> Self {
        toml::from_str(include_str!("presets.toml"))
            .expect("the shipped preset library must be valid")
    }

    pub fn find_local(&self, id: &str) -> Option<&Endpoint> {
        self.local.iter().find(|preset| preset.id == id)
    }

    pub fn find_online(&self, id: &str) -> Option<&Endpoint> {
        self.online.iter().find(|preset| preset.id == id)
    }

    pub fn find_cli(&self, id: &str) -> Option<&Cli> {
        self.cli.iter().find(|preset| preset.id == id)
    }

    pub fn cli_ids(&self) -> Vec<&str> {
        self.cli.iter().map(|preset| preset.id.as_str()).collect()
    }

    pub fn endpoint_ids(&self) -> Vec<&str> {
        self.local
            .iter()
            .chain(&self.online)
            .map(|preset| preset.id.as_str())
            .collect()
    }

    /// Everything on offer, for `spill presets`.
    pub fn catalogue(&self) -> String {
        let mut out = String::new();

        let mut section = |title: &str, rows: Vec<String>| {
            if rows.is_empty() {
                return;
            }
            out.push_str(title);
            out.push('\n');
            for row in rows {
                out.push_str("  ");
                out.push_str(&row);
                out.push('\n');
            }
            out.push('\n');
        };

        section(
            "local servers — no key needed",
            self.local
                .iter()
                .map(|preset| {
                    let mut row =
                        format!("{:<12} {}  ({})", preset.id, preset.name, preset.base_url);
                    if let Some(note) = &preset.note {
                        row.push_str(&format!("\n             {note}"));
                    }
                    row
                })
                .collect(),
        );

        section(
            "hosted endpoints — key read from an environment variable",
            self.online
                .iter()
                .map(|preset| {
                    let key = preset.api_key_env.as_deref().unwrap_or("none");
                    let mut row = format!(
                        "{:<12} {}  ({} · ${})",
                        preset.id, preset.name, preset.base_url, key
                    );
                    if let Some(note) = &preset.note {
                        row.push_str(&format!("\n             {note}"));
                    }
                    row
                })
                .collect(),
        );

        section(
            "agent CLIs — used as a tier, with their own tools",
            self.cli
                .iter()
                .map(|preset| {
                    let models = if preset.models.is_empty() {
                        String::new()
                    } else {
                        format!(" · models: {}", preset.models.join(", "))
                    };
                    let mut row = format!(
                        "{:<12} {}  (bin: {}){models}",
                        preset.id, preset.name, preset.bin
                    );
                    if let Some(note) = &preset.note {
                        row.push_str(&format!("\n             {note}"));
                    }
                    row
                })
                .collect(),
        );

        out.trim_end().to_string()
    }
}

/// The endpoint settings an `openai` tier needs, once a preset has filled in
/// whatever the tier did not say itself.
#[derive(Debug, Clone)]
pub struct OpenAiSettings {
    pub base_url: String,
    pub api_key_env: Option<String>,
}

/// Resolve an `openai` tier's endpoint, letting the tier's own values win over
/// the preset it names.
pub fn openai_settings(library: &Library, tier: &Tier) -> Result<OpenAiSettings, PresetError> {
    let preset = match tier.preset.as_deref() {
        Some(id) => Some(
            library
                .find_local(id)
                .or_else(|| library.find_online(id))
                .ok_or_else(|| {
                    PresetError(format!(
                        "tier \"{}\" names preset \"{id}\", which is not a known endpoint. \
                         Available: {}",
                        tier.id,
                        library.endpoint_ids().join(", ")
                    ))
                })?,
        ),
        None => None,
    };

    let base_url = tier
        .base_url
        .clone()
        .or_else(|| preset.map(|preset| preset.base_url.clone()))
        .unwrap_or_default();

    if base_url.trim().is_empty() {
        return Err(PresetError(format!(
            "tier \"{}\" has no base_url. Set preset = \"lmstudio\", or give the API root, for \
             example http://localhost:1234/v1",
            tier.id
        )));
    }

    Ok(OpenAiSettings {
        base_url,
        api_key_env: tier
            .api_key_env
            .clone()
            .or_else(|| preset.and_then(|preset| preset.api_key_env.clone())),
    })
}

/// Work out how to run a `cli` tier, letting the tier's own settings win over
/// the preset it names.
pub fn cli_spec(library: &Library, tier: &Tier) -> Result<CliSpec, PresetError> {
    let preset = match tier.preset.as_deref() {
        Some(id) => Some(library.find_cli(id).ok_or_else(|| {
            PresetError(format!(
                "tier \"{}\" names preset \"{id}\", which is not one of: {}",
                tier.id,
                library.cli_ids().join(", ")
            ))
        })?),
        None => None,
    };

    let choose = |from_tier: &[String], from_preset: Option<&[String]>| -> Vec<String> {
        if !from_tier.is_empty() {
            from_tier.to_vec()
        } else {
            from_preset.map(<[String]>::to_vec).unwrap_or_default()
        }
    };

    let bin = tier
        .bin
        .clone()
        .or_else(|| preset.map(|preset| preset.bin.clone()))
        .unwrap_or_default();

    // A resolved spec always has a bin and somewhere to put the prompt: config
    // validation enforces that for hand-written tiers, and the preset tests
    // enforce it for shipped ones. Resolving stays a pure lookup.
    let args = choose(&tier.args, preset.map(|preset| preset.args.as_slice()));

    Ok(CliSpec {
        bin,
        args,
        model_args: choose(
            &tier.model_args,
            preset.map(|preset| preset.model_args.as_slice()),
        ),
        extra_args: choose(
            &tier.extra_args,
            preset.map(|preset| preset.extra_args.as_slice()),
        ),
        approve_args: choose(
            &tier.approve_args,
            preset.map(|preset| preset.approve_args.as_slice()),
        ),
        read_only_args: choose(
            &tier.read_only_args,
            preset.map(|preset| preset.read_only_args.as_slice()),
        ),
        workdir_args: choose(
            &tier.workdir_args,
            preset.map(|preset| preset.workdir_args.as_slice()),
        ),
        session_args: choose(
            &tier.session_args,
            preset.map(|preset| preset.session_args.as_slice()),
        ),
        resume_args: choose(
            &tier.resume_args,
            preset.map(|preset| preset.resume_args.as_slice()),
        ),
        approve_all: tier.approve_all,
        model: tier.model.clone().filter(|model| !model.trim().is_empty()),
        dialect: tier
            .dialect
            .unwrap_or_else(|| preset.map(|preset| preset.dialect).unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, TierKind};
    use std::collections::HashSet;
    use std::path::Path;

    fn tier(text: &str) -> Tier {
        // Most of these tests are about cli tiers, but a caller that supplies
        // its own kind gets to keep it.
        let kind = if text.contains("kind =") {
            ""
        } else {
            "kind = \"cli\""
        };
        let config = Config::parse(
            Path::new("test.toml"),
            &format!(
                r#"
                [[tier]]
                id = "second"
                {kind}
                {text}
                "#
            ),
        )
        .expect("the test tier should be valid");
        config.tiers.into_iter().next().expect("one tier")
    }

    #[test]
    fn the_shipped_library_parses() {
        let library = Library::embedded();
        assert!(!library.local.is_empty());
        assert!(!library.online.is_empty());
        assert!(!library.cli.is_empty());
    }

    #[test]
    fn the_shipped_library_has_no_duplicate_ids() {
        let library = Library::embedded();
        let mut seen = HashSet::new();
        for id in library
            .local
            .iter()
            .chain(&library.online)
            .map(|preset| preset.id.as_str())
            .chain(library.cli.iter().map(|preset| preset.id.as_str()))
        {
            assert!(seen.insert(id.to_string()), "duplicate preset id {id}");
        }
    }

    #[test]
    fn every_endpoint_preset_has_a_usable_base_url() {
        let library = Library::embedded();
        for preset in library.local.iter().chain(&library.online) {
            assert!(
                preset.base_url.starts_with("http://") || preset.base_url.starts_with("https://"),
                "{} has base_url {:?}",
                preset.id,
                preset.base_url
            );
            assert!(!preset.name.trim().is_empty(), "{} needs a name", preset.id);
        }
    }

    #[test]
    fn every_cli_preset_places_the_prompt() {
        let library = Library::embedded();
        for preset in &library.cli {
            assert!(
                preset.args.iter().any(|arg| arg.contains("{prompt}")),
                "{} never passes the prompt",
                preset.id
            );
            assert!(!preset.bin.trim().is_empty(), "{} has no bin", preset.id);
        }
    }

    #[test]
    fn every_placeholder_a_preset_uses_is_one_we_substitute() {
        // A typo like {promt} would silently reach the CLI as literal text.
        let library = Library::embedded();
        for preset in &library.cli {
            let all = preset
                .args
                .iter()
                .chain(&preset.model_args)
                .chain(&preset.extra_args)
                .chain(&preset.approve_args)
                .chain(&preset.read_only_args)
                .chain(&preset.workdir_args)
                .chain(&preset.session_args)
                .chain(&preset.resume_args);
            for arg in all {
                let mut rest = arg.as_str();
                while let Some(start) = rest.find('{') {
                    let Some(end) = rest[start..].find('}') else {
                        panic!("{} has an unclosed placeholder in {arg:?}", preset.id);
                    };
                    let name = &rest[start + 1..start + end];
                    assert!(
                        ["prompt", "model", "workspace", "session"].contains(&name),
                        "{} uses unknown placeholder {{{name}}}",
                        preset.id
                    );
                    rest = &rest[start + end + 1..];
                }
            }
        }
    }

    #[test]
    fn only_local_presets_are_keyless() {
        let library = Library::embedded();
        for preset in &library.local {
            assert!(
                preset.api_key_env.is_none(),
                "{} is local and should not need a key",
                preset.id
            );
        }
        for preset in &library.online {
            assert!(
                preset.api_key_env.is_some(),
                "{} is hosted and should name an env var for its key",
                preset.id
            );
        }
    }

    #[test]
    fn a_preset_fills_in_the_invocation() {
        let library = Library::embedded();
        let spec = cli_spec(&library, &tier(r#"preset = "grok""#)).expect("grok should resolve");

        assert_eq!(spec.bin, "grok");
        assert_eq!(spec.dialect, Dialect::Grok);
        assert!(spec.args.contains(&"{prompt}".to_string()));
        assert!(spec.extra_args.contains(&"streaming-json".to_string()));
        assert!(!spec.approve_all, "unattended runs must be opt-in");
    }

    #[test]
    fn a_tier_model_is_carried_into_the_spec() {
        let library = Library::embedded();
        let spec = cli_spec(
            &library,
            &tier(
                r#"preset = "grok"
model = "grok-4.6""#,
            ),
        )
        .expect("grok should resolve");
        assert_eq!(spec.model.as_deref(), Some("grok-4.6"));
    }

    #[test]
    fn the_tiers_own_values_win_over_the_preset() {
        let library = Library::embedded();
        let spec = cli_spec(
            &library,
            &tier(
                r#"preset = "grok"
bin = "my-grok-fork"
args = ["--ask", "{prompt}"]
dialect = "plain""#,
            ),
        )
        .expect("the override should resolve");

        assert_eq!(spec.bin, "my-grok-fork");
        assert_eq!(spec.args, vec!["--ask", "{prompt}"]);
        assert_eq!(spec.dialect, Dialect::Plain);
        // Untouched fields still come from the preset.
        assert!(spec.extra_args.contains(&"streaming-json".to_string()));
    }

    #[test]
    fn a_longhand_cli_tier_needs_no_preset_at_all() {
        let library = Library::embedded();
        let spec = cli_spec(
            &library,
            &tier(
                r#"bin = "my-agent"
args = ["--once", "{prompt}"]"#,
            ),
        )
        .expect("a hand-written cli tier should work");
        assert_eq!(spec.bin, "my-agent");
        assert_eq!(spec.dialect, Dialect::Plain, "plain is the safe default");
    }

    #[test]
    fn a_preset_that_can_name_a_session_ships_both_flags() {
        let library = Library::embedded();
        let spec = cli_spec(&library, &tier(r#"preset = "grok""#)).expect("grok should resolve");

        assert!(spec.continues_sessions());
        assert!(spec.session_args.contains(&"{session}".to_string()));
        assert!(spec.resume_args.contains(&"{session}".to_string()));
        assert!(
            !spec.captures_session(),
            "grok takes the UUID we hand it, so there is nothing to read back"
        );
    }

    #[test]
    fn a_preset_that_cannot_name_a_session_still_resumes_one() {
        let library = Library::embedded();
        let spec =
            cli_spec(&library, &tier(r#"preset = "command-code""#)).expect("cmd should resolve");

        assert!(spec.continues_sessions());
        assert!(
            spec.session_args.is_empty(),
            "cmd has no flag to open a session under a chosen id"
        );
        assert!(
            spec.captures_session(),
            "so the id must be read from what it prints"
        );
    }

    #[test]
    fn a_preset_with_no_session_flags_sends_the_whole_transcript() {
        let library = Library::embedded();
        let spec =
            cli_spec(&library, &tier(r#"preset = "claude""#)).expect("claude should resolve");
        assert!(
            !spec.continues_sessions(),
            "continuity is opt-in per preset, never assumed"
        );
    }

    #[test]
    fn session_continuity_is_off_when_a_tier_is_written_longhand() {
        let library = Library::embedded();
        // The escape hatch from a preset's session flags: name the binary and
        // the arguments yourself. An empty list falls back to the preset, so
        // writing it out is how a tier declines continuity.
        let spec = cli_spec(
            &library,
            &tier(
                r#"bin = "grok"
args = ["-p", "{prompt}", "-m", "grok-4.6"]"#,
            ),
        )
        .expect("a hand-written tier needs no preset");

        assert!(!spec.continues_sessions());
    }

    #[test]
    fn an_unknown_preset_lists_the_ones_that_exist() {
        let library = Library::embedded();
        let error = cli_spec(&library, &tier(r#"preset = "nonexistent""#))
            .expect_err("an unknown preset must fail");
        let message = error.to_string();
        assert!(message.contains("nonexistent"), "{message}");
        assert!(
            message.contains("grok"),
            "it should list the real ids: {message}"
        );
    }

    #[test]
    fn every_preset_id_resolves_to_the_kind_it_belongs_to() {
        let library = Library::embedded();
        for preset in &library.cli {
            assert!(library.find_cli(&preset.id).is_some());
            assert!(library.find_local(&preset.id).is_none());
        }
        for preset in &library.local {
            assert!(library.find_local(&preset.id).is_some());
            assert!(library.find_online(&preset.id).is_none());
        }
        assert!(library.find_online("openrouter").is_some());
        assert!(library.find_local("lmstudio").is_some());
    }

    #[test]
    fn tier_kind_cli_is_the_one_that_uses_presets() {
        let config = Config::parse(
            Path::new("test.toml"),
            r#"
            [[tier]]
            id = "grok"
            kind = "cli"
            preset = "grok"
            "#,
        )
        .expect("valid");
        assert_eq!(config.tiers[0].kind, TierKind::Cli);
    }

    #[test]
    fn an_endpoint_preset_fills_in_the_base_url() {
        let library = Library::embedded();
        let settings = openai_settings(
            &library,
            &tier(
                r#"kind = "openai"
preset = "lmstudio""#,
            ),
        )
        .expect("the preset should resolve");

        assert_eq!(settings.base_url, "http://localhost:1234/v1");
        assert!(
            settings.api_key_env.is_none(),
            "a local server needs no key"
        );
    }

    #[test]
    fn a_hosted_endpoint_preset_supplies_the_key_variable() {
        let library = Library::embedded();
        let settings = openai_settings(
            &library,
            &tier(
                r#"kind = "openai"
preset = "openrouter""#,
            ),
        )
        .expect("the preset should resolve");

        assert_eq!(settings.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(settings.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn a_tier_can_override_a_preset_endpoint() {
        let library = Library::embedded();
        let settings = openai_settings(
            &library,
            &tier(
                r#"kind = "openai"
preset = "lmstudio"
base_url = "http://192.168.1.50:1234/v1""#,
            ),
        )
        .expect("the override should resolve");

        assert_eq!(settings.base_url, "http://192.168.1.50:1234/v1");
    }

    #[test]
    fn a_known_endpoint_preset_is_required_to_exist() {
        let library = Library::embedded();
        let error = openai_settings(
            &library,
            &tier(
                r#"kind = "openai"
preset = "nope""#,
            ),
        )
        .expect_err("an unknown endpoint preset must fail");
        let message = error.to_string();
        assert!(message.contains("nope"), "{message}");
        assert!(
            message.contains("lmstudio"),
            "it should list the real ids: {message}"
        );
    }

    #[test]
    fn the_catalogue_lists_every_preset_with_its_key_or_binary() {
        let catalogue = Library::embedded().catalogue();

        assert!(catalogue.contains("lmstudio"), "{catalogue}");
        assert!(
            catalogue.contains("http://localhost:1234/v1"),
            "{catalogue}"
        );
        assert!(catalogue.contains("openrouter"), "{catalogue}");
        assert!(catalogue.contains("$OPENROUTER_API_KEY"), "{catalogue}");
        assert!(catalogue.contains("grok"), "{catalogue}");
        assert!(catalogue.contains("bin: grok"), "{catalogue}");
        assert!(
            catalogue.contains("grok-4.6"),
            "it should suggest models: {catalogue}"
        );
    }

    #[test]
    fn a_preset_that_can_run_read_only_supplies_the_flag() {
        // This is what lets the tier answer a consult without being able to
        // touch the workspace, and it is the shipped default so nobody has to
        // know the flag themselves.
        let library = Library::embedded();
        let spec = cli_spec(&library, &tier(r#"preset = "grok""#)).expect("grok should resolve");
        assert_eq!(spec.read_only_args.join(" "), "--permission-mode plan");
    }

    #[test]
    fn a_preset_with_no_read_only_mode_cannot_consult() {
        // The other half of the same rule: a CLI that offers no way to be held
        // read-only is left with an empty list, which is what makes the agent
        // refuse to consult it rather than ask and hope.
        let library = Library::embedded();
        let spec =
            cli_spec(&library, &tier(r#"preset = "opencode""#)).expect("opencode should resolve");
        assert!(
            spec.read_only_args.is_empty(),
            "opencode has no read-only flag, so it cannot be a consultant"
        );
    }
}
