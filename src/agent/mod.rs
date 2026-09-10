//! The agent loop: stream a turn, run any tools the model asks for, feed the
//! results back, and repeat until the model answers without asking for one.

pub mod approval;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::agent::approval::{Approver, Decision};
use crate::agent::tools::{Registry, Risk, ToolOutcome};
use crate::detect::progress::ProgressDetector;
use crate::detect::{StuckReason, Watchdog};
use crate::fallback::{FallbackChain, Tier};
use crate::provider::{ChatRequest, Provider, StreamEvent, TurnSummary, Usage};
use crate::session::{ChatMessage, Session, ToolCall};

/// Default cap on tool steps in a single turn, so a model that keeps calling
/// tools without concluding cannot spin forever.
pub const DEFAULT_MAX_STEPS: usize = 12;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant text to append to the transcript.
    Text(String),
    /// A tool is running.
    ToolStarted { name: String, preview: String },
    /// A tool finished.
    ToolFinished {
        name: String,
        ok: bool,
        summary: String,
    },
    /// The user refused a tool.
    Denied { tool: String },
    /// Something the user should see that is not part of the answer.
    Notice(String),
    /// The active tier stalled, looped, or failed, so the same turn is being
    /// retried on the tier below it.
    Escalated {
        from: String,
        to: String,
        reason: String,
    },
    /// The turn ended normally.
    Finished {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
    /// Every tier was tried and none of them produced an answer.
    Exhausted { reason: String },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub max_steps: usize,
}

/// Start the agent task. Send user prompts on the returned sender; the returned
/// receiver carries everything the agent wants shown.
pub fn spawn(
    config: AgentConfig,
    mut chain: FallbackChain,
    registry: Arc<Registry>,
    approver: Arc<dyn Approver>,
) -> (UnboundedSender<String>, UnboundedReceiver<AgentEvent>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<String>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    tokio::spawn(async move {
        let mut session = Session::with_system_prompt(system_prompt(&config.workspace));
        // A closed command channel means the app is shutting down.
        while let Some(prompt) = command_rx.recv().await {
            run_turn(
                &config,
                &mut chain,
                &registry,
                &approver,
                &event_tx,
                &mut session,
                prompt,
            )
            .await;
        }
    });

    (command_tx, event_rx)
}

/// What one tier made of a turn.
enum Attempt {
    Answered,
    Stuck(StuckReason),
}

/// Run one user turn, moving down the tiers until one of them answers.
///
/// Every tier gets the turn in full: the session is rolled back to the
/// checkpoint before the next tier starts, so a tier never inherits the
/// half-finished output of a tier that was looping.
///
/// The rollback undoes the *conversation*, not the world. If a tier that later
/// stalled already wrote a file or ran a command, that has happened, and the
/// next tier is told so by the tool results still present in its history.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: &AgentConfig,
    chain: &mut FallbackChain,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    session: &mut Session,
    prompt: String,
) {
    session.push(ChatMessage::user(prompt));
    chain.begin_turn();
    let checkpoint = session.messages().len();

    loop {
        let tier = chain.active();
        let outcome = try_tier(config, tier, registry, approver, events, session).await;

        match outcome {
            Attempt::Answered => return,
            Attempt::Stuck(reason) => {
                let from = tier.label.clone();
                // Throw the failed attempt away before another model reads it.
                session.truncate(checkpoint);

                match chain.escalate() {
                    Some(next) => {
                        let _ = events.send(AgentEvent::Escalated {
                            from,
                            to: next.label.clone(),
                            reason: reason.summary(),
                        });
                    }
                    None => {
                        let _ = events.send(AgentEvent::Exhausted {
                            reason: reason.summary(),
                        });
                        return;
                    }
                }
            }
        }
    }
}

