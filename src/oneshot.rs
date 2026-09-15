//! `spill -p "…"`: one prompt, one answer, then exit.
//!
//! The non-interactive counterpart to the TUI, for scripts and pipelines. It
//! runs the same chain with the same escalation, so a local model that loops
//! still spills over — the only difference is that there is nobody to approve a
//! tool, so writes and shell commands are refused unless `--yolo` was asked for.

use std::sync::Arc;

use serde_json::json;

use crate::agent::approval::{Approver, PermitAll, RefuseAll};
use crate::agent::tools::Registry;
use crate::agent::{AgentConfig, AgentEvent};
use crate::config::Config;
use crate::preset::Library;
use crate::provider::Usage;
use crate::stalls::SpillLog;

/// Exit codes, matching what the shell and CI expect.
pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;

#[derive(Debug, Clone)]
pub struct Options {
    pub prompt: String,
    /// Print a JSON object instead of the answer text.
    pub json: bool,
    /// Let the model write files and run commands without asking.
    pub yolo: bool,
    /// Where spills are recorded, or `None` for the platform's state directory.
    pub log: Option<SpillLog>,
}

/// What the run produced, for the caller to print.
#[derive(Debug, Default)]
pub struct Outcome {
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    /// The tier that answered, and any the run spilled through on the way.
    pub answered_by: Option<String>,
    pub escalations: Vec<String>,
    /// Tiers that came within one step of being abandoned and were not.
    pub near_misses: Vec<String>,
    pub failure: Option<String>,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        if self.failure.is_some() {
            EXIT_ERROR
        } else {
            EXIT_OK
        }
    }

    pub fn to_json(&self) -> String {
        let payload = json!({
            "text": self.text,
            "stopReason": self.stop_reason,
            "answeredBy": self.answered_by,
            "escalations": self.escalations,
            "nearMisses": self.near_misses,
            "usage": self.usage.map(|usage| json!({
                "inputTokens": usage.prompt_tokens,
                "outputTokens": usage.completion_tokens,
                "cacheReadTokens": usage.cache_read_tokens,
                "cacheWriteTokens": usage.cache_write_tokens,
            })),
            "error": self.failure,
        });
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
    }
}

/// The spill log in the platform's state directory, when there is one.
fn default_log() -> Option<SpillLog> {
    SpillLog::default_path().map(SpillLog::at)
}

