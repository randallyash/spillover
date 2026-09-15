//! Starting a delegated CLI, with an environment it can trust.
//!
//! A `cli` tier hands the turn to a harness that signs in for itself. That is the
//! whole point of the kind, and what the presets advertise — "uses your SuperGrok
//! login", "uses your Command Code plan". Several of those harnesses will prefer
//! a model credential from the environment over the login they already hold, so a
//! key exported for another purpose silently changes whose account is billed.
//!
//! This is not hypothetical, because the keys most likely to be present are the
//! ones spill asks for itself. The `xai` endpoint preset reads `XAI_API_KEY`;
//! a user with that configured and a `grok` CLI tier beside it has exported
//! exactly the variable that would redirect the CLI away from SuperGrok.
//!
//! So the rule is: spill's environment is for spill's own tiers. A delegated CLI
//! signs in on its own and does not inherit a model credential from here.
//!
//! Deliberately not applied to `run_shell`, which runs the user's own command in
//! the user's own workspace and should see exactly what their shell would.

use tokio::process::Command;

#[cfg(test)]
use std::ffi::OsStr;

/// Model credentials that would redirect a delegated CLI's billing.
pub const MODEL_CREDENTIALS: &[&str] = &[
    // `claude`
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    // `codex`, `opencode`, `crush`
    "OPENAI_API_KEY",
    // `grok`
    "XAI_API_KEY",
    "GROK_API_KEY",
    // `gemini`
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    // Read by spill's own endpoint presets, so likely to be exported already.
    "OPENROUTER_API_KEY",
    "DEEPSEEK_API_KEY",
];

/// A command for a delegated CLI: the binary, with the model credentials taken
/// out of the environment it will inherit.
pub fn delegated_cli(bin: &str) -> Command {
    let mut command = Command::new(bin);
    for name in MODEL_CREDENTIALS {
        command.env_remove(name);
    }
    command
}

/// Which model credentials are set in this process's environment.
pub fn inherited_credentials() -> Vec<&'static str> {
    MODEL_CREDENTIALS
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        .collect()
}

/// Whether a name is one this module removes.
#[cfg(test)]
pub fn is_credential(name: impl AsRef<OsStr>) -> bool {
    let name = name.as_ref();
    MODEL_CREDENTIALS
        .iter()
        .any(|known| OsStr::new(known) == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegated_cli_has_every_model_credential_removed() {
        // Asserted through the command's own record of what it will do to the
        // environment, rather than by mutating this process's — tests must not
        // mutate the environment they share.
        let command = delegated_cli("some-agent");
        let removals: Vec<&OsStr> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name)
            .collect();

        for expected in MODEL_CREDENTIALS {
            assert!(
                removals.iter().any(|name| *name == OsStr::new(expected)),
                "{expected} would reach a delegated CLI"
            );
        }
        assert_eq!(
            removals.len(),
            MODEL_CREDENTIALS.len(),
            "the command should remove exactly the documented list"
        );
    }

    #[test]
    fn nothing_else_about_the_environment_is_touched() {
        // Only removals are recorded, so everything else is inherited. A
        // `env_clear` would show up here as no removals plus a cleared env.
        let command = delegated_cli("some-agent");
        let sets: Vec<&OsStr> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_some())
            .map(|(name, _)| name)
            .collect();

        assert!(
            sets.is_empty(),
            "spill should not set variables for a child, only remove them: {sets:?}"
        );
    }

    #[test]
    fn the_binary_is_the_one_asked_for() {
        let command = delegated_cli("cmd");
        assert_eq!(command.as_std().get_program(), OsStr::new("cmd"));
    }

    #[test]
    fn the_list_names_the_providers_the_shipped_presets_use() {
        // Each of these is the credential for a CLI in `presets.toml`, so the
        // list can be checked against that file rather than taken on faith.
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "XAI_API_KEY",
            "GEMINI_API_KEY",
        ] {
            assert!(is_credential(name), "{name} should be scrubbed");
        }

        // A token that is not a model credential is left alone: a GitHub token
        // is used for far more than a model account, and removing it would break
        // legitimate work.
        assert!(!is_credential("GH_TOKEN"));
        assert!(!is_credential("PATH"));
        assert!(!is_credential("HF_TOKEN"));
    }

    #[test]
    fn the_list_has_no_duplicates() {
        let mut sorted: Vec<&str> = MODEL_CREDENTIALS.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "a credential is listed twice");
    }

    #[test]
    fn reporting_finds_only_names_from_the_list() {
        // Whatever is set in the environment being tested, everything reported
        // must be one of ours, or the note would name a variable spill does not
        // remove.
        for name in inherited_credentials() {
            assert!(is_credential(name), "{name} is not one of ours");
        }
    }

    #[tokio::test]
    async fn a_real_child_still_inherits_the_rest_of_the_environment() {
        // The strongest available check that only the listed variables go: a
        // real process is started and asked for something every environment has.
        //
        // A true end-to-end check of the removal would need the variable set in
        // this process, which tests must not do — they share an environment. The
        // recorded removals above are what stand in for it.
        #[cfg(unix)]
        {
            let mut command = delegated_cli("sh");
            command.arg("-c").arg("printf '%s' \"$PATH\"");
            let output = command.output().await.expect("the child should run");
            let path = String::from_utf8_lossy(&output.stdout);

            assert!(
                !path.trim().is_empty(),
                "PATH did not reach the child, so the environment was cleared"
            );
        }
    }
}
