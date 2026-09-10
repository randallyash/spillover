//! Running an agent CLI as a tier.
//!
//! These tiers bring their own agent loop and their own tools, so this is a
//! delegation rather than a chat completion: one prompt in, one stream of output
//! back. The CLI is spawned with an argument vector, never a shell string, so a
//! prompt containing quotes or semicolons is just text.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::provider::dialect::{Dialect, parser_for};
use crate::provider::{ChatRequest, Provider, ProviderError, StreamEvent, TurnSummary};
use crate::session::{ChatMessage, Role};

/// How much of a CLI's stderr to keep for the error message.
const MAX_STDERR: usize = 1_500;

#[derive(Debug, Clone)]
pub struct CliSpec {
    pub bin: String,
    /// Argument templates. `{prompt}`, `{model}` and `{workspace}` are
    /// substituted, each into its own argument.
    pub args: Vec<String>,
    /// Added only when a model is configured.
    pub model_args: Vec<String>,
    pub extra_args: Vec<String>,
    /// Added only when the user has opted in to unattended runs.
    pub approve_args: Vec<String>,
    /// Added after everything else, for CLIs that want the directory spelled out.
    pub workdir_args: Vec<String>,
    pub approve_all: bool,
    pub model: Option<String>,
    pub dialect: Dialect,
}

pub struct CliProvider {
    display: String,
    spec: CliSpec,
    workspace: PathBuf,
}

impl CliProvider {
    pub fn new(display: impl Into<String>, spec: CliSpec, workspace: PathBuf) -> Self {
        Self {
            display: display.into(),
            spec,
            workspace,
        }
    }

    fn build_args(&self, prompt: &str) -> Vec<String> {
        let workspace = self.workspace.display().to_string();
        let model = self.spec.model.as_deref().unwrap_or_default();

        let substitute = |arg: &str| {
            arg.replace("{prompt}", prompt)
                .replace("{workspace}", &workspace)
                .replace("{model}", model)
        };

        let mut args: Vec<String> = self.spec.args.iter().map(|a| substitute(a)).collect();
        if self.spec.model.is_some() {
            args.extend(self.spec.model_args.iter().map(|a| substitute(a)));
        }
        args.extend(self.spec.extra_args.iter().map(|a| substitute(a)));
        if self.spec.approve_all {
            args.extend(self.spec.approve_args.iter().map(|a| substitute(a)));
        }
        args.extend(self.spec.workdir_args.iter().map(|a| substitute(a)));
        args
    }
}

#[async_trait]
impl Provider for CliProvider {
    fn describe(&self) -> String {
        self.display.clone()
    }

    async fn stream(
        &self,
        request: ChatRequest,
        events: UnboundedSender<StreamEvent>,
    ) -> Result<TurnSummary, ProviderError> {
        let bin = self.spec.bin.clone();
        let prompt = render_prompt(&request.messages);
        let args = self.build_args(&prompt);

        let mut child = Command::new(&bin)
            .args(&args)
            .current_dir(&self.workspace)
            // A CLI that decides to prompt would otherwise wait forever.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Abandoning a tier must not leave a process behind.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                let detail = if error.kind() == std::io::ErrorKind::NotFound {
                    format!("{bin} is not on PATH — is it installed, and available to this shell?")
                } else {
                    error.to_string()
                };
                ProviderError::Unreachable {
                    target: bin.clone(),
                    detail,
                }
            })?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Drain stderr at the same time: a chatty CLI would otherwise fill the
        // pipe and block before it ever reached its answer.
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if collected.len() < MAX_STDERR {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
            collected
        });

        let mut parser = parser_for(self.spec.dialect);
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    for event in parser.line(&line) {
                        let _ = events.send(event);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    return Err(ProviderError::Broken {
                        target: bin,
                        detail: error.to_string(),
                    });
                }
            }
        }

        let status = child.wait().await.map_err(|error| ProviderError::Broken {
            target: bin.clone(),
            detail: error.to_string(),
        })?;
        let stderr = stderr_task.await.unwrap_or_default();

        if !status.success() {
            return Err(ProviderError::Rejected {
                target: bin,
                detail: format!("exited with {}{}", describe_status(&status), tail(&stderr)),
            });
        }

        let summary = parser.finish().map_err(|detail| ProviderError::Rejected {
            target: bin.clone(),
            detail,
        })?;

        if summary.text.trim().is_empty() && summary.tool_calls.is_empty() {
            return Err(ProviderError::Rejected {
                target: bin,
                detail: format!("finished without producing an answer{}", tail(&stderr)),
            });
        }

        Ok(summary)
    }
}

/// Whether a command can be found on PATH.
///
/// Used to report a missing agent CLI when a tier is set up or checked, rather
/// than at the moment it is first needed.
pub fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|directory| {
        if directory.join(bin).is_file() {
            return true;
        }
        // Windows resolves executables by extension.
        cfg!(windows)
            && ["exe", "cmd", "bat"]
                .iter()
                .any(|extension| directory.join(format!("{bin}.{extension}")).is_file())
    })
}