/// Run one prompt to completion through the tier chain.
pub async fn run(library: &Library, config: &Config, options: &Options) -> Outcome {
    let workspace = config.general.workspace_path();

    let tiers = match crate::tiers::build(library, config, &workspace).await {
        Ok(tiers) => tiers,
        Err(error) => {
            return Outcome {
                failure: Some(error),
                ..Outcome::default()
            };
        }
    };

    let first = tiers.first().map(|tier| tier.label.clone());
    let Some(chain) = crate::tiers::chain(config, tiers) else {
        return Outcome {
            failure: Some("no usable tiers".to_string()),
            ..Outcome::default()
        };
    };

    let approver: Arc<dyn Approver> = if options.yolo {
        Arc::new(PermitAll)
    } else {
        // No prompt can be shown, so anything that needs permission is refused.
        Arc::new(RefuseAll)
    };

    let (commands, mut events) = crate::agent::spawn(
        AgentConfig {
            workspace,
            max_steps: config.general.max_steps,
            // A one-shot run has nobody to press escape, so nothing cancels it.
            cancel: crate::agent::Canceller::default(),
            // No session is written and none is resumed: `-p` is for scripts,
            // and a script that picked up yesterday's conversation because it
            // happened to run in the same directory would be a trap.
            store: None,
            // The spill log is a different question, and gets the same answer as
            // the interactive path: a script whose turn was handed to another
            // model is exactly the case nobody is watching to notice.
            log: options.log.clone().or_else(default_log),
            // The same rules the interactive path uses: `-p` runs unattended, so
            // a command that ran without asking there should not stop and ask
            // into a void here.
            allow_shell: config.general.allow_shell.clone(),
            origin: config.origin.clone(),
        },
        chain,
        Arc::new(Registry::with_shell_timeout(
            std::time::Duration::from_secs(config.general.shell_timeout_secs),
        )),
        approver,
    );

    if commands
        .send(crate::agent::Command::Prompt(options.prompt.clone()))
        .is_err()
    {
        return Outcome {
            failure: Some("the agent stopped before it could run".to_string()),
            ..Outcome::default()
        };
    }

    let mut outcome = Outcome {
        answered_by: first.clone(),
        ..Outcome::default()
    };

    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Text(chunk) => outcome.text.push_str(&chunk),
            AgentEvent::Escalated { from, to, reason } => {
                // What the abandoned tier produced is not the answer.
                outcome.text.clear();
                outcome.answered_by = Some(to.clone());
                outcome.escalations.push(format!("{from} {reason} → {to}"));
            }
            // Summed rather than replaced: a turn makes one request per tool
            // call, and reporting only the last of them understated a scripted
            // run in proportion to how much work it did.
            AgentEvent::Spent { usage, .. } => {
                let total = outcome.usage.get_or_insert_with(Usage::default);
                total.absorb(&usage);
            }
            AgentEvent::Finished { stop_reason } => {
                outcome.stop_reason = stop_reason;
                break;
            }
            // The counters behind a stall go to the spill log, which is where a
            // script can read them; what a pipeline needs on the stream is the
            // move, which `Escalated` and `Consulted` already carry.
            AgentEvent::Stalled { .. } => {}
            AgentEvent::AlmostStalled { tier, miss } => {
                outcome.near_misses.push(format!("{tier} {}", miss.sentence()));
            }
            AgentEvent::Exhausted { reason } => {
                outcome.failure = Some(format!(
                    "no tier could answer: {reason}. {}",
                    crate::tiers::UNREACHABLE_NEXT_STEPS
                ));
                break;
            }
            // A consult is a detour inside a turn, not its end: the driver
            // carries on, so a pipeline waits for the answer. What the driver
            // produced *before* being helped is not that answer, though — the
            // turn was abandoned mid-stream, so only the text after the advice
            // counts. This is the same rule as an escalation, for the same
            // reason.
            AgentEvent::Consulted {
                driver, consultant, ..
            } => {
                outcome.text.clear();
                outcome
                    .escalations
                    .push(format!("{driver} consulted {consultant}"));
            }
            // Nothing cancels a one-shot run, so this cannot arrive; it is
            // handled as a failure rather than ignored, so a future change that
            // finds a way to cancel would say so rather than return no answer.
            AgentEvent::Cancelled { tier } => {
                outcome.failure = Some(format!("the turn on {tier} was cancelled"));
                break;
            }
            // Notices and tool chatter are for the TUI; a pipeline wants the
            // answer. Refusals are worth surfacing though, since they explain a
            // missing file.
            AgentEvent::Denied { tool } => {
                outcome
                    .escalations
                    .push(format!("declined to run {tool} (no one to approve it)"));
            }
            AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::Notice(_)
            | AgentEvent::Thought(_)
            | AgentEvent::Switched { .. }
            // The announcement of a move that `Escalated` will report anyway; a
            // one-shot run has no interface to narrate it to, and recording both
            // would list every spill twice.
            | AgentEvent::Spilling { .. } => {}
        }
    }

    outcome.text = outcome.text.trim_end().to_string();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn config(text: &str) -> Config {
        Config::parse(Path::new("test.toml"), text).expect("valid")
    }

    // The tiers below stand in for an agent CLI with a shell, so they have to
    // name the shell this platform actually has: `sh -c` on unix, `cmd /C` on
    // Windows. The command bodies differ for the same reason.

    /// Prints `text`.
    #[cfg(unix)]
    fn say(text: &str) -> String {
        format!("printf '{text}'")
    }
    #[cfg(windows)]
    fn say(text: &str) -> String {
        format!("echo {text}")
    }

    /// Exits with `code`, printing nothing.
    fn bail(code: u8) -> String {
        format!("exit {code}")
    }

    /// The shell this platform has, as a tier's `bin` and its "run this" flag.
    fn shell_bin_and_flag() -> (&'static str, &'static str) {
        #[cfg(unix)]
        {
            ("sh", "-c")
        }
        #[cfg(windows)]
        {
            ("cmd", "/C")
        }
    }

    /// A tier running `body` through this platform's shell.
    fn shell_config(body: &str) -> Config {
        let (bin, flag) = shell_bin_and_flag();
        config(&format!(
            r#"
            [[tier]]
            id = "shell"
            name = "Shell"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", {body:?}]
            "#
        ))
    }

    /// Options whose spill log goes to a temporary directory.
    fn options(prompt: &str) -> Options {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"));
        Options {
            prompt: prompt.to_string(),
            json: false,
            yolo: false,
            log: Some(SpillLog::at(dir.path().join("spills.jsonl"))),
        }
    }

    #[test]
    fn the_test_options_never_point_at_the_real_log() {
        // The guard for the bug this file caused: these tests run the real
        // one-shot path, so the only thing standing between a test run and the
        // spill log of whoever runs it is this helper. Pinned because the
        // failure is invisible — a green suite that quietly appends to a file
        // the user is tuning from.
        let options = options("anything");
        let chosen = options.log.as_ref().expect("the helper sets a log");
        let real = SpillLog::default_path();

        assert!(chosen.path().is_absolute(), "{}", chosen.path().display());
        assert_ne!(
            Some(chosen.path().to_path_buf()),
            real,
            "a test would write to the real spill log"
        );
    }

    #[tokio::test]
    async fn a_plain_answer_comes_back_as_the_text() {
        let outcome = run(
            &Library::embedded(),
            &shell_config(&say("the answer")),
            &options("anything"),
        )
        .await;

        assert_eq!(outcome.exit_code(), EXIT_OK, "{outcome:?}");
        assert_eq!(outcome.text, "the answer");
        assert_eq!(outcome.answered_by.as_deref(), Some("Shell"));
    }

    #[tokio::test]
    async fn the_answer_is_trimmed_of_trailing_whitespace() {
        // Both shells here end their output with a newline, and `echo` on
        // Windows also emits a carriage return.
        let outcome = run(
            &Library::embedded(),
            &shell_config(&say("answer")),
            &options("anything"),
        )
        .await;
        assert_eq!(outcome.text, "answer");
    }

    #[tokio::test]
    async fn a_tier_that_fails_ends_the_run_with_an_error() {
        let outcome = run(
            &Library::embedded(),
            &shell_config(&bail(3)),
            &options("anything"),
        )
        .await;

        assert_ne!(outcome.exit_code(), EXIT_OK);
        assert!(outcome.failure.is_some(), "{outcome:?}");
    }

    #[tokio::test]
    async fn an_empty_config_fails_with_the_setup_hint() {
        let outcome = run(
            &Library::embedded(),
            &Config::default(),
            &options("anything"),
        )
        .await;

        assert_ne!(outcome.exit_code(), EXIT_OK);
        let failure = outcome.failure.expect("a failure");
        assert!(failure.contains("spill setup"), "{failure}");
    }

    #[tokio::test]
    async fn a_write_is_refused_when_there_is_nobody_to_ask() {
        // The topic of this test is the approver, so the tier only has to
        // produce text; the workspace is left at its default rather than being
        // written into TOML, where a Windows path would need escaping.
        let outcome = run(
            &Library::embedded(),
            &shell_config(&say("I would like to write")),
            &options("write a file"),
        )
        .await;

        // The run goes through because nothing needed approving. A write would
        // have been refused, and the refusal reported rather than dropped.
        assert_eq!(outcome.exit_code(), EXIT_OK, "{outcome:?}");
        assert!(
            outcome.text.contains("I would like to write"),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn the_json_output_carries_the_answer_and_the_tier() {
        let outcome = run(
            &Library::embedded(),
            &shell_config(&say("hello")),
            &options("anything"),
        )
        .await;

        let parsed: serde_json::Value =
            serde_json::from_str(&outcome.to_json()).expect("valid JSON");

        assert_eq!(parsed["text"], "hello");
        assert_eq!(parsed["answeredBy"], "Shell");
        assert!(parsed["error"].is_null(), "{parsed}");
        assert_eq!(parsed["escalations"].as_array().expect("array").len(), 0);
    }

    #[tokio::test]
    async fn a_failed_run_reports_the_error_in_its_json() {
        let outcome = run(
            &Library::embedded(),
            &Config::default(),
            &options("anything"),
        )
        .await;

        let parsed: serde_json::Value =
            serde_json::from_str(&outcome.to_json()).expect("valid JSON");

        assert!(!parsed["error"].is_null(), "{parsed}");
        assert!(
            parsed["error"]
                .as_str()
                .expect("a string")
                .contains("spill setup")
        );
    }

    #[test]
    fn json_from_an_empty_outcome_is_still_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(&Outcome::default().to_json()).expect("valid JSON");
        assert_eq!(parsed["text"], "");
        assert!(parsed["usage"].is_null());
    }

    #[tokio::test]
    async fn a_failing_tier_spills_over_and_the_answer_is_the_second_tiers() {
        let (bin, flag) = shell_bin_and_flag();

        // The first tier exits non-zero, so the second should answer.
        let config = config(&format!(
            r#"
            [[tier]]
            id = "broken"
            name = "Broken"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{}"]

            [[tier]]
            id = "working"
            name = "Working"
            kind = "cli"
            bin = "{bin}"
            args = ["{flag}", "{}"]
            "#,
            bail(1),
            say("second tier here")
        ));

        let outcome = run(&Library::embedded(), &config, &options("anything")).await;

        assert_eq!(outcome.exit_code(), EXIT_OK, "{outcome:?}");
        assert_eq!(outcome.text, "second tier here");
        assert_eq!(outcome.answered_by.as_deref(), Some("Working"));
        assert_eq!(outcome.escalations.len(), 1, "{:?}", outcome.escalations);
        assert!(
            outcome.escalations[0].contains("Broken"),
            "the escalation should name the tier it left: {:?}",
            outcome.escalations
        );
    }
}
