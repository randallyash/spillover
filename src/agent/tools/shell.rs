//! `run_shell` — run a shell command in the workspace.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, cap, object_schema};
use crate::config::{DEFAULT_SHELL_TIMEOUT_SECS, MAX_SHELL_TIMEOUT_SECS};

/// The tool's name, as the model sees it.
pub const NAME: &str = "run_shell";

pub struct RunShell {
    /// How long a command may run when the call does not name a timeout of its
    /// own. Comes from `[general] shell_timeout_secs`.
    timeout: Duration,
}

impl RunShell {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for RunShell {
    fn default() -> Self {
        Self::new(Duration::from_secs(DEFAULT_SHELL_TIMEOUT_SECS))
    }
}

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        NAME
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the workspace directory and return its output and exit status. \
         Use this to build, test, and inspect the project. Long-running or interactive commands \
         are killed when they hit the configured timeout (300 seconds unless \
         [general] shell_timeout_secs says otherwise). Pass timeout_secs to allow a longer run \
         of this one command, up to 1800 seconds."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "command": {
                    "type": "string",
                    "description": "The command line to run."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "How many seconds this command may run before it is killed. \
                                    Defaults to the configured shell timeout. Maximum 1800."
                }
            }),
            &["command"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn preview(&self, arguments: &Value, workspace: &Path) -> String {
        let args = Args::new(arguments);
        match args.required_str("command") {
            Ok(command) => match timeout_secs(args, self.timeout) {
                Ok(seconds) => {
                    let mut preview = format!("run in {}:\n  {command}", workspace.display());
                    if seconds != self.timeout.as_secs() {
                        preview.push_str(&format!("\n  (timeout {seconds}s)"));
                    }
                    preview
                }
                Err(error) => error.content,
            },
            Err(error) => error.content,
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let args = Args::new(arguments);
        let command = match args.required_str("command") {
            Ok(command) => command,
            Err(error) => return error,
        };
        let seconds = match timeout_secs(args, self.timeout) {
            Ok(seconds) => seconds,
            Err(error) => return error,
        };
        let timeout = Duration::from_secs(seconds);

        let mut process = platform_shell(command);
        process
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Killing on drop means a timeout tears down the child rather than
            // leaving it running detached.
            .kill_on_drop(true);

        let output = match tokio::time::timeout(timeout, process.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return ToolOutcome::io(format!("could not run {command:?}"), &error);
            }
            Err(_) => {
                return ToolOutcome::error(format!(
                    "{command:?} did not finish within {seconds}s and was killed"
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let failed = !output.status.success();

        let mut report = match output.status.code() {
            Some(0) => "exit status 0\n".to_string(),
            Some(code) => format!("exit status {code}\n"),
            None => "terminated by a signal\n".to_string(),
        };

        if stdout.trim().is_empty() && stderr.trim().is_empty() {
            report.push_str("(no output)");
        }
        if !stdout.trim().is_empty() {
            report.push_str("stdout:\n");
            report.push_str(stdout.trim_end());
            report.push('\n');
        }
        if !stderr.trim().is_empty() {
            report.push_str("stderr:\n");
            report.push_str(stderr.trim_end());
            report.push('\n');
        }

        let report = cap(report);
        if failed {
            ToolOutcome::error(report)
        } else {
            ToolOutcome::ok(report)
        }
    }
}

fn timeout_secs(args: Args<'_>, fallback: Duration) -> Result<u64, ToolOutcome> {
    match args.optional_usize("timeout_secs") {
        None => Ok(fallback.as_secs()),
        Some(0) => Err(ToolOutcome::error(
            "timeout_secs must be at least 1; omit it to use the configured default",
        )),
        Some(seconds) if seconds as u64 > MAX_SHELL_TIMEOUT_SECS => Err(ToolOutcome::error(
            format!("timeout_secs is capped at {MAX_SHELL_TIMEOUT_SECS}s; pass a smaller value"),
        )),
        Some(seconds) => Ok(seconds as u64),
    }
}

#[cfg(unix)]
fn platform_shell(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn platform_shell(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tool runs `sh -c` on unix and `cmd /C` on Windows, so the test
    // commands have to be written for whichever shell is actually there.

    /// Listed in the workspace, so the test knows the command ran there.
    #[cfg(unix)]
    const LIST: &str = "ls";
    #[cfg(windows)]
    const LIST: &str = "dir /b";

    /// Writes to stderr and exits 3.
    #[cfg(unix)]
    const FAIL_WITH_MESSAGE: &str = "echo oops >&2; exit 3";
    #[cfg(windows)]
    const FAIL_WITH_MESSAGE: &str = "echo oops 1>&2 & exit /b 3";

    /// Succeeds and prints nothing.
    #[cfg(unix)]
    const QUIET_SUCCESS: &str = "true";
    #[cfg(windows)]
    const QUIET_SUCCESS: &str = "exit 0";

    #[tokio::test]
    async fn runs_a_command_and_returns_its_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(&json!({"command": "echo hello"}), dir.path())
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("hello"), "{}", outcome.content);
        assert!(
            outcome.content.contains("exit status 0"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn runs_in_the_workspace_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker.txt"), "x").expect("write");
        let outcome = RunShell::default()
            .run(&json!({"command": LIST}), dir.path())
            .await;
        assert!(
            outcome.content.contains("marker.txt"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_failing_command_is_a_tool_error_with_its_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(&json!({"command": FAIL_WITH_MESSAGE}), dir.path())
            .await;
        assert!(outcome.is_error, "{}", outcome.content);
        assert!(
            outcome.content.contains("exit status 3"),
            "{}",
            outcome.content
        );
        assert!(outcome.content.contains("oops"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn an_unknown_command_does_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(
                &json!({"command": "definitely-not-a-real-command-xyz"}),
                dir.path(),
            )
            .await;
        assert!(outcome.is_error, "{}", outcome.content);
    }

    #[tokio::test]
    async fn a_command_with_no_output_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(&json!({"command": QUIET_SUCCESS}), dir.path())
            .await;
        assert!(!outcome.is_error);
        assert!(
            outcome.content.contains("(no output)"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_missing_command_argument_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default().run(&json!({}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("command"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn the_preview_shows_the_command_and_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preview = RunShell::default()
            .preview(&json!({"command": "cargo test"}), dir.path())
            .await;
        assert!(preview.contains("cargo test"), "{preview}");
        assert!(
            preview.contains(&dir.path().display().to_string()),
            "{preview}"
        );
    }

    #[tokio::test]
    async fn a_shell_command_offers_nothing_to_undo() {
        // A command can do anything — write a file, delete a tree, talk to the
        // network. There is no honest way to reverse that, so it offers nothing
        // rather than a partial reversal that would be worse than none.
        let dir = tempfile::tempdir().expect("tempdir");

        // A bare command, not a shell invocation: the tool already wraps what it
        // is given in the platform's own shell. Naming `sh` here put
        // `'echo hello'` in front of `cmd`, whose single quotes are not quoting
        // at all, so the command failed and the test proved the opposite of its
        // point.
        let outcome = RunShell::default()
            .run(&json!({"command": "echo hello"}), dir.path())
            .await;

        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("hello"), "{}", outcome.content);
        assert!(
            outcome.undo.is_none(),
            "a shell command must never claim to be reversible"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_overruns_its_timeout_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(
                &json!({"command": "sleep 5", "timeout_secs": 1}),
                dir.path(),
            )
            .await;
        assert!(outcome.is_error, "{}", outcome.content);
        assert!(
            outcome.content.contains("did not finish within 1s"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_zero_timeout_is_refused_rather_than_killing_immediately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(
                &json!({"command": "echo hi", "timeout_secs": 0}),
                dir.path(),
            )
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("at least 1"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_timeout_above_the_cap_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell::default()
            .run(
                &json!({"command": "echo hi", "timeout_secs": 10_000}),
                dir.path(),
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("capped"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn the_preview_names_a_timeout_the_call_chose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preview = RunShell::default()
            .preview(
                &json!({"command": "cargo test", "timeout_secs": 600}),
                dir.path(),
            )
            .await;
        assert!(preview.contains("timeout 600s"), "{preview}");
    }
}
