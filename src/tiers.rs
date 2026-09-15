//! Turning configuration into runnable tiers, and into the warnings worth
//! showing before anything goes wrong.
//!
//! Three front ends need this — the interactive app, `spill doctor`, and
//! `spill -p` — so it lives here rather than in any one of them.

use std::path::Path;
use std::sync::Arc;

use crate::config::{Config, TierClass, TierKind};
use crate::fallback::{FallbackChain, Tier};
use crate::preset::{Library, cli_spec, openai_settings};
use crate::provider::Provider;
use crate::provider::cli::{CliProvider, on_path};
use crate::provider::openai::OpenAiProvider;

/// What to say when spill cannot start a session at all.
pub const NEXT_STEPS: &str = "Run `spill presets` to see what a tier can be, or `spill setup` \
                              to choose tiers and write a config.";

/// The same, for a chain that was built and then answered nothing.
pub const UNREACHABLE_NEXT_STEPS: &str = "Run `spill doctor` to see which tiers are reachable, \
                                           or `spill setup` to change them.";

/// Anything the user should be told before it bites them: a key variable that
/// is not set, a CLI that is not installed, a tier that runs unattended.
pub fn notes(library: &Library, config: &Config) -> Vec<String> {
    let mut notes = Vec::new();

    for tier in &config.tiers {
        match tier.kind {
            TierKind::OpenAi => {
                let Ok(settings) = openai_settings(library, tier) else {
                    continue;
                };
                if let Some(name) = settings.api_key_env.as_deref() {
                    if std::env::var(name).is_err() {
                        notes.push(format!(
                            "tier \"{}\" reads its key from ${name}, which is not set yet, so it \
                             will fail until you export it",
                            tier.id
                        ));
                    }
                }
            }
            TierKind::Cli => {
                if let Ok(spec) = cli_spec(library, tier) {
                    if !on_path(&spec.bin) {
                        notes.push(format!(
                            "tier \"{}\" runs \"{}\", which is not on PATH — install it, or point \
                             the tier at it with bin = \"/full/path/to/{}\"",
                            tier.id, spec.bin, spec.bin
                        ));
                    }
                }
            }
        }
    }

    // A delegated CLI signs in for itself, so a model credential in this
    // environment would only redirect its billing. Saying so, once, means the
    // removal is never the invisible cause of a CLI that cannot find its key.
    let delegated: Vec<&str> = config
        .tiers
        .iter()
        .filter(|tier| tier.kind == TierKind::Cli)
        .map(|tier| tier.display_name())
        .collect();
    let credentials = crate::spawn::inherited_credentials();
    if !delegated.is_empty() && !credentials.is_empty() {
        notes.push(format!(
            "{} sign in for themselves and are given their own credentials, so spill removes {} \
             from their environment rather than let a key meant for another tier change which \
             account is billed",
            delegated.join(", "),
            credentials.join(", ")
        ));
    }

    // Running a model without its own guardrails is worth saying out loud.
    let unattended: Vec<&str> = config
        .tiers
        .iter()
        .filter(|tier| tier.approve_all)
        .map(|tier| tier.display_name())
        .collect();
    if !unattended.is_empty() {
        notes.push(format!(
            "these tiers run without asking first, so they can change files and run commands \
             unattended: {}",
            unattended.join(", ")
        ));
    }

    notes
}

/// Which class a configured tier belongs to.
pub fn class_of(library: &Library, tier: &crate::config::Tier) -> TierClass {
    match tier.kind {
        TierKind::OpenAi => openai_settings(library, tier)
            .map(|settings| TierClass::of_endpoint(&settings.base_url))
            .unwrap_or(TierClass::Hosted),
        // A CLI harness runs locally but reaches a remote model, and its first
        // line of output includes the harness starting up. That is slow for
        // reasons a local server is not, so it keeps the hosted numbers.
        TierKind::Cli => TierClass::Hosted,
    }
}

