//! Running an agent CLI as a tier.
//!
//! These tiers bring their own agent loop and their own tools, so this is a
//! delegation rather than a chat completion: one prompt in, one stream of output
//! back. The CLI is spawned with an argument vector, never a shell string, so a
//! prompt containing quotes or semicolons is just text.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::provider::dialect::{Dialect, parser_for};
use crate::provider::{ChatRequest, Provider, ProviderError, StreamEvent, TurnSummary};
use crate::session::{ChatMessage, Role};

/// How much of a CLI's stderr to keep for the error message.
const MAX_STDERR: usize = 1_500;

/// The hard instruction that leads a consult's prompt.
const CONSULT_GUARD: &str = "You are being consulted, not asked to do the work. This run is \
     read-only: your tools are disabled, so do not read, write, edit, search, or run anything, \
     and do not ask to. Answer the question below in prose, in a single reply, using only what \
     it already tells you.";

#[derive(Debug, Clone)]
pub struct CliSpec {
    pub bin: String,
    /// Argument templates. `{prompt}`, `{model}`, `{workspace}` and `{session}`
    /// are substituted, each into its own argument.
    pub args: Vec<String>,
    /// Added only when a model is configured.
    pub model_args: Vec<String>,
    pub extra_args: Vec<String>,
    /// Added only when the user has opted in to unattended runs.
    pub approve_args: Vec<String>,
    /// Added only to a consult, to make the run read-only. Empty means this CLI
    /// cannot be consulted at all.
    pub read_only_args: Vec<String>,
    /// Added after everything else, for CLIs that want the directory spelled out.
    pub workdir_args: Vec<String>,
    /// Flags that open a session under an id spill chooses, with `{session}`
    /// substituted. Empty for a CLI that mints its own id.
    pub session_args: Vec<String>,
    /// Flags that continue the session spill is following, with `{session}`
    /// substituted.
    pub resume_args: Vec<String>,
    pub approve_all: bool,
    pub model: Option<String>,
    pub dialect: Dialect,
}

impl CliSpec {
    /// Whether this tier continues a session across turns at all.
    pub fn continues_sessions(&self) -> bool {
        !self.resume_args.is_empty()
    }

    /// Whether the CLI picks its own session id and reports it, rather than
    /// taking one from us.
    pub fn captures_session(&self) -> bool {
        self.continues_sessions() && self.session_args.is_empty()
    }
}

/// What one call should do about sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCall {
    /// No session: a fresh run, with the whole conversation in the prompt.
    Fresh,
    /// Open a session under an id we chose.
    Open(String),
    /// Continue an existing session, so only the new turn is sent.
    Continue(String),
}

pub struct CliProvider {
    display: String,
    spec: CliSpec,
    workspace: PathBuf,
    /// The session this tier is following, once it has one.
    session: Mutex<Option<String>>,
}

impl CliProvider {
    pub fn new(display: impl Into<String>, spec: CliSpec, workspace: PathBuf) -> Self {
        let session = Mutex::new(None);
        Self {
            display: display.into(),
            spec,
            workspace,
            session,
        }
    }

    /// Decide, and record, which session this call runs under.
    fn plan_session(&self) -> SessionCall {
        if !self.spec.continues_sessions() {
            return SessionCall::Fresh;
        }

        let existing = self
            .session
            .lock()
            .map(|slot| (*slot).clone())
            .unwrap_or_default();

        match existing {
            Some(id) => SessionCall::Continue(id),
            // No session yet. If we can name one, do; otherwise the CLI picks
            // its own and we pick it up from the output.
            None if !self.spec.session_args.is_empty() => {
                let id = uuid::Uuid::new_v4().to_string();
                // The flags are already built with this id, so the session
                // exists as far as the next call is concerned.
                if let Ok(mut slot) = self.session.lock() {
                    *slot = Some(id.clone());
                }
                SessionCall::Open(id)
            }
            None => SessionCall::Fresh,
        }
    }

    /// Follow whichever session the CLI says it actually ran, when it mints its
    /// own rather than taking ours.
    fn note_session(&self, summary: &TurnSummary) {
        let Some(id) = summary.session_id.as_deref() else {
            return;
        };
        if let Ok(mut slot) = self.session.lock() {
            *slot = Some(id.to_string());
        }
    }

