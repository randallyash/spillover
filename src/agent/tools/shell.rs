//! `run_shell` — run a shell command in the workspace.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, cap, object_schema};

/// Commands that never finish would otherwise wedge the session.
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the workspace directory and return its output and exit status. \
         Use this to build, test, and inspect the project. Long-running or interactive commands \
         will be killed when they time out."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "command": {
                    "type": "string",
                    "description": "The command line to run."
                }
            }),
            &["command"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn preview(&self, arguments: &Value, workspace: &Path) -> String {
        match Args::new(arguments).required_str("command") {
            Ok(command) => format!("run in {}:\n  {command}", workspace.display()),
            Err(error) => error.content,
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let command = match Args::new(arguments).required_str("command") {
            Ok(command) => command,
            Err(error) => return error,
        };

        let mut process = platform_shell(command);
        process
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Killing on drop means a timeout tears down the child rather than
            // leaving it running detached.
            .kill_on_drop(true);

        let output = match tokio::time::timeout(TIMEOUT, process.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return ToolOutcome::error(format!("could not run {command:?}: {error}"));
            }
            Err(_) => {
                return ToolOutcome::error(format!(
                    "{command:?} did not finish within {}s and was killed",
                    TIMEOUT.as_secs()
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

    #[tokio::test]
    async fn runs_a_command_and_returns_its_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell
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
        let outcome = RunShell.run(&json!({"command": "ls"}), dir.path()).await;
        assert!(
            outcome.content.contains("marker.txt"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_failing_command_is_a_tool_error_with_its_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = RunShell
            .run(&json!({"command": "echo oops >&2; exit 3"}), dir.path())
            .await;
        assert!(outcome.is_error);
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
        let outcome = RunShell
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
        let outcome = RunShell.run(&json!({"command": "true"}), dir.path()).await;
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
        let outcome = RunShell.run(&json!({}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("command"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn the_preview_shows_the_command_and_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preview = RunShell
            .preview(&json!({"command": "cargo test"}), dir.path())
            .await;
        assert!(preview.contains("cargo test"), "{preview}");
        assert!(
            preview.contains(&dir.path().display().to_string()),
            "{preview}"
        );
    }
}