/// Give one tier the turn, up to the step limit.
async fn try_tier(
    config: &AgentConfig,
    tier: &Tier,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    session: &mut Session,
) -> Attempt {
    let tools = registry.specs();
    let mut watchdog = Watchdog::new(&tier.limits);
    let mut progress = ProgressDetector::new(tier.limits.max_repeat_run as usize);

    for _step in 0..config.max_steps {
        let request = ChatRequest {
            model: tier.model.clone(),
            messages: session.messages().to_vec(),
            tools: tools.clone(),
        };

        let summary = match stream_turn(&tier.provider, request, events, &mut watchdog).await {
            Ok(summary) => summary,
            Err(reason) => return Attempt::Stuck(reason),
        };

        session.push(ChatMessage::assistant(
            summary.text.clone(),
            summary.tool_calls.clone(),
        ));

        if summary.tool_calls.is_empty() {
            let _ = events.send(AgentEvent::Finished {
                stop_reason: summary.stop_reason.clone(),
                usage: summary.usage,
            });
            return Attempt::Answered;
        }

        for call in summary.tool_calls.clone() {
            let outcome = run_tool(registry, approver, &config.workspace, &call, events).await;
            if let Some(reason) = progress.record(&call.name, &call.arguments, !outcome.is_error) {
                return Attempt::Stuck(reason);
            }
            // The result goes back even when it is an error or a refusal, so the
            // model can see what happened instead of retrying blindly.
            session.push(ChatMessage::tool_result(call.id.clone(), outcome.content));
        }
    }

    Attempt::Stuck(StuckReason::StepLimit {
        steps: config.max_steps,
    })
}

/// Stream one turn from one tier, cutting it off if it goes quiet or loops.
///
/// The request runs as a task so that deciding to abandon it can also *stop*
/// it: a tier that is looping would otherwise keep generating, and keep
/// billing, while nobody is listening.
async fn stream_turn(
    provider: &Arc<dyn Provider>,
    request: ChatRequest,
    events: &UnboundedSender<AgentEvent>,
    watchdog: &mut Watchdog,
) -> Result<TurnSummary, StuckReason> {
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let provider = provider.clone();
    let mut task = tokio::spawn(async move { provider.stream(request, delta_tx).await });

    let outcome = loop {
        let allowance = watchdog.allowance();

        tokio::select! {
            biased;
            joined = &mut task => {
                // A fast response can finish before this loop gets to its
                // queued frames, so drain them first: otherwise a looping
                // answer that arrived in one burst would look healthy.
                let mut tripped = None;
                while let Ok(event) = delta_rx.try_recv() {
                    if let Some(reason) = observe(event, events, watchdog) {
                        tripped = Some(reason);
                        break;
                    }
                }
                break match (tripped, joined) {
                    (Some(reason), _) => Err(reason),
                    (None, Ok(Ok(summary))) => Ok(summary),
                    (None, Ok(Err(error))) => Err(StuckReason::Failed {
                        detail: error.to_string(),
                    }),
                    (None, Err(_)) => Err(StuckReason::Failed {
                        detail: "the model task ended unexpectedly".to_string(),
                    }),
                };
            }
            received = tokio::time::timeout(allowance, delta_rx.recv()) => {
                match received {
                    // Nothing at all arrived within this tier's allowance.
                    Err(_) => break Err(Watchdog::stall_reason(allowance)),
                    // The stream closed; the join above now holds the answer.
                    Ok(None) => continue,
                    Ok(Some(event)) => {
                        if let Some(reason) = observe(event, events, watchdog) {
                            break Err(reason);
                        }
                    }
                }
            }
        }
    };

    // Abandoning a turn has to stop the request, not just ignore its output.
    if outcome.is_err() {
        task.abort();
    }

    outcome
}

/// Feed one stream event to the watchdog, forwarding text to the UI.
fn observe(
    event: StreamEvent,
    events: &UnboundedSender<AgentEvent>,
    watchdog: &mut Watchdog,
) -> Option<StuckReason> {
    match event {
        StreamEvent::Activity => {
            watchdog.note_activity();
            None
        }
        StreamEvent::Text(text) => {
            let _ = events.send(AgentEvent::Text(text.clone()));
            watchdog.feed(&text)
        }
    }
}

