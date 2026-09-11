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

/// How many recent user turns compaction leaves intact.
///
/// Enough that the model still knows what is being worked on, few enough that
/// the frontier's cold prefix stays small. Only older turns are folded away.
pub const KEEP_TURNS: usize = 3;

/// Below this many user turns there is nothing worth compacting, so an
/// escalation does not churn the history of a short session.
const COMPACT_ABOVE_TURNS: usize = KEEP_TURNS + 2;

/// What a turn is allowed to do.
///
/// Two modes, because there are two things a person wants from an agent: work
/// it out, or do it. Plan is read-only, and that is a promise kept in three
/// places rather than one — the write tools are not offered, a call for one is
/// refused anyway, and the system prompt says why. A read-only mode that relies
/// on the model choosing to behave is not read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Everything the agent can do, with approval as configured.
    #[default]
    Build,
    /// Reading, searching, and thinking. Nothing is changed.
    Plan,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Build => Self::Plan,
            Self::Plan => Self::Build,
        }
    }

    /// The ceiling on what a turn in this mode may touch.
    pub fn risk_ceiling(self) -> Risk {
        match self {
            Self::Build => Risk::Write,
            Self::Plan => Risk::Read,
        }
    }

    pub fn is_read_only(self) -> bool {
        self.risk_ceiling() == Risk::Read
    }

    /// The system prompt for this mode.
    ///
    /// Plan mode needs its own prompt rather than a sentence appended to the
    /// usual one: the usual prompt explains that writes need approval, which is
    /// the wrong thing to teach a turn that has no writes to approve.
    pub fn system_prompt(self, workspace: &std::path::Path) -> String {
        match self {
            Self::Build => build_prompt(workspace),
            Self::Plan => plan_prompt(workspace),
        }
    }
}

/// What the interface asks the agent to do.
///
/// Most of these exist because the state they touch lives with the agent — the
/// chain and the conversation — and cannot be reached from the render loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// An ordinary message, and a turn.
    Prompt(String),
    /// Change what a turn is allowed to do.
    SetMode(Mode),
    /// Move to the next tier now, and stay there.
    Escalate,
    /// Send the last turn again, optionally on a named tier.
    Retry { tier: Option<String> },
    /// Forget the active tier's own conversation.
    Drop,
    /// Use this tier until told otherwise. `None` hands control back to the
    /// fallback policy.
    SetTier(Option<String>),
    /// Change whether a spill keeps the lower tier.
    SetSticky(bool),
    /// Fold earlier turns into a ledger.
    Compact,
    /// Start the conversation over.
    Clear,
    /// Report what is being sent each turn.
    Context,
}

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

/// The conversation and the chain, plus what it takes to run the last turn
/// again.
struct Loop {
    session: Session,
    chain: FallbackChain,
    /// What a turn is allowed to do. Held here rather than sent with each
    /// prompt, because the system prompt has to change with it and the session
    /// is what carries the system prompt.
    mode: Mode,
    /// The turn most recently started, so it can be retried.
    last: Option<LastTurn>,
}

#[derive(Clone)]
struct LastTurn {
    prompt: String,
    /// The session length before the turn was added, so a retry can put the
    /// conversation back exactly as it was.
    checkpoint: usize,
}

/// Start the agent task. Send commands on the returned sender; the returned
/// receiver carries everything the agent wants shown.
pub fn spawn(
    config: AgentConfig,
    chain: FallbackChain,
    registry: Arc<Registry>,
    approver: Arc<dyn Approver>,
) -> (UnboundedSender<Command>, UnboundedReceiver<AgentEvent>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    tokio::spawn(async move {
        let mut state = Loop {
            session: Session::with_system_prompt(Mode::default().system_prompt(&config.workspace)),
            chain,
            mode: Mode::default(),
            last: None,
        };

        // A closed command channel means the app is shutting down.
        while let Some(command) = command_rx.recv().await {
            handle_command(
                &config, &registry, &approver, &event_tx, &mut state, command,
            )
            .await;
        }
    });

    (command_tx, event_rx)
}