    /// Substitute the placeholders an argument template may carry.
    fn substituter<'a>(
        &'a self,
        prompt: &'a str,
        session_id: &'a str,
    ) -> impl Fn(&str) -> String + 'a {
        let workspace = self.workspace.display().to_string();
        let model = self.spec.model.clone().unwrap_or_default();
        move |arg: &str| {
            arg.replace("{prompt}", prompt)
                .replace("{workspace}", &workspace)
                .replace("{model}", &model)
                .replace("{session}", session_id)
        }
    }

    fn build_args(&self, prompt: &str, session: &SessionCall) -> Vec<String> {
        let session_id = match session {
            SessionCall::Fresh => "",
            SessionCall::Open(id) | SessionCall::Continue(id) => id,
        };
        let substitute = self.substituter(prompt, session_id);

        let mut args: Vec<String> = self.spec.args.iter().map(|a| substitute(a)).collect();
        if self.spec.model.is_some() {
            args.extend(self.spec.model_args.iter().map(|a| substitute(a)));
        }
        args.extend(self.spec.extra_args.iter().map(|a| substitute(a)));
        match session {
            SessionCall::Fresh => {}
            SessionCall::Open(_) => {
                args.extend(self.spec.session_args.iter().map(|a| substitute(a)));
            }
            SessionCall::Continue(_) => {
                args.extend(self.spec.resume_args.iter().map(|a| substitute(a)));
            }
        }
        if self.spec.approve_all {
            args.extend(self.spec.approve_args.iter().map(|a| substitute(a)));
        }
        args.extend(self.spec.workdir_args.iter().map(|a| substitute(a)));
        args
    }

    /// The argument vector for a consult.
    fn build_consult_args(&self, prompt: &str) -> Vec<String> {
        let substitute = self.substituter(prompt, "");

        let mut args: Vec<String> = self.spec.args.iter().map(|a| substitute(a)).collect();
        if self.spec.model.is_some() {
            args.extend(self.spec.model_args.iter().map(|a| substitute(a)));
        }
        args.extend(self.spec.extra_args.iter().map(|a| substitute(a)));
        args.extend(self.spec.read_only_args.iter().map(|a| substitute(a)));
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
        let session = self.plan_session();
        // A continued session already holds everything up to this turn, so
        // resending the transcript would duplicate the whole conversation in
        // the CLI's context and be billed again on every turn.
        let prompt = match &session {
            SessionCall::Continue(_) => last_user_turn(&request.messages)
                .unwrap_or_else(|| render_prompt(&request.messages)),
            _ => render_prompt(&request.messages),
        };
        let args = self.build_args(&prompt, &session);

        let summary = match self.run(args, events.clone()).await {
            Ok(summary) => summary,
            Err(error)
                if matches!(session, SessionCall::Continue(_))
                    && error.looks_like_dead_session() =>
            {
                // Command Code (and similar) will reject `--session` when the
                // on-disk transcript is empty or gone. Spill still has the
                // conversation, so start over with it rather than treating a
                // dead file as a stall that consults the frontier.
                self.forget_session();
                let prompt = render_prompt(&request.messages);
                let args = self.build_args(&prompt, &SessionCall::Fresh);
                self.run(args, events).await?
            }
            Err(error) => return Err(error),
        };

        if self.spec.captures_session() {
            self.note_session(&summary);
        }

        Ok(summary)
    }

    /// One consult: the question led by the guard, and the run held read-only.
    async fn consult(
        &self,
        request: ChatRequest,
        events: UnboundedSender<StreamEvent>,
    ) -> Result<TurnSummary, ProviderError> {
        let prompt = format!("{CONSULT_GUARD}\n\n{}", render_prompt(&request.messages));
        let args = self.build_consult_args(&prompt);
        self.run(args, events).await
    }

    fn consult_refusal(&self) -> Option<String> {
        if !self.spec.read_only_args.is_empty() {
            return None;
        }
        Some(
            "it runs its own tools and has no read-only mode configured, so it could act instead \
             of answering"
                .to_string(),
        )
    }

    fn session_id(&self) -> Option<String> {
        self.session.lock().ok().and_then(|slot| slot.clone())
    }

    fn set_session(&self, id: Option<String>) {
        // An empty id is not a session: a CLI that reported a blank string would
        // otherwise resume a conversation that cannot exist.
        let id = id.filter(|value| !value.trim().is_empty());
        if let Ok(mut slot) = self.session.lock() {
            *slot = id;
        }
    }

    fn forget_session(&self) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = None;
        }
    }
}