/// Build a provider for each configured tier, in order.
pub async fn build(
    library: &Library,
    config: &Config,
    workspace: &Path,
) -> Result<Vec<Tier>, String> {
    let mut tiers = Vec::new();
    let mut left_out: Vec<String> = Vec::new();

    for tier in &config.tiers {
        match tier.kind {
            TierKind::OpenAi => {
                let settings = openai_settings(library, tier)
                    .map_err(|error| format!("tier \"{}\": {error}. {NEXT_STEPS}", tier.id))?;

                let api_key = settings
                    .api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok());

                // An empty model means "whatever this server has loaded".
                let model = match tier.model.as_deref().filter(|model| !model.is_empty()) {
                    Some(model) => model.to_string(),
                    None => {
                        crate::provider::openai::first_model(&settings.base_url, api_key.as_deref())
                            .await
                            .map_err(|error| {
                                format!("tier \"{}\": {error}. {NEXT_STEPS}", tier.id)
                            })?
                    }
                };

                // Where the endpoint lives decides the timeouts this tier is
                // judged by: a model on the LAN may need a minute to load weights
                // before its first token, where a hosted one that is silent for
                // half a minute is simply broken.
                let limits = tier.limits.resolve(class_of(library, tier));
                let provider = OpenAiProvider::new(tier.display_name(), settings.base_url, api_key);
                let label = provider.describe();
                let mut built =
                    Tier::with_id(tier.id.clone(), label, model, Arc::new(provider), limits);
                built.on_stuck = tier.on_stuck;
                built.consults_per_turn = tier.consults_per_turn;
                tiers.push(built);
            }
            TierKind::Cli => {
                let spec = cli_spec(library, tier)
                    .map_err(|error| format!("tier \"{}\": {error}. {NEXT_STEPS}", tier.id))?;

                if !on_path(&spec.bin) {
                    left_out.push(format!(
                        "tier \"{}\" runs \"{}\", which is not on PATH",
                        tier.id, spec.bin
                    ));
                    continue;
                }
                // A CLI tier's model is its own business; this value only keeps
                // the request from carrying an empty model name.
                let model = spec.model.clone().unwrap_or_else(|| tier.id.clone());
                let provider = CliProvider::new(tier.display_name(), spec, workspace.to_path_buf());
                let label = provider.describe();
                // A CLI harness runs locally but reaches a remote model, and its
                // first line of output includes the harness starting up. That is
                // slow for reasons a local server is not, so it keeps the hosted
                // numbers rather than getting the local model's patience.
                let limits = tier.limits.resolve(class_of(library, tier));
                let mut built =
                    Tier::with_id(tier.id.clone(), label, model, Arc::new(provider), limits);
                built.on_stuck = tier.on_stuck;
                built.consults_per_turn = tier.consults_per_turn;
                tiers.push(built);
            }
        }
    }

    if tiers.is_empty() {
        return Err(match left_out.is_empty() {
            true => {
                format!("no tiers are configured yet, so prompts have nowhere to go. {NEXT_STEPS}")
            }
            // Naming what was left out, because "no tiers" would be a lie: there
            // are tiers, and the reason each one cannot be used is the thing that
            // has to change.
            false => format!(
                "none of the configured tiers can be used ({}), so no prompt could be answered. \
                 {NEXT_STEPS}",
                left_out.join("; ")
            ),
        });
    }

    Ok(tiers)
}