/// Act on one thing the interface asked for.
async fn handle_command(
    config: &AgentConfig,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    state: &mut Loop,
    command: Command,
) {
    match command {
        Command::Prompt(prompt) => {
            let checkpoint = state.session.messages().len();
            state.last = Some(LastTurn {
                prompt: prompt.clone(),
                checkpoint,
            });
            run_turn(
                config,
                &mut state.chain,
                state.mode,
                registry,
                approver,
                events,
                &mut state.session,
                prompt,
            )
            .await;
        }

        Command::SetMode(mode) => {
            state.mode = mode;
            // The system prompt is the first message of the session, so changing
            // the mode means changing it. Leaving the old one in place would
            // have a read-only turn still being told about approval prompts.
            if let Some(first) = state.session.messages().first() {
                if first.role == crate::session::Role::System {
                    state
                        .session
                        .replace_system_prompt(mode.system_prompt(&config.workspace));
                }
            }
            let _ = events.send(AgentEvent::Notice(match mode {
                Mode::Plan => "plan mode: it can read and search, but nothing will be changed. \
                              You will get a plan rather than an edit."
                    .to_string(),
                Mode::Build => "build mode: it can write files and run commands again, asking \
                                first unless the tier is set to run unattended."
                    .to_string(),
            }));
        }

        Command::Escalate => {
            // Escalating from the last tier has nowhere to go, which is worth
            // saying rather than silently doing nothing.
            let stepped = state.chain.escalate().map(|tier| tier.label.clone());
            match stepped {
                Some(label) => {
                    let index = state.chain.active_index();
                    let total = state.chain.len();
                    // A hand-issued escalation is a choice, not a fallback, so
                    // it is pinned: otherwise a per-turn chain would snap back
                    // to the top on the next message and the command would look
                    // broken.
                    state.chain.pin(index);
                    let _ = events.send(AgentEvent::Notice(format!(
                        "moving to {label} ({} of {total}) — it will answer from here",
                        index + 1
                    )));
                }
                None => {
                    let _ = events.send(AgentEvent::Notice(
                        "already on the last tier in the chain, so there is nowhere to spill to"
                            .to_string(),
                    ));
                }
            }
        }

        Command::Retry { tier } => {
            let Some(last) = state.last.clone() else {
                let _ = events.send(AgentEvent::Notice(
                    "there is no turn to retry yet".to_string(),
                ));
                return;
            };

            if let Some(query) = tier {
                match state.chain.resolve(&query) {
                    Some(index) => {
                        state.chain.pin(index);
                    }
                    None => {
                        let _ = events.send(AgentEvent::Notice(format!(
                            "no tier matches {query:?}. The chain is: {}",
                            describe_chain(&state.chain)
                        )));
                        return;
                    }
                }
            }

            // Put the conversation back the way it was before that turn, so the
            // retry does not stack a second copy of it on top of the first.
            state.session.truncate(last.checkpoint);
            // If the tier holding the turn is a CLI, its own copy must go too.
            state.chain.forget_sessions();

            let _ = events.send(AgentEvent::Notice(format!(
                "retrying the last turn on {}",
                state.chain.active().label
            )));
            run_turn(
                config,
                &mut state.chain,
                state.mode,
                registry,
                approver,
                events,
                &mut state.session,
                last.prompt,
            )
            .await;
        }

        Command::Drop => {
            let tier = state.chain.active();
            tier.provider.forget_session();
            let _ = events.send(AgentEvent::Notice(format!(
                "{} will start a fresh conversation on its next turn",
                tier.label
            )));
        }

        Command::SetTier(tier) => match tier {
            None => {
                state.chain.unpin();
                state.chain.begin_turn();
                let _ = events.send(AgentEvent::Notice(format!(
                    "back to the configured order — {} answers next",
                    state.chain.active().label
                )));
            }
            Some(query) => match state.chain.resolve(&query) {
                Some(index) => {
                    let label = state.chain.pin(index).map(|tier| tier.label.clone());
                    let _ = events.send(AgentEvent::Notice(format!(
                        "{} will answer from here, until you say /tier auto",
                        label.unwrap_or_default()
                    )));
                }
                None => {
                    let _ = events.send(AgentEvent::Notice(format!(
                        "no tier matches {query:?}. The chain is: {}",
                        describe_chain(&state.chain)
                    )));
                }
            },
        },

        Command::SetSticky(on) => {
            state.chain.set_sticky(on);
            let _ = events.send(AgentEvent::Notice(if on {
                "a spill will stay on the lower tier for the rest of the session".to_string()
            } else {
                "the top tier will be retried on each new message".to_string()
            }));
        }

        Command::Compact => {
            let report = state.session.compact(KEEP_TURNS);
            let summary = report.summary();
            if report.happened() {
                // The conversation the CLI tiers were holding no longer exists
                // in that form, so none of them may resume it.
                state.chain.forget_sessions();
                let _ = events.send(AgentEvent::Notice(format!(
                    "{summary}\nEvery tier's own session was dropped, so the next call re-reads \
                     the compacted history once and then continues from there."
                )));
            } else {
                let _ = events.send(AgentEvent::Notice(summary));
            }
        }

        Command::Clear => {
            state.session.reset();
            state.last = None;
            state.chain.forget_sessions();
            let _ = events.send(AgentEvent::Notice(
                "conversation cleared; the tiers are unchanged".to_string(),
            ));
        }

        Command::Context => {
            let _ = events.send(AgentEvent::Notice(describe_context(
                &state.session,
                &state.chain,
            )));
        }
    }
}