impl CliProvider {
    /// Spawn the CLI with these arguments and collect its answer.
    async fn run(
        &self,
        args: Vec<String>,
        events: UnboundedSender<StreamEvent>,
    ) -> Result<TurnSummary, ProviderError> {
        let bin = self.spec.bin.clone();

        let mut child = crate::spawn::delegated_cli(&bin)
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

/// A command that exists on every platform, with the flag it takes for a
/// command line. For tests that need *some* installed CLI.
#[cfg(test)]
pub fn portable_shell() -> (&'static str, &'static str) {
    #[cfg(unix)]
    {
        ("sh", "-c")
    }
    #[cfg(windows)]
    {
        ("cmd", "/C")
    }
}

/// Where a command would be found, if it is on PATH at all.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path).find_map(|directory| {
        let direct = directory.join(bin);
        if direct.is_file() {
            return Some(direct);
        }

        // Windows resolves executables by extension.
        if cfg!(windows) {
            return ["exe", "cmd", "bat"]
                .iter()
                .map(|extension| directory.join(format!("{bin}.{extension}")))
                .find(|candidate| candidate.is_file());
        }

        None
    })
}

/// Whether a command can be found on PATH.
pub fn on_path(bin: &str) -> bool {
    which(bin).is_some()
}

/// The newest user turn, which is all a continued session needs.
fn last_user_turn(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.clone())
        .filter(|content| !content.trim().is_empty())
}