/// A CLI takes one prompt, so the conversation is flattened into it.
///
/// These tiers run their own agent loop and their own tools, so what they need
/// is the transcript, not our tool protocol.
fn render_prompt(messages: &[ChatMessage]) -> String {
    let mut out = String::new();

    for message in messages {
        match message.role {
            Role::System => {
                out.push_str(&message.content);
                out.push_str("\n\n");
            }
            Role::User => {
                out.push_str("User: ");
                out.push_str(&message.content);
                out.push_str("\n\n");
            }
            Role::Assistant => {
                out.push_str("Assistant: ");
                out.push_str(&message.content);
                out.push_str("\n\n");
            }
            Role::Tool => {
                out.push_str("Tool result: ");
                out.push_str(&message.content);
                out.push_str("\n\n");
            }
        }
    }

    out.trim_end().to_string()
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "a signal".to_string(),
    }
}

fn tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let shown: String = if trimmed.chars().count() > 400 {
        let tail: String = trimmed
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        trimmed.to_string()
    };
    format!(" — it said: {shown}")
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A tier whose "CLI" is a shell running `body`, with the prompt passed on
    /// as the next argument (so it arrives as `$0`).
    ///
    /// Deliberately not an executable script written to disk: a file that is
    /// written and then exec'd races with any other thread forking, whose child
    /// inherits the still-open write handle and makes the exec fail with
    /// ETXTBSY. Running `sh -c` creates no file at all.
    fn spec(body: &str, dialect: Dialect) -> CliSpec {
        CliSpec {
            bin: "sh".to_string(),
            args: vec!["-c".to_string(), body.to_string(), "{prompt}".to_string()],
            model_args: vec!["-m".to_string(), "{model}".to_string()],
            extra_args: Vec::new(),
            approve_args: vec!["--yolo".to_string()],
            workdir_args: Vec::new(),
            approve_all: false,
            model: None,
            dialect,
        }
    }

    fn provider(spec: CliSpec, workspace: &Path) -> CliProvider {
        CliProvider::new("Fake agent", spec, workspace.to_path_buf())
    }

    fn request(prompt: &str) -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage::system("be brief"), ChatMessage::user(prompt)],
            tools: Vec::new(),
        }
    }

    async fn run(
        provider: &CliProvider,
        prompt: &str,
    ) -> (Result<TurnSummary, ProviderError>, Vec<StreamEvent>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider.stream(request(prompt), tx).await;
        let events = drain(&mut rx);
        (result, events)
    }

    fn drain(rx: &mut UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn text_of(events: &[StreamEvent]) -> String {
        let mut out = String::new();
        for event in events {
            if let StreamEvent::Text(text) = event {
                out.push_str(text);
            }
        }
        out
    }

    #[tokio::test]
    async fn a_plain_cli_becomes_a_tier() {
        let dir = tempfile::tempdir().expect("tempdir");

        let (result, events) = run(
            &provider(
                spec("echo 'hello from the cli'", Dialect::Plain),
                dir.path(),
            ),
            "hi",
        )
        .await;

        let summary = result.expect("the run should succeed");
        assert_eq!(summary.text, "hello from the cli");
        assert_eq!(text_of(&events), "hello from the cli\n");
    }

    #[tokio::test]
    async fn a_grok_shaped_cli_reports_text_and_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"printf '%s\n' '{"type":"text","data":"answer "}' '{"type":"text","data":"here"}' '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":12,"output_tokens":3}}'"#;

        let (result, _) = run(&provider(spec(body, Dialect::Grok), dir.path()), "hi").await;
        let summary = result.expect("the run should succeed");

        assert_eq!(summary.text, "answer here");
        assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(summary.usage.expect("usage").prompt_tokens, 12);
    }

    #[tokio::test]
    async fn a_command_code_shaped_cli_reports_its_result_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"printf '%s\n' '{"type":"event","event":{"type":"tool_running","toolName":"read_file"}}' '{"type":"result","subtype":"success","stopReason":"end_turn","finalText":"all done"}'"#;

        let (result, events) = run(
            &provider(spec(body, Dialect::CommandCode), dir.path()),
            "hi",
        )
        .await;
        let summary = result.expect("the run should succeed");

        assert_eq!(summary.text, "all done");
        // The progress frame is forwarded, and no raw JSON reaches the answer.
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Activity)));
        assert_eq!(text_of(&events), "");
    }

    #[tokio::test]
    async fn a_failing_cli_reports_its_exit_status_and_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "echo 'you are not signed in' >&2; exit 3";

        let (result, _) = run(&provider(spec(body, Dialect::Plain), dir.path()), "hi").await;
        let error = result.expect_err("a non-zero exit must fail the tier");

        let message = error.to_string();
        assert!(message.contains("exit status 3"), "{message}");
        assert!(message.contains("you are not signed in"), "{message}");
    }

    #[tokio::test]
    async fn a_cli_that_says_nothing_is_reported_rather_than_passed_off_as_an_answer() {
        let dir = tempfile::tempdir().expect("tempdir");

        let (result, _) = run(&provider(spec("exit 0", Dialect::Plain), dir.path()), "hi").await;
        let error = result.expect_err("silence is not an answer");
        assert!(
            error.to_string().contains("without producing an answer"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_cli_that_reports_an_error_frame_fails_the_tier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"printf '%s\n' '{"type":"error","message":"no credentials"}'; exit 0"#;

        let (result, _) = run(&provider(spec(body, Dialect::Grok), dir.path()), "hi").await;
        let error = result.expect_err("an error frame must fail the tier");
        assert!(error.to_string().contains("no credentials"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_binary_says_so_instead_of_failing_obscurely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut spec = spec("echo unused", Dialect::Plain);
        spec.bin = "definitely-not-a-real-cli-xyz".to_string();

        let (result, _) = run(&provider(spec, dir.path()), "hi").await;
        let error = result.expect_err("a missing binary must fail the tier");
        assert!(error.to_string().contains("not on PATH"), "{error}");
    }

    #[tokio::test]
    async fn the_prompt_arrives_as_a_single_argument_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        // With `sh -c body prompt`, the prompt arrives as $0.
        let body = r#"printf '%s' "$0" > seen.txt; echo ok"#;

        let awkward = "quotes \" and ' and $(rm -rf /) and ; semicolons\nand a newline";
        let (result, _) = run(&provider(spec(body, Dialect::Plain), dir.path()), awkward).await;
        result.expect("the run should succeed");

        let seen = std::fs::read_to_string(dir.path().join("seen.txt")).expect("prompt file");
        assert!(seen.contains(awkward), "prompt was altered:\n{seen}");
        assert!(
            seen.starts_with("be brief"),
            "the system prompt should lead:\n{seen}"
        );
        assert!(
            seen.contains("User: "),
            "the turn should be labelled:\n{seen}"
        );
    }

    #[tokio::test]
    async fn the_cli_runs_in_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");

        let (result, _) = run(&provider(spec("pwd", Dialect::Plain), dir.path()), "hi").await;
        let summary = result.expect("the run should succeed");
        // macOS reports /private/var for a temp dir, so compare the file name.
        assert!(
            summary.text.contains(
                dir.path()
                    .file_name()
                    .expect("temp dir name")
                    .to_str()
                    .expect("utf8")
            ),
            "ran in the wrong directory: {}",
            summary.text
        );
    }

    #[test]
    fn model_arguments_are_added_only_when_a_model_is_set() {
        let workspace = PathBuf::from("/tmp");
        let mut spec = CliSpec {
            bin: "grok".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            model_args: vec!["-m".to_string(), "{model}".to_string()],
            extra_args: vec!["--output-format".to_string(), "streaming-json".to_string()],
            approve_args: vec!["--always-approve".to_string()],
            workdir_args: vec!["--cwd".to_string(), "{workspace}".to_string()],
            approve_all: false,
            model: None,
            dialect: Dialect::Grok,
        };

        let provider = CliProvider::new("grok", spec.clone(), workspace.clone());
        let args = provider.build_args("hello");
        assert_eq!(
            args,
            vec![
                "-p",
                "hello",
                "--output-format",
                "streaming-json",
                "--cwd",
                "/tmp"
            ]
        );

        spec.model = Some("grok-4.6".to_string());
        let provider = CliProvider::new("grok", spec.clone(), workspace.clone());
        let args = provider.build_args("hello");
        assert_eq!(
            args,
            vec![
                "-p",
                "hello",
                "-m",
                "grok-4.6",
                "--output-format",
                "streaming-json",
                "--cwd",
                "/tmp"
            ]
        );
    }

    #[test]
    fn unattended_flags_are_added_only_when_opted_in() {
        let workspace = PathBuf::from("/tmp");
        let spec = CliSpec {
            bin: "cmd".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            model_args: Vec::new(),
            extra_args: vec!["--output-format".to_string(), "json".to_string()],
            approve_args: vec!["--yolo".to_string()],
            workdir_args: Vec::new(),
            approve_all: true,
            model: None,
            dialect: Dialect::CommandCode,
        };

        let args = CliProvider::new("cmd", spec, workspace).build_args("hi");
        assert!(args.contains(&"--yolo".to_string()), "{args:?}");
    }

    #[test]
    fn a_prompt_is_never_run_through_a_shell() {
        let workspace = PathBuf::from("/tmp");
        let spec = CliSpec {
            bin: "cmd".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            model_args: Vec::new(),
            extra_args: Vec::new(),
            approve_args: Vec::new(),
            workdir_args: Vec::new(),
            approve_all: false,
            model: None,
            dialect: Dialect::Plain,
        };

        let args = CliProvider::new("cmd", spec, workspace).build_args("a; rm -rf /tmp/x");
        // The dangerous text stays inside one argument.
        assert_eq!(args, vec!["-p", "a; rm -rf /tmp/x"]);
    }
}