/// What a `/tier` or `/retry` message shows when a name does not match.
fn describe_chain(chain: &FallbackChain) -> String {
    chain
        .labels()
        .iter()
        .enumerate()
        .map(|(index, label)| format!("{} {}", index + 1, crate::fallback::tier_name(label)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `/context` reports: what actually goes out on each turn.
fn describe_context(session: &Session, chain: &FallbackChain) -> String {
    let messages = session.messages();
    let characters: usize = messages.iter().map(|m| m.content.len()).sum();
    let tools = messages
        .iter()
        .map(|message| message.tool_calls.len())
        .sum::<usize>();
    let active = chain.active();

    // An estimate, and said as one: the real figure depends on the tokenizer,
    // which is the provider's business.
    let estimate = characters / 4;

    let mut out = format!(
        "conversation: {} message{} (~{characters} characters, roughly {estimate} tokens)\n\
         tool calls in history: {tools}",
        messages.len(),
        if messages.len() == 1 { "" } else { "s" }
    );

    // Which tiers actually pay for it, which is not the same for each kind.
    out.push_str(&format!(
        "\nanswering tier: {} ({})",
        active.label, active.model
    ));
    out.push_str(if chain.sticky() {
        "\nfallback: sticky"
    } else {
        "\nfallback: per turn"
    });
    if chain.is_pinned() {
        out.push_str("\na tier was chosen by hand: /tier auto returns to the configured order");
    }
    out
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
    mode: Mode,
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
        let outcome = try_tier(config, tier, mode, registry, approver, events, session).await;

        match outcome {
            Attempt::Answered => return,
            Attempt::Stuck(reason) => {
                let from = tier.label.clone();
                let abandoned = Arc::clone(&tier.provider);
                // Throw the failed attempt away before another model reads it.
                session.truncate(checkpoint);
                // A CLI tier may be holding a session that contains the output
                // just discarded, so it must not be resumed.
                abandoned.forget_session();

                match chain.escalate() {
                    Some(next) => {
                        let _ = events.send(AgentEvent::Escalated {
                            from,
                            to: next.label.clone(),
                            reason: reason.summary(),
                        });
                        compact_before_falling(session, chain, events);
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

/// Shrink the history when a tier is about to be abandoned, so the tier that
/// takes over does not open with a large cold read.
///
/// A fallback is a cache miss whatever we do — caches are per provider — so the
/// only lever is making the thing being re-read small. Compaction is deliberately
/// not attempted on a short session: the churn would cost more than it saved.
fn compact_before_falling(
    session: &mut Session,
    chain: &FallbackChain,
    events: &UnboundedSender<AgentEvent>,
) {
    let turns = session
        .messages()
        .iter()
        .filter(|message| message.role == crate::session::Role::User)
        .count();
    if turns <= COMPACT_ABOVE_TURNS {
        return;
    }

    let report = session.compact(KEEP_TURNS);
    if !report.happened() {
        return;
    }

    // The other tiers are holding conversations in their own sessions; after
    // the history changed shape, resuming one would continue a transcript that
    // no longer matches.
    chain.forget_sessions();
    let _ = events.send(AgentEvent::Notice(format!(
        "compacted {} earlier turn{} so the next tier starts from a smaller history",
        report.dropped_turns,
        if report.dropped_turns == 1 { "" } else { "s" }
    )));
}

/// Give one tier the turn, up to the step limit.
async fn try_tier(
    config: &AgentConfig,
    tier: &Tier,
    mode: Mode,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    session: &mut Session,
) -> Attempt {
    // A read-only turn is not offered the tools that could change anything. The
    // refusal in `run_tool` is what makes that a guarantee; this is what stops a
    // cooperative model from wasting turns on calls that would be refused.
    let tools = registry.specs_permitting(mode.risk_ceiling());
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
            let outcome =
                run_tool(mode, registry, approver, &config.workspace, &call, events).await;
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
    mode: Mode,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    workspace: &std::path::Path,
    call: &ToolCall,
    events: &UnboundedSender<AgentEvent>,
) -> ToolOutcome {
    let offered = || -> Vec<String> {
        registry
            .specs_permitting(mode.risk_ceiling())
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    };

    let Some(tool) = registry.get(&call.name) else {
        let message = format!(
            "there is no tool called {:?}. Available tools: {}",
            call.name,
            offered().join(", ")
        );
        let _ = events.send(AgentEvent::Notice(message.clone()));
        return ToolOutcome::error(message);
    };

    // The guarantee. Withholding these from the tool list is the polite version
    // of this check; a model that names one anyway — hallucinating a tool name,
    // or carrying a habit from build mode — is stopped here, before a preview is
    // even computed, so nothing about the call can touch the disk.
    if !mode.risk_ceiling().permits(tool.risk()) {
        let message = format!(
            "{} is not available in plan mode, which is read-only. Nothing has been changed. \
             Read and search as much as you need, then reply with the plan instead of carrying \
             it out.",
            call.name
        );
        let _ = events.send(AgentEvent::Notice(format!(
            "✗ {} was refused — plan mode is read-only",
            call.name
        )));
        return ToolOutcome::error(message);
    }

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

fn build_prompt(workspace: &std::path::Path) -> String {
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

/// The system prompt for a read-only turn.
///
/// It says plainly that the write tools are not merely discouraged but absent,
/// because a model told only "do not change anything" tends to announce changes
/// it did not make, and one that discovers the absence for itself tends to spend
/// turns trying.
fn plan_prompt(workspace: &std::path::Path) -> String {
    format!(
        "You are spill, a coding agent working in the user's terminal. The workspace is {}.\n\n\
         You are in PLAN MODE. This turn is read-only: you can read files, list directories, \
         search and glob, and that is all. write_file, edit_file and run_shell do not exist for \
         you right now, and asking for one is refused. Nothing you do can change anything.\n\n\
         Investigate as much as you need — read the relevant files, follow the code, check how \
         the thing is used elsewhere — and then answer with a plan rather than a change:\n\n\
         - what you would change, file by file, and why\n\
         - the order to do it in, and what depends on what\n\
         - anything you would need to check, or would want the user to decide, first\n\n\
         Be specific enough that the plan could be carried out without asking you again. Never \
         say you have made a change, because you have not: report what you found and what you \
         would do. The user returns to build mode when they are ready for you to act.",
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
            session_id: None,
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
        drain_from(&mut rx).await
    }

    /// The same, over a borrow, for a test that needs the receiver again after.
    async fn drain_from(rx: &mut UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
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
        tx.send(Command::Prompt(prompt.to_string()))
            .expect("send prompt");

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
        tx.send(Command::Prompt("hello".to_string())).expect("send");

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
        tx.send(Command::Prompt("loop please".to_string()))
            .expect("send");

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
                ..Default::default()
            }),
            tool_calls: Vec::new(),
            session_id: None,
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
    fn the_system_prompt_names_the_workspace_in_both_modes() {
        let workspace = std::path::Path::new("/tmp/example");
        for mode in [Mode::Build, Mode::Plan] {
            let prompt = mode.system_prompt(workspace);
            assert!(prompt.contains("/tmp/example"), "{prompt}");
        }
    }

    #[test]
    fn plan_mode_says_it_cannot_change_anything_and_build_mode_explains_approval() {
        let workspace = std::path::Path::new("/tmp/example");

        let plan = Mode::Plan.system_prompt(workspace);
        assert!(plan.contains("PLAN MODE"), "{plan}");
        assert!(
            plan.contains("write_file"),
            "it should name what it cannot use: {plan}"
        );
        assert!(
            plan.contains("Never say you have made a change"),
            "the failure mode to head off: {plan}"
        );
        assert!(
            !plan.contains("need the user's approval"),
            "approval is the wrong thing to teach a turn with no writes to approve: {plan}"
        );

        let build = Mode::Build.system_prompt(workspace);
        assert!(build.contains("approval"), "{build}");
        assert!(!build.contains("PLAN MODE"), "{build}");
    }

    #[test]
    fn the_mode_toggles_and_carries_its_own_ceiling() {
        assert_eq!(Mode::default(), Mode::Build);
        assert_eq!(Mode::Build.toggled(), Mode::Plan);
        assert_eq!(Mode::Plan.toggled(), Mode::Build);

        assert_eq!(Mode::Build.label(), "build");
        assert_eq!(Mode::Plan.label(), "plan");

        assert_eq!(Mode::Build.risk_ceiling(), Risk::Write);
        assert_eq!(Mode::Plan.risk_ceiling(), Risk::Read);
        assert!(!Mode::Build.is_read_only());
        assert!(Mode::Plan.is_read_only());
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
        tx.send(Command::Prompt("do the thing".to_string()))
            .expect("send");
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
        tx.send(Command::Prompt("are you there".to_string()))
            .expect("send");
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
        tx.send(Command::Prompt("hello".to_string())).expect("send");
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
        tx.send(Command::Prompt("read it".to_string()))
            .expect("send");
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
        tx.send(Command::Prompt("go".to_string())).expect("send");
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

    // ---- commands ---------------------------------------------------------

    /// Answers at once, and counts everything it was asked to do. Stands in for
    /// a CLI tier, which is the only kind with a session to forget.
    struct Quiet {
        forgotten: std::sync::atomic::AtomicUsize,
        requests: std::sync::atomic::AtomicUsize,
        answer: String,
    }

    impl Quiet {
        fn new(answer: &str) -> Arc<Self> {
            Arc::new(Self {
                forgotten: std::sync::atomic::AtomicUsize::new(0),
                requests: std::sync::atomic::AtomicUsize::new(0),
                answer: answer.to_string(),
            })
        }

        fn forgotten(&self) -> usize {
            self.forgotten.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn requests(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for Quiet {
        fn describe(&self) -> String {
            "quiet".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = events.send(StreamEvent::Text(self.answer.clone()));
            Ok(TurnSummary {
                text: self.answer.clone(),
                stop_reason: Some("end_turn".to_string()),
                ..TurnSummary::default()
            })
        }

        fn forget_session(&self) {
            self.forgotten
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Collect whatever the agent emits, stopping once it goes quiet.
    ///
    /// Most commands answer with a notice and no turn, so there is no terminal
    /// event to wait for.
    async fn collect(rx: &mut UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
        {
            out.push(event);
        }
        out
    }

    fn notices(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Two tiers whose providers can be inspected afterwards.
    fn two_tiers() -> (Vec<Tier>, Arc<Quiet>, Arc<Quiet>) {
        let first = Quiet::new("first answer");
        let second = Quiet::new("second answer");
        let tiers = vec![
            Tier {
                label: "Local (http://10.0.0.1:1234/v1)".to_string(),
                model: "m0".to_string(),
                provider: first.clone(),
                limits: Limits::default(),
            },
            Tier {
                label: "DeepSeek V4 Flash".to_string(),
                model: "m1".to_string(),
                provider: second.clone(),
                limits: Limits::default(),
            },
        ];
        (tiers, first, second)
    }

    fn loop_over(
        dir: &std::path::Path,
        chain: FallbackChain,
    ) -> (UnboundedSender<Command>, UnboundedReceiver<AgentEvent>) {
        spawn(
            config(dir, DEFAULT_MAX_STEPS),
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        )
    }

    fn one_tier(answer: &str) -> (FallbackChain, Arc<Quiet>) {
        let provider = Quiet::new(answer);
        let chain = FallbackChain::new(
            vec![Tier {
                label: "Only".to_string(),
                model: "a-model".to_string(),
                provider: provider.clone(),
                limits: Limits::default(),
            }],
            true,
        )
        .expect("a chain");
        (chain, provider)
    }

    #[tokio::test]
    async fn escalate_moves_down_and_stays_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tiers, _first, second) = two_tiers();
        // Per-turn, so the pin is the only thing that can hold it down.
        let chain = FallbackChain::new(tiers, false).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Escalate).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("DeepSeek")),
            "{:?}",
            notices(&events)
        );

        tx.send(Command::Prompt("hello".to_string())).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t == "second answer")),
            "the hand-picked tier should have answered: {events:?}"
        );
        assert_eq!(second.requests(), 1);
    }

    #[tokio::test]
    async fn escalating_from_the_last_tier_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("only");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Escalate).expect("send");
        let events = collect(&mut rx).await;

        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("nowhere to spill")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn choosing_a_tier_by_name_works_and_a_bad_name_lists_the_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tiers, _first, second) = two_tiers();
        let chain = FallbackChain::new(tiers, true).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetTier(Some("deepseek".to_string())))
            .expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events).iter().any(|n| n.contains("DeepSeek")),
            "{:?}",
            notices(&events)
        );

        tx.send(Command::Prompt("hello".to_string())).expect("send");
        let _ = collect(&mut rx).await;
        assert_eq!(second.requests(), 1, "the named tier should have answered");

        // A name that matches nothing is reported with the chain, not guessed at.
        tx.send(Command::SetTier(Some("nope".to_string())))
            .expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");
        assert!(said.contains("no tier matches"), "{said}");
        assert!(
            said.contains("DeepSeek"),
            "it should say what the chain is: {said}"
        );
    }

    #[tokio::test]
    async fn drop_forgets_only_the_tier_that_is_answering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tiers, first, second) = two_tiers();
        let chain = FallbackChain::new(tiers, true).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Drop).expect("send");
        let events = collect(&mut rx).await;

        assert_eq!(first.forgotten(), 1, "the active tier's session must go");
        assert_eq!(second.forgotten(), 0, "the others are untouched");
        assert!(
            notices(&events).iter().any(|n| n.contains("fresh")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn compact_forgets_every_session_because_the_history_changed_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Long answers, so there is genuinely something to compact.
        let long = "a thorough explanation of the matter ".repeat(12);
        let (tiers, _first, _second) = two_tiers();
        let tiers: Vec<Tier> = tiers
            .into_iter()
            .map(|tier| Tier {
                label: tier.label,
                model: tier.model,
                provider: Quiet::new(&long),
                limits: tier.limits,
            })
            .collect();
        let chain = FallbackChain::new(tiers, true).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        for turn in 0..6 {
            tx.send(Command::Prompt(format!("question {turn}")))
                .expect("send");
            let _ = collect(&mut rx).await;
        }

        tx.send(Command::Compact).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");
        assert!(said.contains("compacted"), "{said}");
        assert!(
            said.contains("dropped"),
            "it should say the sessions went too: {said}"
        );
    }

    #[tokio::test]
    async fn retry_re_runs_the_last_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, provider) = one_tier("an answer");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("do the thing".to_string()))
            .expect("send");
        let _ = collect(&mut rx).await;
        assert_eq!(provider.requests(), 1);

        tx.send(Command::Retry { tier: None }).expect("send");
        let events = collect(&mut rx).await;

        assert_eq!(provider.requests(), 2, "the turn should run again");
        assert!(
            notices(&events).iter().any(|n| n.contains("retrying")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn retry_without_a_turn_says_there_is_nothing_to_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("x");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Retry { tier: None }).expect("send");
        let events = collect(&mut rx).await;

        assert!(
            notices(&events)
                .iter()
                .any(|n| n.contains("no turn to retry")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn sticky_can_be_changed_at_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("x");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetSticky(false)).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events).iter().any(|n| n.contains("top tier")),
            "{:?}",
            notices(&events)
        );

        tx.send(Command::SetSticky(true)).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events)
                .iter()
                .any(|n| n.contains("rest of the session")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn context_reports_what_is_being_sent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("an answer");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("first".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        tx.send(Command::Context).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join("\n");

        assert!(said.contains("conversation:"), "{said}");
        assert!(said.contains("a-model"), "it should name the tier: {said}");
        assert!(said.contains("sticky"), "and the policy: {said}");
    }

    #[tokio::test]
    async fn clear_resets_the_conversation_and_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, provider) = one_tier("an answer");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("something".to_string()))
            .expect("send");
        let _ = collect(&mut rx).await;

        tx.send(Command::Clear).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events).iter().any(|n| n.contains("cleared")),
            "{:?}",
            notices(&events)
        );
        assert!(provider.forgotten() >= 1, "the session must be dropped");

        // The next turn starts a fresh conversation, so the old prompt is gone.
        tx.send(Command::Context).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join("\n");
        assert!(
            said.contains("conversation: 1 message"),
            "only the system prompt should remain: {said}"
        );
    }

    // ---- modes ------------------------------------------------------------

    /// The names offered to the model on a given request.
    fn offered(request: &ChatRequest) -> Vec<String> {
        request.tools.iter().map(|spec| spec.name.clone()).collect()
    }

    #[tokio::test]
    async fn plan_mode_is_not_offered_the_tools_that_can_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![answer("here is a plan")]);
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::SetMode(Mode::Plan)).expect("send");
        tx.send(Command::Prompt("plan the change".to_string()))
            .expect("send");
        let _ = drain(rx).await;

        let names = offered(&provider.request(0));
        for absent in ["write_file", "edit_file", "run_shell"] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} must not be on the table in plan mode: {names:?}"
            );
        }
        for present in ["read_file", "list_dir", "glob", "grep"] {
            assert!(
                names.contains(&present.to_string()),
                "{present} should still be offered: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn plan_mode_refuses_a_write_the_model_asked_for_anyway() {
        // The guarantee, not the courtesy: a model that names a write tool that
        // was never offered — hallucinating it, or carrying a habit from build
        // mode — must not be able to touch the disk.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![
            calls_tool("write_file", r#"{"path":"written.txt","content":"hi"}"#),
            answer("understood"),
        ]);
        let approver = Arc::new(AlwaysApprove::default());
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            approver.clone(),
        );

        tx.send(Command::SetMode(Mode::Plan)).expect("send");
        tx.send(Command::Prompt("write that file".to_string()))
            .expect("send");
        let events = drain(rx).await;

        assert!(
            !dir.path().join("written.txt").exists(),
            "a refused write must not reach the disk"
        );
        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("plan mode is read-only")),
            "the refusal should be visible: {:?}",
            notices(&events)
        );
        assert!(
            approver.asked.lock().expect("lock").is_empty(),
            "there is nothing to approve in a read-only mode, so nobody should be asked"
        );

        // And the model is told why, in terms it can act on.
        let second = provider.request(1);
        let tool_message = second
            .messages
            .iter()
            .find(|message| message.role == crate::session::Role::Tool)
            .expect("the refusal should be fed back");
        assert!(
            tool_message.content.contains("not available in plan mode"),
            "{}",
            tool_message.content
        );
        assert!(
            tool_message.content.contains("reply with the plan"),
            "it should point somewhere useful: {}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn build_mode_still_allows_a_write_after_approval() {
        // The other half of the guarantee: leaving plan mode really does restore
        // the ability to change things.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![
            calls_tool("write_file", r#"{"path":"written.txt","content":"hi"}"#),
            answer("wrote it"),
        ]);
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        // Plan, then straight back to build.
        tx.send(Command::SetMode(Mode::Plan)).expect("send");
        tx.send(Command::SetMode(Mode::Build)).expect("send");
        tx.send(Command::Prompt("write that file".to_string()))
            .expect("send");
        let _ = drain(rx).await;

        assert!(
            dir.path().join("written.txt").exists(),
            "build mode should have written the file"
        );
        assert!(
            offered(&provider.request(0)).contains(&"write_file".to_string()),
            "the write tool should be offered again"
        );
    }

    #[tokio::test]
    async fn switching_mode_swaps_the_system_prompt_the_tier_receives() {
        // The prompt is the first message of the session, so a stale one would
        // leave a read-only turn being told about approval prompts.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![answer("ok"), answer("ok")]);
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("first".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;
        assert!(
            !provider.request(0).messages[0]
                .content
                .contains("PLAN MODE"),
            "build is the default"
        );

        tx.send(Command::SetMode(Mode::Plan)).expect("send");
        tx.send(Command::Prompt("second".to_string()))
            .expect("send");
        let _ = drain_from(&mut rx).await;

        let first_message = &provider.request(1).messages[0];
        assert_eq!(first_message.role, crate::session::Role::System);
        assert!(
            first_message.content.contains("PLAN MODE"),
            "the system prompt should have changed with the mode: {}",
            first_message.content
        );
    }

    #[tokio::test]
    async fn switching_mode_says_which_mode_is_now_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(Vec::new());
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::SetMode(Mode::Plan)).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");
        assert!(said.contains("nothing will be changed"), "{said}");

        tx.send(Command::SetMode(Mode::Build)).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");
        assert!(said.contains("write files"), "{said}");
    }
}