/// The chain, in order, once the tiers have been built.
pub fn chain(config: &Config, tiers: Vec<Tier>) -> Option<FallbackChain> {
    FallbackChain::new(tiers, config.general.sticky_fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn config(text: &str) -> Config {
        Config::parse(Path::new("test.toml"), text).expect("the test config should be valid")
    }

    #[test]
    fn an_empty_config_has_nothing_to_warn_about() {
        assert!(notes(&Library::embedded(), &Config::default()).is_empty());
    }

    #[test]
    fn an_unset_key_variable_is_reported_by_name() {
        // A name that cannot be set by anything else, so the answer is stable
        // without touching the environment (which tests must not share).
        let config = config(
            r#"
            [[tier]]
            id = "hosted"
            kind = "openai"
            base_url = "https://example.invalid/v1"
            api_key_env = "SPILL_TEST_KEY_THAT_IS_NEVER_SET"
            model = "m"
            "#,
        );

        let notes = notes(&Library::embedded(), &config);

        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("$SPILL_TEST_KEY_THAT_IS_NEVER_SET"),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("hosted"), "{}", notes[0]);
    }

    #[test]
    fn a_set_key_variable_is_not_reported() {
        // PATH is always present, so this exercises the set case without
        // mutating the environment.
        let config = config(
            r#"
            [[tier]]
            id = "hosted"
            kind = "openai"
            base_url = "https://example.invalid/v1"
            api_key_env = "PATH"
            model = "m"
            "#,
        );

        let notes = notes(&Library::embedded(), &config);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_local_server_needs_no_key_warning() {
        let config = config(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            preset = "lmstudio"
            "#,
        );
        assert!(notes(&Library::embedded(), &config).is_empty());
    }

    #[test]
    fn a_missing_agent_cli_is_reported() {
        let config = config(
            r#"
            [[tier]]
            id = "made-up"
            kind = "cli"
            bin = "definitely-not-a-real-cli-xyz"
            args = ["-p", "{prompt}"]
            "#,
        );

        let notes = notes(&Library::embedded(), &config);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("not on PATH"), "{}", notes[0]);
        assert!(
            notes[0].contains("definitely-not-a-real-cli-xyz"),
            "{}",
            notes[0]
        );
    }

    #[test]
    fn an_installed_agent_cli_is_not_reported() {
        let (bin, flag) = crate::provider::cli::portable_shell();
        let config = config(&format!(
            r#"
            [[tier]]
            id = "shell"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]
            "#
        ));
        assert!(notes(&Library::embedded(), &config).is_empty());
    }

    #[test]
    fn unattended_tiers_are_called_out() {
        let config = config(
            r#"
            [[tier]]
            id = "risky"
            name = "Risky"
            kind = "cli"
            preset = "grok"
            approve_all = true
            "#,
        );

        let notes = notes(&Library::embedded(), &config);
        assert!(
            notes
                .iter()
                .any(|note| note.contains("without asking first")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("Risky")),
            "it should name the tier: {notes:?}"
        );
    }

    #[tokio::test]
    async fn building_with_no_tiers_is_an_error_that_says_what_to_do() {
        let error = build(&Library::embedded(), &Config::default(), Path::new("/tmp"))
            .await
            .err()
            .expect("an empty config cannot be built");
        assert!(error.contains("spill setup"), "{error}");
    }

    #[tokio::test]
    async fn a_cli_tier_builds_without_touching_the_network() {
        let (bin, flag) = crate::provider::cli::portable_shell();
        let config = config(&format!(
            r#"
            [[tier]]
            id = "shell"
            name = "A shell"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]
            "#
        ));

        let tiers = build(&Library::embedded(), &config, Path::new("/tmp"))
            .await
            .expect("a cli tier needs nothing but a binary");

        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].label, "A shell");
        assert_eq!(tiers[0].model, "shell", "it stands in for the model name");
    }

    #[tokio::test]
    async fn an_openai_tier_with_an_unreachable_server_names_the_tier() {
        let config = config(
            r#"
            [[tier]]
            id = "dead"
            kind = "openai"
            base_url = "http://127.0.0.1:9/v1"
            model = ""
            "#,
        );

        let error = build(&Library::embedded(), &config, Path::new("/tmp"))
            .await
            .err()
            .expect("discovery cannot succeed against a closed port");
        assert!(error.contains("dead"), "{error}");
    }

    // ---- what is left out, and what is refused ---------------------------

    #[test]
    fn the_guidance_names_the_commands_to_reach_for() {
        // The whole point of these strings: a refusal that does not say what to
        // do next is where a first run turns into an uninstall.
        assert!(NEXT_STEPS.contains("`spill setup`"), "{NEXT_STEPS}");
        assert!(NEXT_STEPS.contains("`spill presets`"), "{NEXT_STEPS}");
        assert!(
            UNREACHABLE_NEXT_STEPS.contains("`spill doctor`"),
            "{UNREACHABLE_NEXT_STEPS}"
        );
        assert!(
            UNREACHABLE_NEXT_STEPS.contains("`spill setup`"),
            "{UNREACHABLE_NEXT_STEPS}"
        );
    }

    #[tokio::test]
    async fn a_cli_that_is_not_installed_is_left_out_rather_than_carried() {
        // A harness that is not installed cannot answer, ever, and a chain that
        // carries it anyway puts a fallback in the rail that never happens. The
        // tier that *is* usable still starts.
        let (bin, flag) = crate::provider::cli::portable_shell();
        let config = config(&format!(
            r#"
            [[tier]]
            id = "good"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]

            [[tier]]
            id = "missing"
            kind = "cli"
            bin = "definitely-not-a-real-cli-xyz"
            args = ["-p", "{{prompt}}"]
            "#
        ));

        let tiers = build(&Library::embedded(), &config, Path::new("/tmp"))
            .await
            .expect("one usable tier is enough to start");

        assert_eq!(tiers.len(), 1, "the missing one was left out");
        assert_eq!(tiers[0].id, "good");
    }

    #[tokio::test]
    async fn a_chain_of_nothing_but_missing_clis_refuses_and_says_what_to_do() {
        // Zero reachable tiers. This is the refusal, and it has to name the tier
        // that went, why, and the two commands that fix it.
        let config = config(
            r#"
            [[tier]]
            id = "missing"
            kind = "cli"
            bin = "definitely-not-a-real-cli-xyz"
            args = ["-p", "{prompt}"]
            "#,
        );

        let error = build(&Library::embedded(), &config, Path::new("/tmp"))
            .await
            .err()
            .expect("nothing can answer");

        assert!(error.contains("missing"), "{error}");
        assert!(error.contains("definitely-not-a-real-cli-xyz"), "{error}");
        assert!(error.contains("not on PATH"), "{error}");
        assert!(error.contains("`spill presets`"), "{error}");
        assert!(error.contains("`spill setup`"), "{error}");
    }

    #[tokio::test]
    async fn a_local_tier_that_is_merely_down_is_not_dropped() {
        // The distinction the drop rule turns on. An endpoint that is not
        // answering is *not* knowably unusable without asking it, and dropping it
        // here would mean starting on the paid tier instead — the opposite of
        // what this program is for. It stays, and the refusal is the honest one.
        let (bin, flag) = crate::provider::cli::portable_shell();
        let config = config(&format!(
            r#"
            [[tier]]
            id = "local"
            kind = "openai"
            base_url = "http://127.0.0.1:9/v1"
            model = ""

            [[tier]]
            id = "fallback"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]
            "#
        ));

        let error = build(&Library::embedded(), &config, Path::new("/tmp"))
            .await
            .err()
            .expect("a local tier with no reachable server and no model to ask for");

        assert!(
            error.contains("local"),
            "it should name the local tier, not quietly move on: {error}"
        );
    }

    #[test]
    fn the_chain_keeps_the_configured_order() {
        let (bin, flag) = crate::provider::cli::portable_shell();
        let config = config(&format!(
            r#"
            [[tier]]
            id = "first"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]

            [[tier]]
            id = "second"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{{prompt}}"]
            "#
        ));

        // Built manually to avoid the async path in a pure test.
        let tiers = vec![
            crate::fallback::Tier::new(
                "first".to_string(),
                "m".to_string(),
                Arc::new(crate::provider::cli::CliProvider::new(
                    "first",
                    cli_spec(&Library::embedded(), &config.tiers[0]).expect("spec"),
                    std::path::PathBuf::from("/tmp"),
                )),
                Default::default(),
            ),
            crate::fallback::Tier::new(
                "second".to_string(),
                "m".to_string(),
                Arc::new(crate::provider::cli::CliProvider::new(
                    "second",
                    cli_spec(&Library::embedded(), &config.tiers[1]).expect("spec"),
                    std::path::PathBuf::from("/tmp"),
                )),
                Default::default(),
            ),
        ];

        let chain = chain(&config, tiers).expect("a chain");
        assert_eq!(chain.labels(), vec!["first", "second"]);
    }
}