/// A CLI takes one prompt, so the conversation is flattened into it.
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
    fn spec(body: &str, dialect: Dialect) -> CliSpec {
        CliSpec {
            bin: "sh".to_string(),
            args: vec!["-c".to_string(), body.to_string(), "{prompt}".to_string()],
            model_args: vec!["-m".to_string(), "{model}".to_string()],
            extra_args: Vec::new(),
            approve_args: vec!["--yolo".to_string()],
            read_only_args: Vec::new(),
            workdir_args: Vec::new(),
            session_args: Vec::new(),
            resume_args: Vec::new(),
            approve_all: false,
            model: None,
            dialect,
        }
    }

    /// A tier that lets us name the session, like grok: `-s` to open, `-r` to
    /// resume.
    fn minting_spec(body: &str) -> CliSpec {
        CliSpec {
            session_args: vec!["-s".to_string(), "{session}".to_string()],
            resume_args: vec!["-r".to_string(), "{session}".to_string()],
            ..spec(body, Dialect::Plain)
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

    /// A body that reports the session flags it was given and the prompt it was
    /// handed: with `sh -c body <prompt> <flags…>`, the prompt is `$0` and the
    /// flags land in `$1` and `$2`.
    fn echo_body() -> &'static str {
        r#"printf '%s|%s|%s\n' "$1" "$2" "$0""#
    }

    fn conversation(question: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::system("be brief"), ChatMessage::user(question)]
    }

    async fn turn(provider: &CliProvider, messages: Vec<ChatMessage>) -> TurnSummary {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        provider
            .stream(
                ChatRequest {
                    model: "test-model".to_string(),
                    messages,
                    tools: Vec::new(),
                },
                tx,
            )
            .await
            .expect("the turn should succeed")
    }

    /// The next turn, as spill would send it: with the CLI already holding the
    /// earlier exchange in its own session.
    fn follow_up(question: &str) -> Vec<ChatMessage> {
        let mut messages = conversation("first question");
        messages.push(ChatMessage::assistant("first answer", Vec::new()));
        messages.push(ChatMessage::user(question));
        messages
    }

    #[test]
    fn session_continuity_follows_from_the_flags() {
        let mut spec = spec("echo hi", Dialect::Plain);
        assert!(
            !spec.continues_sessions(),
            "off unless a preset asks for it"
        );

        spec.resume_args = vec!["-r".to_string(), "{session}".to_string()];
        assert!(spec.continues_sessions());
        assert!(
            spec.captures_session(),
            "with no way to name a session, the CLI must be naming it"
        );

        spec.session_args = vec!["-s".to_string(), "{session}".to_string()];
        assert!(spec.continues_sessions());
        assert!(!spec.captures_session(), "it takes the id we give it");
    }

    #[test]
    fn opening_and_resuming_are_mutually_exclusive() {
        let mut spec = minting_spec("echo hi");
        spec.approve_all = true;
        spec.approve_args = vec!["--yolo".to_string()];
        spec.workdir_args = vec!["--cwd".to_string(), "{workspace}".to_string()];
        let provider = CliProvider::new("x", spec, PathBuf::from("/tmp"));

        assert_eq!(
            provider.build_args("hi", &SessionCall::Open("abc".to_string())),
            vec![
                "-c", "echo hi", "hi", "-s", "abc", "--yolo", "--cwd", "/tmp"
            ]
        );
        assert_eq!(
            provider.build_args("hi", &SessionCall::Continue("abc".to_string())),
            vec![
                "-c", "echo hi", "hi", "-r", "abc", "--yolo", "--cwd", "/tmp"
            ]
        );
        assert_eq!(
            provider.build_args("hi", &SessionCall::Fresh),
            vec!["-c", "echo hi", "hi", "--yolo", "--cwd", "/tmp"],
            "a fresh call carries no session flags at all"
        );
    }

    #[tokio::test]
    async fn a_session_we_name_is_opened_once_and_then_resumed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(minting_spec(echo_body()), dir.path());

        let first = turn(&provider, conversation("first question")).await;
        let opened: Vec<&str> = first.text.split('|').collect();
        assert_eq!(opened[0], "-s", "the first call opens a session");
        assert!(
            uuid::Uuid::parse_str(opened[1]).is_ok(),
            "the id we hand over must be a UUID: {}",
            opened[1]
        );
        assert!(opened[2].contains("first question"), "{}", opened[2]);

        let resumed: Vec<String> = turn(&provider, follow_up("second question"))
            .await
            .text
            .split('|')
            .map(str::to_string)
            .collect();

        assert_eq!(resumed[0], "-r", "the second call resumes");
        assert_eq!(resumed[1], opened[1], "it resumes the session it opened");
        assert_eq!(
            resumed[2], "second question",
            "only the new turn goes out, not the whole transcript again"
        );
    }

    #[tokio::test]
    async fn a_session_the_cli_names_is_captured_and_then_resumed() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The shape a real `cmd -p --output-format json` run has: the id arrives
        // on the first frame, and `--session <id>` continues that transcript.
        let body = r#"
            if [ -z "$2" ]; then
              printf '%s\n' '{"type":"event","event":{"type":"run_start","sessionId":"sess-123"}}' '{"type":"result","subtype":"success","stopReason":"end_turn","finalText":"first answer"}'
            else
              printf '{"type":"result","subtype":"success","sessionId":"%s","stopReason":"end_turn","finalText":"session=%s prompt=%s"}\n' "$2" "$2" "$0"
            fi
        "#;
        let mut spec = spec(body, Dialect::CommandCode);
        spec.resume_args = vec!["--session".to_string(), "{session}".to_string()];
        let provider = provider(spec, dir.path());

        let first = turn(&provider, conversation("first question")).await;
        assert_eq!(first.text, "first answer");

        let second = turn(&provider, follow_up("second question")).await;
        assert_eq!(
            second.text, "session=sess-123 prompt=second question",
            "the captured id should be resumed, with only the new turn"
        );
    }

    #[tokio::test]
    async fn a_dead_session_is_dropped_and_the_turn_runs_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        // First call has no `--session` and succeeds. The second is asked to
        // resume, refuses the way Command Code does for an empty transcript,
        // and the retry must then succeed without that flag.
        let body = r#"
            if [ "$1" = "--session" ]; then
              echo 'Error: --session "'"$2"'" is neither an existing .jsonl transcript nor a known session-id prefix.' >&2
              exit 1
            fi
            printf '%s\n' '{"type":"event","event":{"type":"run_start","sessionId":"sess-dead"}}' '{"type":"result","subtype":"success","stopReason":"end_turn","finalText":"recovered"}'
        "#;
        let mut spec = spec(body, Dialect::CommandCode);
        spec.resume_args = vec!["--session".to_string(), "{session}".to_string()];
        let provider = provider(spec, dir.path());

        let first = turn(&provider, conversation("first question")).await;
        assert_eq!(first.text, "recovered");
        assert_eq!(provider.session_id().as_deref(), Some("sess-dead"));

        let second = turn(&provider, follow_up("second question")).await;
        assert_eq!(second.text, "recovered");
        assert_eq!(
            provider.session_id().as_deref(),
            Some("sess-dead"),
            "the fresh run may mint a session again"
        );
    }

    #[tokio::test]
    async fn forgetting_a_session_makes_the_next_turn_start_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(minting_spec(echo_body()), dir.path());

        let first = turn(&provider, conversation("first question")).await;
        let abandoned = first.text.split('|').nth(1).expect("an id").to_string();

        // What the chain does when this tier's turn is thrown away.
        provider.forget_session();

        let after: Vec<String> = turn(&provider, follow_up("second question"))
            .await
            .text
            .split('|')
            .map(str::to_string)
            .collect();

        assert_eq!(after[0], "-s", "it should open a session, not resume one");
        assert_ne!(
            after[1], abandoned,
            "the discarded session must not be reused"
        );
        assert!(
            after[2].contains("first question"),
            "the transcript goes out again: {}",
            after[2]
        );
    }

    #[tokio::test]
    async fn a_tier_with_no_session_flags_sends_the_whole_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(spec(echo_body(), Dialect::Plain), dir.path());

        let _ = turn(&provider, conversation("first question")).await;
        let second = turn(&provider, follow_up("second question")).await;
        let parts: Vec<&str> = second.text.split('|').collect();

        assert_eq!(parts[0], "", "no session flags are invented");
        assert!(parts[2].contains("first question"), "{}", parts[2]);
        assert!(parts[2].contains("second question"), "{}", parts[2]);
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
            read_only_args: Vec::new(),
            workdir_args: vec!["--cwd".to_string(), "{workspace}".to_string()],
            session_args: Vec::new(),
            resume_args: Vec::new(),
            approve_all: false,
            model: None,
            dialect: Dialect::Grok,
        };

        let provider = CliProvider::new("grok", spec.clone(), workspace.clone());
        let args = provider.build_args("hello", &SessionCall::Fresh);
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
        let args = provider.build_args("hello", &SessionCall::Fresh);
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
            read_only_args: Vec::new(),
            workdir_args: Vec::new(),
            session_args: Vec::new(),
            resume_args: Vec::new(),
            approve_all: true,
            model: None,
            dialect: Dialect::CommandCode,
        };

        let args = CliProvider::new("cmd", spec, workspace).build_args("hi", &SessionCall::Fresh);
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
            read_only_args: Vec::new(),
            workdir_args: Vec::new(),
            session_args: Vec::new(),
            resume_args: Vec::new(),
            approve_all: false,
            model: None,
            dialect: Dialect::Plain,
        };

        let args = CliProvider::new("cmd", spec, workspace)
            .build_args("a; rm -rf /tmp/x", &SessionCall::Fresh);
        // The dangerous text stays inside one argument.
        assert_eq!(args, vec!["-p", "a; rm -rf /tmp/x"]);
    }

    /// A CLI whose preset names a read-only mode, which is what makes it
    /// consultable at all.
    fn consult_spec(body: &str) -> CliSpec {
        let mut spec = spec(body, Dialect::Plain);
        spec.read_only_args = vec!["--permission-mode".to_string(), "plan".to_string()];
        spec
    }

    #[test]
    fn a_consult_carries_the_read_only_flags_and_nothing_that_widens_them() {
        let mut spec = consult_spec("echo hi");
        // Everything that could widen the child's powers, all present on the
        // tier: a consult has to drop every one of them.
        spec.approve_all = true;
        spec.approve_args = vec!["--yolo".to_string()];
        spec.session_args = vec!["-s".to_string(), "{session}".to_string()];
        spec.resume_args = vec!["-r".to_string(), "{session}".to_string()];
        spec.workdir_args = vec!["--cwd".to_string(), "{workspace}".to_string()];
        let provider = CliProvider::new("grok", spec, PathBuf::from("/tmp"));

        assert_eq!(
            provider.build_consult_args("question"),
            vec![
                "-c",
                "echo hi",
                "question",
                "--permission-mode",
                "plan",
                "--cwd",
                "/tmp",
            ],
            "the read-only flags go in; --yolo and the session flags stay out"
        );
    }

    #[test]
    fn which_returns_the_file_not_just_the_answer() {
        // What the doctor report prints, and the reason it prints it: "it is on
        // PATH" cannot say *which* one, and a second copy earlier on PATH is the
        // usual reason a CLI tier behaves differently here than in a shell.
        #[cfg(unix)]
        {
            let found = which("sh").expect("sh is on PATH in any unix environment");
            assert!(found.is_file(), "{}", found.display());
            assert!(found.ends_with("sh"), "{}", found.display());
        }

        // Whatever it finds or does not, the boolean must be the same answer:
        // one rule, two callers.
        assert_eq!(on_path("sh"), which("sh").is_some());
    }

    #[test]
    fn a_command_that_is_not_installed_has_nowhere_to_be_found() {
        let absent = "spill-nothing-is-called-this-anywhere";
        assert!(which(absent).is_none());
        assert!(!on_path(absent));
    }

    #[test]
    fn a_cli_without_a_read_only_flag_refuses_to_be_consulted() {
        // The guarantee is that a consultant cannot act. A harness spill does
        // not run cannot be stripped of its tools, so without a flag to hold it
        // there is no consult to be had.
        let plain = provider(spec("echo hi", Dialect::Plain), Path::new("/tmp"));
        assert!(
            plain.consult_refusal().is_some(),
            "no read-only flag means no consult"
        );

        let guarded = provider(consult_spec("echo hi"), Path::new("/tmp"));
        assert!(
            guarded.consult_refusal().is_none(),
            "a read-only flag is what makes a CLI consultable"
        );
    }

    #[tokio::test]
    async fn a_consult_leads_with_the_guard_instruction() {
        // The flags are what the CLI enforces; the guard is what tells the model
        // why it has no tools. A prompt that did not open with it would let the
        // CLI's own long harness prompt bury the request to answer rather than
        // act.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(consult_spec(echo_body()), dir.path());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let summary = provider
            .consult(
                ChatRequest {
                    model: "test-model".to_string(),
                    messages: vec![ChatMessage::user("why is it stuck?")],
                    tools: Vec::new(),
                },
                tx,
            )
            .await
            .expect("the consult should succeed");

        // `echo_body` prints the first two arguments and then the prompt.
        let parts: Vec<&str> = summary.text.split('|').collect();
        assert_eq!(parts[0], "--permission-mode", "the flag must be passed");
        assert_eq!(parts[1], "plan");
        assert!(
            parts[2].starts_with("You are being consulted"),
            "the guard must lead the prompt: {}",
            parts[2]
        );
        assert!(
            parts[2].contains("why is it stuck?"),
            "the question must still be there: {}",
            parts[2]
        );
    }

    #[test]
    fn the_followed_session_can_be_read_out_and_put_back() {
        // The read/restore pair is what lets a conversation outlive the process:
        // the id is otherwise sealed inside this provider.
        let provider = provider(minting_spec("echo hi"), Path::new("/tmp"));
        assert_eq!(provider.session_id(), None);

        provider.set_session(Some("abc-123".to_string()));
        assert_eq!(provider.session_id().as_deref(), Some("abc-123"));

        // A blank id is not a conversation, and resuming one would send a flag
        // naming nothing.
        provider.set_session(Some("   ".to_string()));
        assert_eq!(provider.session_id(), None);

        provider.set_session(Some("abc-123".to_string()));
        provider.set_session(None);
        assert_eq!(provider.session_id(), None);
    }

    #[tokio::test]
    async fn a_restored_session_is_resumed_rather_than_started_over() {
        // The point of saving the id: the first turn after a restart continues
        // the CLI's own conversation instead of flattening the whole transcript
        // into its prompt and paying for all of it again.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(minting_spec(echo_body()), dir.path());

        provider.set_session(Some("from-last-time".to_string()));

        let resumed: Vec<String> = turn(&provider, follow_up("second question"))
            .await
            .text
            .split('|')
            .map(str::to_string)
            .collect();

        assert_eq!(resumed[0], "-r", "it should resume, not open");
        assert_eq!(resumed[1], "from-last-time");
        assert_eq!(
            resumed[2], "second question",
            "only the new turn goes out, because the CLI already has the rest"
        );
    }

    #[tokio::test]
    async fn a_forgotten_session_makes_the_next_turn_start_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = provider(minting_spec(echo_body()), dir.path());
        provider.set_session(Some("stale".to_string()));

        provider.forget_session();
        assert_eq!(provider.session_id(), None, "forgetting clears the id");

        let opened = turn(&provider, conversation("first question")).await;
        let parts: Vec<&str> = opened.text.split('|').collect();
        assert_eq!(parts[0], "-s", "a fresh session is opened");
        assert!(parts[2].contains("first question"), "{}", parts[2]);
    }
}