async fn run_tool(
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    workspace: &std::path::Path,
    call: &ToolCall,
    events: &UnboundedSender<AgentEvent>,
) -> ToolOutcome {
    let Some(tool) = registry.get(&call.name) else {
        let message = format!(
            "there is no tool called {:?}. Available tools: {}",
            call.name,
            registry.names().join(", ")
        );
        let _ = events.send(AgentEvent::Notice(message.clone()));
        return ToolOutcome::error(message);
    };

    let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            let message = format!(
                "the arguments for {} were not valid JSON ({error}). Arguments received: {}",
                call.name, call.arguments
            );
            let _ = events.send(AgentEvent::Notice(message.clone()));
            return ToolOutcome::error(message);
        }
    };

    let preview = tool.preview(&arguments, workspace).await;

    // Only tools that can change something need permission; the short-circuit
    // keeps read-only tools from ever reaching the approver.
    if tool.risk() == Risk::Write && approver.decide(&call.name, &preview).await == Decision::Deny {
        let _ = events.send(AgentEvent::Denied {
            tool: call.name.clone(),
        });
        return ToolOutcome::error(format!(
            "the user declined to run {}. Do not repeat it; ask what they would prefer instead.",
            call.name
        ));
    }

    let _ = events.send(AgentEvent::ToolStarted {
        name: call.name.clone(),
        preview: preview.clone(),
    });
    let outcome = tool.run(&arguments, workspace).await;

    let _ = events.send(AgentEvent::ToolFinished {
        name: call.name.clone(),
        ok: !outcome.is_error,
        summary: first_line(&outcome.content),
    });

    outcome
}

/// A tool's output can be thousands of lines; the transcript gets the gist.
pub fn first_line(content: &str) -> String {
    let line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if line.chars().count() > 160 {
        let head: String = line.chars().take(160).collect();
        format!("{head}…")
    } else if line.is_empty() {
        "(no output)".to_string()
    } else {
        line.to_string()
    }
}

fn system_prompt(workspace: &std::path::Path) -> String {
    format!(
        "You are spill, a coding agent working in the user's terminal. The workspace is {}.\n\n\
         Inspect before you change: read a file before editing it, and search for a path rather \
         than guessing at it. Prefer edit_file for a targeted change and write_file for a new \
         file.\n\n\
         File writes and shell commands need the user's approval, so say briefly what you are \
         about to do and why. When a tool returns an error, read it and correct yourself rather \
         than repeating the same call.\n\n\
         Keep final answers short and concrete.",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::approval::testing::{AlwaysApprove, AlwaysDeny};
    use crate::agent::tools::Registry;
    use crate::config::Limits;
    use crate::provider::{ProviderError, TurnSummary};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Replays scripted turns and records what it was asked.
    struct ScriptedProvider {
        turns: Mutex<VecDeque<TurnSummary>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// When the script runs out, keep returning this instead.
        fallback: TurnSummary,
        fail_with: Option<String>,
    }

    impl ScriptedProvider {
        fn new(turns: Vec<TurnSummary>) -> Arc<Self> {
            Arc::new(Self {
                turns: Mutex::new(turns.into()),
                requests: Mutex::new(Vec::new()),
                fallback: TurnSummary::default(),
                fail_with: None,
            })
        }

        fn failing(message: &str) -> Arc<Self> {
            Arc::new(Self {
                turns: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
                fallback: TurnSummary::default(),
                fail_with: Some(message.to_string()),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("lock").len()
        }

        fn request(&self, index: usize) -> ChatRequest {
            self.requests.lock().expect("lock")[index].clone()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn describe(&self) -> String {
            "scripted".to_string()
        }

        async fn stream(
            &self,
            request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            self.requests.lock().expect("lock").push(request);

            if let Some(message) = &self.fail_with {
                return Err(ProviderError::Broken {
                    target: "scripted".to_string(),
                    detail: message.clone(),
                });
            }

            let turn = self
                .turns
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone());

            if !turn.text.is_empty() {
                let _ = events.send(StreamEvent::Text(turn.text.clone()));
            }
            Ok(turn)
        }
    }

    fn answer(text: &str) -> TurnSummary {
        TurnSummary {
            text: text.to_string(),
            stop_reason: Some("end_turn".to_string()),
            ..TurnSummary::default()
        }
    }

    fn calls_tool(name: &str, arguments: &str) -> TurnSummary {
        TurnSummary {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            usage: None,
        }
    }

    fn config(workspace: &std::path::Path, max_steps: usize) -> AgentConfig {
        AgentConfig {
            workspace: workspace.to_path_buf(),
            max_steps,
        }
    }

    /// A chain of one, which is what most of these tests want.
    fn single_tier(provider: Arc<dyn Provider>) -> FallbackChain {
        chain_of(vec![(provider, Limits::default())])
    }

    /// Scripted tiers in order, so escalation can be driven end to end.
    fn chain_of(tiers: Vec<(Arc<dyn Provider>, Limits)>) -> FallbackChain {
        let tiers = tiers
            .into_iter()
            .enumerate()
            .map(|(index, (provider, limits))| Tier {
                label: format!("Tier {index}"),
                model: format!("model-{index}"),
                provider,
                limits,
            })
            .collect();
        FallbackChain::new(tiers, true).expect("at least one tier")
    }

    /// Collect events until the turn ends, failing rather than hanging.
    async fn drain(mut rx: UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the turn should not hang")
                .expect("the agent should emit an event");
            let terminal = matches!(
                event,
                AgentEvent::Finished { .. } | AgentEvent::Exhausted { .. }
            );
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    /// Drive one prompt through a single-tier chain.
    async fn run_one(
        script: Vec<TurnSummary>,
        approver: Arc<dyn Approver>,
        workspace: &std::path::Path,
        prompt: &str,
    ) -> (Vec<AgentEvent>, Arc<ScriptedProvider>) {
        let provider = ScriptedProvider::new(script);
        let (tx, rx) = spawn(
            config(workspace, DEFAULT_MAX_STEPS),
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            approver,
        );
        tx.send(prompt.to_string()).expect("send prompt");

        (drain(rx).await, provider)
    }

    #[tokio::test]
    async fn a_turn_without_tools_emits_text_and_finishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (events, provider) = run_one(
            vec![answer("all done")],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "hello",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t == "all done"))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Finished { stop_reason: Some(reason), .. }) if reason == "end_turn"
        ));
        assert_eq!(provider.request_count(), 1);
    }

    #[tokio::test]
    async fn the_user_prompt_reaches_the_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_events, provider) = run_one(
            vec![answer("ok")],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "what is in this project?",
        )
        .await;

        let first = provider.request(0);
        assert_eq!(first.model, "model-0");
        assert_eq!(first.messages[0].role, crate::session::Role::System);
        assert!(
            first
                .messages
                .iter()
                .any(|m| m.content == "what is in this project?")
        );
    }

    #[tokio::test]
    async fn a_read_tool_runs_without_asking_and_its_result_is_fed_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "contents here").expect("write");

        let approver = Arc::new(AlwaysApprove::default());
        let (events, provider) = run_one(
            vec![
                calls_tool("read_file", r#"{"path":"note.txt"}"#),
                answer("read it"),
            ],
            approver.clone(),
            dir.path(),
            "read note.txt",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { name, .. } if name == "read_file"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { ok: true, .. }))
        );
        assert!(
            approver.asked.lock().expect("lock").is_empty(),
            "a read-only tool must not prompt"
        );

        // The second request must carry the tool result back to the model.
        assert_eq!(provider.request_count(), 2);
        let second = provider.request(1);
        let tool_message = second
            .messages
            .iter()
            .find(|m| m.role == crate::session::Role::Tool)
            .expect("a tool result should have been sent back");
        assert_eq!(tool_message.tool_call_id.as_deref(), Some("call_1"));
        assert!(
            tool_message.content.contains("contents here"),
            "{}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn a_write_tool_runs_only_after_approval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());
        let (events, _provider) = run_one(
            vec![
                calls_tool("write_file", r#"{"path":"new.txt","content":"hi"}"#),
                answer("wrote it"),
            ],
            approver.clone(),
            dir.path(),
            "make a file",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { ok: true, .. }))
        );
        assert!(
            dir.path().join("new.txt").exists(),
            "the approved write should have happened"
        );

        let asked = approver.asked.lock().expect("lock");
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].1.contains("new.txt"),
            "preview was: {}",
            asked[0].1
        );
    }

    #[tokio::test]
    async fn a_denied_write_does_not_happen_and_is_reported_to_the_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (events, provider) = run_one(
            vec![
                calls_tool("write_file", r#"{"path":"new.txt","content":"hi"}"#),
                answer("understood"),
            ],
            Arc::new(AlwaysDeny),
            dir.path(),
            "make a file",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Denied { tool } if tool == "write_file"))
        );
        assert!(
            !dir.path().join("new.txt").exists(),
            "a denied write must not touch the disk"
        );

        let second = provider.request(1);
        let tool_message = second
            .messages
            .iter()
            .find(|m| m.role == crate::session::Role::Tool)
            .expect("the refusal should be fed back");
        assert!(
            tool_message.content.contains("declined"),
            "{}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_becomes_a_tool_error_the_model_can_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (events, provider) = run_one(
            vec![calls_tool("launch_missiles", "{}"), answer("sorry")],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "do something impossible",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice(m) if m.contains("launch_missiles")))
        );

        let second = provider.request(1);
        let tool_message = second
            .messages
            .iter()
            .find(|m| m.role == crate::session::Role::Tool)
            .expect("the failure should be fed back");
        assert!(
            tool_message.content.contains("no tool called"),
            "{}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn malformed_arguments_become_a_tool_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (events, provider) = run_one(
            vec![calls_tool("read_file", "{not json"), answer("oops")],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "read something",
        )
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice(m) if m.contains("not valid JSON")))
        );

        let second = provider.request(1);
        let tool_message = second
            .messages
            .iter()
            .find(|m| m.role == crate::session::Role::Tool)
            .expect("the failure should be fed back");
        assert!(
            tool_message.content.contains("not valid JSON"),
            "{}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn a_provider_error_with_nowhere_to_go_is_reported_to_the_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::failing("connection reset");
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("hello".to_string()).expect("send");

        let events = drain(rx).await;
        match events.last() {
            Some(AgentEvent::Exhausted { reason }) => {
                assert!(reason.contains("connection reset"), "{reason}")
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_tier_that_never_finishes_is_given_up_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");

        // Always asks for a tool, so the step limit is what ends the attempt.
        let always_tools = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            fallback: calls_tool("read_file", r#"{"path":"note.txt"}"#),
            fail_with: None,
        });

        let (tx, rx) = spawn(
            config(dir.path(), 2),
            single_tier(always_tools.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("loop please".to_string()).expect("send");

        let events = drain(rx).await;
        match events.last() {
            Some(AgentEvent::Exhausted { reason }) => {
                assert!(reason.contains("used all 2 tool steps"), "{reason}")
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
        assert_eq!(always_tools.request_count(), 2);
    }

    #[tokio::test]
    async fn a_truncated_answer_is_reported_through_the_stop_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let truncated = TurnSummary {
            text: "half a sen".to_string(),
            stop_reason: Some("length".to_string()),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 4,
            }),
            tool_calls: Vec::new(),
        };

        let (events, _provider) = run_one(
            vec![truncated],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "write an essay",
        )
        .await;

        match events.last() {
            Some(AgentEvent::Finished {
                stop_reason,
                usage: Some(usage),
            }) => {
                assert_eq!(stop_reason.as_deref(), Some("length"));
                assert_eq!(usage.completion_tokens, 4);
            }
            other => panic!("expected a finished turn with usage, got {other:?}"),
        }
    }

    #[test]
    fn the_system_prompt_names_the_workspace() {
        let prompt = system_prompt(std::path::Path::new("/tmp/example"));
        assert!(prompt.contains("/tmp/example"), "{prompt}");
    }

    #[test]
    fn tool_summaries_are_trimmed_to_one_line() {
        assert_eq!(first_line("first\nsecond"), "first");
        assert_eq!(first_line("\n\n  \nreal"), "real");
        assert_eq!(first_line(""), "(no output)");

        let long = "x".repeat(200);
        assert!(first_line(&long).chars().count() <= 161);
    }

    // ---- escalation -------------------------------------------------------

    fn looping_answer() -> TurnSummary {
        TurnSummary {
            text: "the same line\nthe same line\nthe same line\nthe same line\n".to_string(),
            stop_reason: Some("end_turn".to_string()),
            ..TurnSummary::default()
        }
    }

    /// Accepts the request and then never answers, standing in for a box that
    /// has stopped responding.
    struct Hanging;

    #[async_trait]
    impl Provider for Hanging {
        fn describe(&self) -> String {
            "hanging".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    fn escalation(events: &[AgentEvent]) -> Option<(String, String, String)> {
        events.iter().find_map(|event| match event {
            AgentEvent::Escalated { from, to, reason } => {
                Some((from.clone(), to.clone(), reason.clone()))
            }
            _ => None,
        })
    }

    fn short_allowance() -> Limits {
        Limits {
            first_token_timeout_ms: 50,
            idle_timeout_ms: 50,
            max_repeat_run: 4,
        }
    }

    #[tokio::test]
    async fn a_looping_tier_falls_through_to_the_next_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let looping = ScriptedProvider::new(vec![looping_answer()]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (looping.clone(), Limits::default()),
            (healthy.clone(), Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("do the thing".to_string()).expect("send");
        let events = drain(rx).await;

        let (from, to, reason) = escalation(&events).expect("a looping tier should escalate");
        assert_eq!(from, "Tier 0");
        assert_eq!(to, "Tier 1");
        assert!(reason.contains("repeated"), "reason was {reason:?}");

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t == "recovered"))
        );
        assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));

        // The next tier must not inherit the half-finished answer.
        assert_eq!(healthy.request_count(), 1);
        let second = healthy.request(0);
        assert!(
            !second
                .messages
                .iter()
                .any(|m| m.content.contains("the same line")),
            "the looping tier's output leaked into the next tier's history"
        );
        assert!(second.messages.iter().any(|m| m.content == "do the thing"));
    }

    #[tokio::test]
    async fn a_quiet_tier_falls_through_rather_than_blocking_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (Arc::new(Hanging), short_allowance()),
            (healthy, Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("are you there".to_string()).expect("send");
        let events = drain(rx).await;

        let (_, to, reason) = escalation(&events).expect("a hung tier should escalate");
        assert_eq!(to, "Tier 1");
        assert!(reason.contains("went quiet"), "reason was {reason:?}");
        assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn a_failing_tier_falls_through_to_a_working_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (
                ScriptedProvider::failing("connection refused"),
                Limits::default(),
            ),
            (healthy, Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("hello".to_string()).expect("send");
        let events = drain(rx).await;

        let (_, to, reason) = escalation(&events).expect("a failed tier should escalate");
        assert_eq!(to, "Tier 1");
        assert!(
            reason.contains("connection refused"),
            "reason was {reason:?}"
        );
        assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn a_tier_stuck_calling_the_same_tool_falls_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");

        let stubborn = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            fallback: calls_tool("read_file", r#"{"path":"note.txt"}"#),
            fail_with: None,
        });
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (stubborn.clone(), Limits::default()),
            (healthy, Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("read it".to_string()).expect("send");
        let events = drain(rx).await;

        let (_, _, reason) = escalation(&events).expect("a tool loop should escalate");
        assert!(
            reason.contains("identical arguments"),
            "reason was {reason:?}"
        );
        assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn every_tier_looping_ends_in_exhaustion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (
                ScriptedProvider::new(vec![looping_answer()]),
                Limits::default(),
            ),
            (
                ScriptedProvider::new(vec![looping_answer()]),
                Limits::default(),
            ),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send("go".to_string()).expect("send");
        let events = drain(rx).await;

        assert!(
            escalation(&events).is_some(),
            "the top tier should have been abandoned"
        );
        match events.last() {
            Some(AgentEvent::Exhausted { reason }) => {
                assert!(reason.contains("repeated"), "reason was {reason:?}")
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
    }
}
