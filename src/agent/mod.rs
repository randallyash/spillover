//! The agent loop: stream a turn, run any tools the model asks for, feed the
//! results back, and repeat until the model answers without asking for one.

pub mod approval;
pub mod consult;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::agent::approval::{Approver, Decision};
use crate::agent::tools::{Registry, Risk, ToolOutcome};
use crate::config::OnStuck;
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

/// Stops a turn that is already running.
///
/// Deliberately not a `Command`: the command channel is read by the very loop
/// that is *awaiting* the turn, so a cancel sent down it would sit in the queue
/// until the turn it was meant to stop had already finished. This is shared state
/// instead, which the turn watches directly.
#[derive(Clone, Debug)]
pub struct Canceller {
    flag: Arc<watch::Sender<bool>>,
    watcher: watch::Receiver<bool>,
}

impl Default for Canceller {
    fn default() -> Self {
        let (flag, watcher) = watch::channel(false);
        Self {
            flag: Arc::new(flag),
            watcher,
        }
    }
}

impl Canceller {
    /// Ask the running turn to stop. Does nothing when nothing is running.
    pub fn cancel(&self) {
        let _ = self.flag.send(true);
    }

    /// Whether a cancel is outstanding.
    pub fn is_cancelled(&self) -> bool {
        *self.flag.borrow()
    }

    /// Clear the flag before a new turn, so a cancel cannot leak into it.
    fn arm(&self) {
        let _ = self.flag.send(false);
    }

    fn watcher(&self) -> watch::Receiver<bool> {
        self.watcher.clone()
    }
}

/// Resolve when a cancel is asked for.
///
/// If every sender is gone this never resolves, which is what the turn wants: no
/// signal, nothing to stop for.
async fn cancelled(mut watcher: watch::Receiver<bool>) {
    loop {
        if *watcher.borrow_and_update() {
            return;
        }
        if watcher.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

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
    /// Ask the next tier about the next stall, and keep the driver.
    ///
    /// A one-shot rather than a mode: "try a consult here" is a different
    /// intention from "consult from now on", and the second one has a command of
    /// its own.
    Consult,
    /// Choose the stuck policy for the rest of the session. `None` hands the
    /// choice back to each tier's own, which is the only way to return to a
    /// chain whose tiers differ.
    SetOnStuck(Option<OnStuck>),
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
    /// Tokens a tier spent on this turn.
    ///
    /// The single accounting path, and deliberately separate from the events
    /// that say what *happened*: every request is billed, including the ones
    /// behind an answer that was thrown away, so the money has to be reported
    /// whether the attempt answered, was abandoned, or was stopped. Emitting it
    /// from one place is what keeps it from being counted twice or not at all.
    Spent { tier: String, usage: Usage },
    /// The turn ended normally.
    Finished { stop_reason: Option<String> },
    /// The user stopped the turn. The tier is named so the transcript can say
    /// where it was stopped, not just that it was.
    Cancelled { tier: String },
    /// The driver was stuck and asked the tier below it a narrow question,
    /// rather than handing the turn over.
    Consulted {
        driver: String,
        consultant: String,
        /// One line for the transcript: what the consult was about.
        about: String,
        /// Which consult this was within the turn, and the cap.
        nth: u32,
        of: u32,
    },
    /// Every tier was tried and none of them produced an answer.
    Exhausted { reason: String },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub max_steps: usize,
    /// How a running turn is stopped from outside.
    pub cancel: Canceller,
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

        Command::Consult => {
            // Refused now rather than at the stall: a request that could never
            // be honoured is worth saying so about while the user is looking.
            if !state.chain.can_consult() {
                let _ = events.send(AgentEvent::Notice(format!(
                    "{} is the last tier, so there is nobody to consult — /escalate hands it \
                     over instead",
                    short(&state.chain.active().label)
                )));
            } else {
                state.chain.consult_next_stall();
                let consultant = state
                    .chain
                    .consultant()
                    .map(|tier| short(&tier.label).to_string())
                    .unwrap_or_default();
                let driver = short(&state.chain.active().label).to_string();
                let _ = events.send(AgentEvent::Notice(format!(
                    "the next stall goes to {consultant} as one question, and {driver} keeps \
                     the turn — once"
                )));
            }
        }

        Command::SetOnStuck(None) => {
            state.chain.set_on_stuck(None);
            // Says what going back actually means, because it is not one policy:
            // each tier has its own, and that is the reason to go back.
            let _ = events.send(AgentEvent::Notice(format!(
                "back to the configured policies — {}",
                state.chain.describe_own_policies()
            )));
        }

        Command::SetOnStuck(Some(policy)) => {
            state.chain.set_on_stuck(Some(policy));
            let _ = events.send(AgentEvent::Notice(match policy {
                OnStuck::Consult => {
                    let consultant = state
                        .chain
                        .consultant()
                        .map(|tier| short(&tier.label).to_string())
                        .unwrap_or_default();
                    if state.chain.can_consult() {
                        format!(
                            "from now on a stuck tier asks {consultant} one question and keeps \
                             the turn, rather than handing it over — /on-stuck escalate to go back"
                        )
                    } else {
                        // The policy is set, but this chain is one tier deep, so
                        // it can never take effect. Saying so beats a setting
                        // that silently does nothing.
                        "consult needs a tier below to ask, and this chain has one tier, so it \
                         cannot take effect"
                            .to_string()
                    }
                }
                OnStuck::Escalate => {
                    "from now on a stuck tier hands the turn to the next one, as configured"
                        .to_string()
                }
            }));
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

/// A tier's name without its address, for anything a person reads.
///
/// The address belongs in the session panel; repeating it in every notice is how
/// a one-line message turns into three wrapped ones.
fn short(label: &str) -> &str {
    crate::fallback::tier_name(label)
}

/// What one tier made of a turn, and what finding out cost.
///
/// The usage is carried out of the attempt rather than reported from inside it,
/// because the money is owed whichever way the attempt went: a tier that looped
/// five times and was abandoned has been billed for all five requests. The
/// caller reports it through one path so nothing is counted twice.
enum Attempt {
    Answered {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
    Stuck(StuckReason, Option<Usage>),
    /// The user stopped it. Kept apart from `Stuck` because it must not spill to
    /// the next tier: nobody asked for a different model.
    Cancelled(Option<Usage>),
}

/// How a tool call ended.
enum ToolRun {
    Done(ToolOutcome),
    /// Stopped by the user while it was running.
    Cancelled,
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
    // A cancel from a previous turn must not stop this one.
    config.cancel.arm();
    // Consults spent within this turn, and what each one said. Both are
    // per-turn: the cap exists to bound one stuck episode, and the answers are
    // only relevant to the consult that follows them.
    let mut consulted: Vec<consult::Previous> = Vec::new();

    loop {
        let (label, outcome) = {
            let tier = chain.active();
            (
                tier.label.clone(),
                try_tier(config, tier, mode, registry, approver, events, session).await,
            )
        };

        // Reported before the outcome is judged. Whatever this tier spent is
        // owed however the attempt ended — answered, abandoned, or stopped — so
        // it is sent here rather than inside any one branch, where a later edit
        // could silently drop one of them. The UI sums these into the turn's
        // total; this side only says who spent what.
        if let Some(spent) = usage_of(&outcome) {
            let _ = events.send(AgentEvent::Spent {
                tier: label.clone(),
                usage: spent,
            });
        }

        match outcome {
            Attempt::Answered { stop_reason, .. } => {
                let _ = events.send(AgentEvent::Finished { stop_reason });
                return;
            }
            Attempt::Cancelled(_) => {
                // No rollback and no session forgotten. Unlike a stalled tier,
                // a cancelled one is not being abandoned: the conversation up to
                // this point is real work, and the same tier will carry on from
                // it. The half-generated answer was never pushed to the session
                // (only the finished ones are), so there is nothing to undo.
                let _ = events.send(AgentEvent::Cancelled { tier: label });
                return;
            }
            Attempt::Stuck(reason, _) => {
                let from = label;
                // Read out of the driver before it can be borrowed mutably below.
                // Taken rather than read, so a one-shot request applies to one
                // stall and cannot quietly become the policy.
                let asked_for = chain.take_consult_request();
                let cap = chain.active().consults_per_turn;
                let wants_consult = asked_for || chain.consults_when_stuck();
                if asked_for && !chain.can_consult() {
                    let _ = events.send(AgentEvent::Notice(
                        "this is the last tier, so there is nobody to consult — handing the turn \
                         over instead"
                            .to_string(),
                    ));
                }

                // A consult keeps the driver in charge, so it is tried before
                // handing the turn over. It is only tried when it can succeed:
                // the tier has to ask for it, there has to be someone below to
                // ask, and the cap has to have budget left. Any failure falls
                // through to escalating, which is the path that always
                // terminates.
                if wants_consult && (consulted.len() as u32) < cap && chain.consultant().is_some() {
                    // The evidence is what the failed attempt added, read before
                    // it is discarded. This is the plan's central point: the
                    // question is built from what spill already holds, and the
                    // driver contributes no prose of its own.
                    let evidence: Vec<ChatMessage> = session.messages()[checkpoint..].to_vec();
                    let goal = goal_before(session, checkpoint);

                    if let Some(answer) =
                        try_consult(config, chain, &goal, &reason, &evidence, &consulted, events)
                            .await
                    {
                        let consultant = chain
                            .consultant()
                            .map(|tier| tier.label.clone())
                            .unwrap_or_default();

                        // The consult is another request that was billed, so it
                        // goes through the same accounting as an attempt.
                        if let Some(spent) = answer.usage {
                            let _ = events.send(AgentEvent::Spent {
                                tier: consultant.clone(),
                                usage: spent,
                            });
                        }

                        // The failed attempt goes, exactly as it would for an
                        // escalation: it was the poison. What replaces it is the
                        // answer, which is short and is the only new context.
                        session.truncate(checkpoint);
                        // The driver's CLI session still holds the output just
                        // discarded, so it must not be resumed either.
                        chain.active().provider.forget_session();
                        session.push(ChatMessage::system(consult::injection(
                            &consultant,
                            &answer.text,
                        )));

                        consulted.push(consult::Previous {
                            consultant: consultant.clone(),
                            answer: answer.text.clone(),
                        });
                        let _ = events.send(AgentEvent::Consulted {
                            driver: from.clone(),
                            consultant,
                            about: answer.about,
                            nth: consulted.len() as u32,
                            of: cap,
                        });

                        // Back to the same tier, with the answer in hand.
                        continue;
                    }
                }

                // Throw the failed attempt away before another model reads it.
                session.truncate(checkpoint);
                // A CLI tier may be holding a session that contains the output
                // just discarded, so it must not be resumed.
                chain.active().provider.forget_session();

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

/// What an attempt cost, if it reported anything.
fn usage_of(attempt: &Attempt) -> Option<Usage> {
    match attempt {
        Attempt::Answered { usage, .. } | Attempt::Stuck(_, usage) | Attempt::Cancelled(usage) => {
            *usage
        }
    }
}

/// The user's own words for this turn.
///
/// Found by searching back for the user turn rather than assuming it sits at
/// `checkpoint - 1`, so a later change to how a turn is opened cannot silently
/// start sending something else as the goal.
fn goal_before(session: &Session, checkpoint: usize) -> String {
    session.messages()[..checkpoint]
        .iter()
        .rev()
        .find(|message| message.role == crate::session::Role::User)
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

/// What a consultant produced, plus what it cost.
struct Answer {
    text: String,
    /// One line for the transcript: why the driver was stuck, which is what the
    /// consult was about. It does not restate who asked whom — the transcript
    /// line already carries both names, and saying them three times made the
    /// message wrap across three rows.
    about: String,
    usage: Option<Usage>,
}

/// Put the stuck driver's question to the tier below it, as a fresh call.
///
/// `None` when the consult itself failed or was stopped, which the caller turns
/// into an escalation: a consult that cannot complete must not strand the turn.
#[allow(clippy::too_many_arguments)]
async fn try_consult(
    config: &AgentConfig,
    chain: &FallbackChain,
    goal: &str,
    reason: &StuckReason,
    evidence: &[ChatMessage],
    previous: &[consult::Previous],
    events: &UnboundedSender<AgentEvent>,
) -> Option<Answer> {
    let consultant = chain.consultant()?;
    let question = consult::build(goal, reason, evidence, previous);

    // No tools. For an `openai` tier that is what makes "the answer is prose"
    // true rather than hoped for: it has nothing to call, so it must answer, and
    // the call is one round trip rather than a tool loop. A `cli` tier runs its
    // own harness and cannot be stripped of its tools this way, which is why the
    // question asks it plainly not to act.
    let request = ChatRequest {
        model: consultant.model.clone(),
        messages: vec![ChatMessage::user(question.question.clone())],
        tools: Vec::new(),
    };

    let mut watchdog = Watchdog::new(&consultant.limits);
    // The consultant's answer is not the driver's answer, so its text must not
    // reach the transcript as though the driver had said it. A throwaway channel
    // is how that is guaranteed: the watchdog still sees every frame, so a
    // looping consultant is still caught, but the UI hears nothing until the
    // call is over and reported as a consult.
    let (discard, _unused) = mpsc::unbounded_channel();

    let (result, observed) = stream_turn(
        &consultant.provider,
        request,
        &discard,
        &mut watchdog,
        &config.cancel,
    )
    .await;

    let summary = match result {
        Ok(summary) => summary,
        Err(reason) => {
            // A consult that could not finish was still billed for what it did.
            // Reported before giving up, so the cost is not silently lost along
            // with the answer — and said out loud, because a silent fallback
            // would make consult look like it simply did nothing.
            if let Some(usage) = observed {
                let _ = events.send(AgentEvent::Spent {
                    tier: consultant.label.clone(),
                    usage,
                });
            }
            let _ = events.send(AgentEvent::Notice(format!(
                "{} could not be consulted ({}) — carrying on as if it had not been asked",
                consultant.label,
                reason.summary()
            )));
            return None;
        }
    };

    let text = summary.text.trim().to_string();
    if text.is_empty() {
        // An empty answer is not an answer: injecting it would add nothing and
        // cost context, so the turn escalates instead.
        let _ = events.send(AgentEvent::Notice(format!(
            "{} was consulted but said nothing — carrying on as if it had not been asked",
            consultant.label
        )));
        return None;
    }

    Some(Answer {
        text,
        about: reason.summary(),
        // One figure, never both: the finished response's own total if it gave
        // one, otherwise what arrived before it ended.
        usage: summary.usage.or(observed),
    })
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
    // Every request this tier makes is billed, one per step, so the total is
    // carried across the loop rather than read off the last step.
    let mut spent: Option<Usage> = None;

    for _step in 0..config.max_steps {
        // Stopped between steps, so a cancel that arrives while tools are being
        // run does not buy another round trip.
        if config.cancel.is_cancelled() {
            return Attempt::Cancelled(spent);
        }

        let request = ChatRequest {
            model: tier.model.clone(),
            messages: session.messages().to_vec(),
            tools: tools.clone(),
        };

        let (result, observed) = stream_turn(
            &tier.provider,
            request,
            events,
            &mut watchdog,
            &config.cancel,
        )
        .await;

        // Folded in before anything else can return, so a request that produced
        // a tool call — or one whose output is about to be discarded — still
        // counts what it cost. Exactly one figure is taken per request, never
        // both, so a request cannot be counted twice.
        let summary = match result {
            Ok(summary) => {
                // The finished response's own total is the better number; what
                // was observed on the way is the fallback for a provider that
                // reported as it went.
                crate::provider::accumulate(&mut spent, summary.usage.or(observed));
                summary
            }
            Err(StuckReason::Cancelled) => {
                // Nothing finished, so whatever the tier reported before the
                // stop is what it spent — and it is owed either way.
                crate::provider::accumulate(&mut spent, observed);
                return Attempt::Cancelled(spent);
            }
            Err(reason) => {
                crate::provider::accumulate(&mut spent, observed);
                return Attempt::Stuck(reason, spent);
            }
        };

        session.push(ChatMessage::assistant(
            summary.text.clone(),
            summary.tool_calls.clone(),
        ));

        if summary.tool_calls.is_empty() {
            return Attempt::Answered {
                stop_reason: summary.stop_reason.clone(),
                usage: spent,
            };
        }

        for call in summary.tool_calls.clone() {
            let outcome = run_tool(
                mode,
                registry,
                approver,
                &config.workspace,
                &call,
                events,
                &config.cancel,
            )
            .await;

            let outcome = match outcome {
                ToolRun::Done(outcome) => outcome,
                ToolRun::Cancelled => {
                    // The assistant message above is already in the session and
                    // names this call, and a provider rejects a call with no
                    // result. So the cancellation is recorded as the result
                    // rather than left as a gap: the conversation stays valid,
                    // and the next turn knows what became of it.
                    session.push(ChatMessage::tool_result(
                        call.id.clone(),
                        "the user cancelled before this finished.",
                    ));
                    return Attempt::Cancelled(spent);
                }
            };

            if let Some(reason) = progress.record(&call.name, &call.arguments, !outcome.is_error) {
                return Attempt::Stuck(reason, spent);
            }
            // The result goes back even when it is an error or a refusal, so the
            // model can see what happened instead of retrying blindly.
            session.push(ChatMessage::tool_result(call.id.clone(), outcome.content));
        }
    }

    Attempt::Stuck(
        StuckReason::StepLimit {
            steps: config.max_steps,
        },
        spent,
    )
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
    cancel: &Canceller,
) -> (Result<TurnSummary, StuckReason>, Option<Usage>) {
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let provider = provider.clone();
    let mut task = tokio::spawn(async move { provider.stream(request, delta_tx).await });

    // Usage the tier reported before the attempt ended, whichever way it ended.
    // A killed stream has still been billed, and the figure often arrives before
    // the answer does, so this is kept rather than only read off a finished
    // response.
    let mut observed: Option<Usage> = None;

    let outcome = loop {
        let allowance = watchdog.allowance();

        tokio::select! {
            biased;
            // Ahead of the stream, so a cancel is felt on the next frame rather
            // than after it.
            _ = cancelled(cancel.watcher()) => break Err(StuckReason::Cancelled),
            joined = &mut task => {
                // A fast response can finish before this loop gets to its
                // queued frames, so drain them first: otherwise a looping
                // answer that arrived in one burst would look healthy.
                let mut tripped = None;
                while let Ok(event) = delta_rx.try_recv() {
                    if let Some(reason) = observe(event, events, watchdog, &mut observed) {
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
                        if let Some(reason) = observe(event, events, watchdog, &mut observed) {
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

    (outcome, observed)
}

/// Feed one stream event to the watchdog, forwarding text to the UI.
///
/// `observed` collects whatever the tier said the request had cost. It is filled
/// in as the frames arrive rather than read off the finished response, because
/// an attempt that is abandoned — a loop, a stall, a cancel — never produces a
/// finished response, and its bills are owed all the same.
fn observe(
    event: StreamEvent,
    events: &UnboundedSender<AgentEvent>,
    watchdog: &mut Watchdog,
    observed: &mut Option<Usage>,
) -> Option<StuckReason> {
    match event {
        StreamEvent::Activity => {
            watchdog.note_activity();
            None
        }
        StreamEvent::Usage(usage) => {
            // A frame saying what this cost is a frame: the tier is alive.
            watchdog.note_activity();
            // Last one wins. These restate the same figure rather than adding
            // up, so keeping the latest is right and summing would multiply it.
            *observed = Some(usage);
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
    cancel: &Canceller,
) -> ToolRun {
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
        return ToolRun::Done(ToolOutcome::error(message));
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
        return ToolRun::Done(ToolOutcome::error(message));
    }

    let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            let message = format!(
                "the arguments for {} were not valid JSON ({error}). Arguments received: {}",
                call.name, call.arguments
            );
            let _ = events.send(AgentEvent::Notice(message.clone()));
            return ToolRun::Done(ToolOutcome::error(message));
        }
    };

    let preview = tool.preview(&arguments, workspace).await;

    // Only tools that can change something need permission; the short-circuit
    // keeps read-only tools from ever reaching the approver.
    //
    // The wait is raced against the cancel as well. In practice Esc over a modal
    // denies it rather than cancelling, so this is belt and braces: without it,
    // a cancel raised while a prompt was open would wait for an answer that is
    // never coming.
    if tool.risk() == Risk::Write {
        let decision = tokio::select! {
            biased;
            _ = cancelled(cancel.watcher()) => return ToolRun::Cancelled,
            decision = approver.decide(&call.name, &preview) => decision,
        };
        if decision == Decision::Deny {
            let _ = events.send(AgentEvent::Denied {
                tool: call.name.clone(),
            });
            return ToolRun::Done(ToolOutcome::error(format!(
                "the user declined to run {}. Do not repeat it; ask what they would prefer instead.",
                call.name
            )));
        }
    }

    let _ = events.send(AgentEvent::ToolStarted {
        name: call.name.clone(),
        preview: preview.clone(),
    });

    // The tool races the cancel, so a long build is stopped rather than waited
    // out. Both the shell and the CLI tiers spawn their child with
    // `kill_on_drop`, so dropping this future takes the process with it instead
    // of leaving it running unnoticed.
    let outcome = tokio::select! {
        biased;
        _ = cancelled(cancel.watcher()) => return ToolRun::Cancelled,
        outcome = tool.run(&arguments, workspace) => outcome,
    };

    let _ = events.send(AgentEvent::ToolFinished {
        name: call.name.clone(),
        ok: !outcome.is_error,
        summary: first_line(&outcome.content),
    });

    ToolRun::Done(outcome)
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
            cancel: Canceller::default(),
        }
    }

    /// The same, with a canceller the test can hold.
    fn config_with_cancel(
        workspace: &std::path::Path,
        max_steps: usize,
    ) -> (AgentConfig, Canceller) {
        let cancel = Canceller::default();
        let config = AgentConfig {
            workspace: workspace.to_path_buf(),
            max_steps,
            cancel: cancel.clone(),
        };
        (config, cancel)
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
            .map(|(index, (provider, limits))| {
                Tier::new(
                    format!("Tier {index}"),
                    format!("model-{index}"),
                    provider,
                    limits,
                )
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

    /// A turn that uses a tool, with a token cost on each step.
    fn step_one() -> TurnSummary {
        TurnSummary {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"note.txt"}"#.to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            usage: Some(Usage {
                prompt_tokens: 1_000,
                completion_tokens: 10,
                ..Default::default()
            }),
            session_id: None,
        }
    }

    fn step_two() -> TurnSummary {
        TurnSummary {
            text: "read it".to_string(),
            stop_reason: Some("end_turn".to_string()),
            usage: Some(Usage {
                prompt_tokens: 2_000,
                completion_tokens: 20,
                ..Default::default()
            }),
            ..TurnSummary::default()
        }
    }

    #[tokio::test]
    async fn every_request_in_a_turn_is_counted_not_just_the_last() {
        // A turn makes one request per tool call and every one of them is
        // billed. Reporting only the final step understated a turn in
        // proportion to how much work it did — a turn that read four files made
        // five requests and reported one — which is the worst direction for a
        // cost figure to be wrong in.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "contents").expect("write");

        let (events, provider) = run_one(
            vec![step_one(), step_two()],
            Arc::new(AlwaysApprove::default()),
            dir.path(),
            "read note.txt",
        )
        .await;

        assert_eq!(provider.request_count(), 2, "two steps, two requests");

        let spent: Vec<Usage> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Spent { usage, .. } => Some(*usage),
                _ => None,
            })
            .collect();

        // One report per attempt rather than per step: the accumulation is the
        // agent's job, and a report per step would push a bar per request into
        // the cost graph.
        assert_eq!(spent.len(), 1, "one attempt, one report: {events:?}");
        assert_eq!(
            spent[0].prompt_tokens, 3_000,
            "both requests are billed, so both must be counted"
        );
        assert_eq!(spent[0].completion_tokens, 30);
    }

    #[tokio::test]
    async fn an_abandoned_attempt_is_counted_too() {
        // A tier that is thrown away was still billed for the requests it
        // finished. Dropping those would make a spill — the most interesting
        // cost event there is — the least visible one.
        //
        // The failure used here is the step limit, because that is one of the
        // cases where the bills are actually knowable: usage arrives with a
        // response, so an attempt whose *stream* was killed mid-flight (a
        // repetition loop, a transport failure) never reported any and there is
        // nothing to count. That gap is inherent to how providers bill, not
        // something this code can close.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "contents").expect("write");

        let stubborn = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            // Keeps asking for the same tool, so the step limit is what ends it.
            fallback: TurnSummary {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"note.txt"}"#.to_string(),
                }],
                stop_reason: Some("tool_calls".to_string()),
                usage: Some(Usage {
                    prompt_tokens: 1_000,
                    completion_tokens: 5,
                    ..Default::default()
                }),
                session_id: None,
            },
            fail_with: None,
        });
        let healthy = ScriptedProvider::new(vec![step_two()]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (stubborn.clone(), Limits::default()),
            (healthy, Limits::default()),
        ];
        let (tx, mut rx) = spawn(
            config(dir.path(), 2),
            chain_of(tiers),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "the tier should have been abandoned: {events:?}"
        );
        assert_eq!(stubborn.request_count(), 2, "it used both its steps");

        let spent: Vec<(String, Usage)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Spent { tier, usage } => Some((tier.clone(), *usage)),
                _ => None,
            })
            .collect();

        assert_eq!(spent.len(), 2, "both tiers spent something: {events:?}");
        assert_eq!(spent[0].0, "Tier 0");
        assert_eq!(
            spent[0].1.prompt_tokens, 2_000,
            "both of the abandoned attempt's requests are owed"
        );
        assert_eq!(spent[1].0, "Tier 1");
        assert_eq!(spent[1].1.prompt_tokens, 2_000);
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

        let spent: Vec<Usage> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Spent { usage, .. } => Some(*usage),
                _ => None,
            })
            .collect();
        assert_eq!(spent.len(), 1, "one attempt, one spend: {events:?}");
        assert_eq!(spent[0].completion_tokens, 4);

        match events.last() {
            Some(AgentEvent::Finished { stop_reason }) => {
                assert_eq!(stop_reason.as_deref(), Some("length"));
            }
            other => panic!("expected a finished turn, got {other:?}"),
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
            Tier::new(
                "Local (http://10.0.0.1:1234/v1)".to_string(),
                "m0".to_string(),
                first.clone(),
                Limits::default(),
            ),
            Tier::new(
                "DeepSeek V4 Flash".to_string(),
                "m1".to_string(),
                second.clone(),
                Limits::default(),
            ),
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
            vec![Tier::new(
                "Only".to_string(),
                "a-model".to_string(),
                provider.clone(),
                Limits::default(),
            )],
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
            .map(|tier| Tier::new(tier.label, tier.model, Quiet::new(&long), tier.limits))
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
    async fn consult_asks_once_and_leaves_the_policy_alone() {
        // The whole point of a one-shot: trying consult must not silently turn
        // it on for the rest of the session, which is what `/on-stuck` is for.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Consult).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("the next stall")),
            "{:?}",
            notices(&events)
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(
            consulted(&events).len(),
            1,
            "the request should have been honoured: {events:?}"
        );
        assert_eq!(driver.request_count(), 2, "the driver kept the turn");
        assert_eq!(consultant.request_count(), 1);
    }

    #[tokio::test]
    async fn a_consult_request_is_not_honoured_twice() {
        // Taken, not read. A request that survived would make every later stall
        // consult, which is a policy change wearing a one-shot's clothes.
        let dir = tempfile::tempdir().expect("tempdir");

        // One consult wanted, but the driver loops past it so a second stall
        // happens. The second must escalate, not consult again.
        let driver = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            fallback: looping_answer(),
            fail_with: None,
        });
        let consultant = ScriptedProvider::new(vec![answer("advice one")]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        // Configured to escalate, so only the one-shot can produce a consult.
        first.on_stuck = OnStuck::Escalate;
        first.consults_per_turn = 5;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant.clone(),
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Consult).expect("send");
        let _ = collect(&mut rx).await;
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(
            consulted(&events).len(),
            1,
            "one request means one consult, however many stalls follow: {events:?}"
        );
        // Twice: once as the consultant, and once as the tier the second stall
        // escalated into. Only the first was a consult, which is what the count
        // of `Consulted` events above is measuring.
        assert_eq!(consultant.request_count(), 2);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "the second stall should escalate: {events:?}"
        );
    }

    #[tokio::test]
    async fn on_stuck_changes_the_policy_for_the_rest_of_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetOnStuck(Some(OnStuck::Consult)))
            .expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");
        assert!(said.contains("from now on"), "{said}");
        assert!(
            said.contains("keeps the turn"),
            "it should say what it means: {said}"
        );

        // No `/consult` needed: the policy alone produces the consult.
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(consulted(&events).len(), 1, "{events:?}");
        assert_eq!(driver.request_count(), 2);
        assert_eq!(consultant.request_count(), 1);
    }

    #[tokio::test]
    async fn on_stuck_auto_hands_the_choice_back_and_says_what_that_means() {
        // The route back. Says what going back actually means, because it is not
        // one policy: each tier has its own, and that is the reason to go back.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _driver, _consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        // Flatten both tiers, so going back is visible.
        tx.send(Command::SetOnStuck(Some(OnStuck::Escalate)))
            .expect("send");
        let _ = collect(&mut rx).await;

        tx.send(Command::SetOnStuck(None)).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");

        assert!(said.contains("back to the configured"), "{said}");
        assert!(
            said.contains("Local consult"),
            "it should name each tier's own policy: {said}"
        );
        assert!(said.contains("DeepSeek escalate"), "{said}");
    }

    #[tokio::test]
    async fn on_stuck_auto_restores_the_tier_that_consults() {
        // The behaviour that matters, not just the message: after going back, a
        // stall consults again because the tier's own config says so.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetOnStuck(Some(OnStuck::Escalate)))
            .expect("send");
        let _ = collect(&mut rx).await;
        tx.send(Command::SetOnStuck(None)).expect("send");
        let _ = collect(&mut rx).await;

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(
            consulted(&events).len(),
            1,
            "the tier's own policy is back in force: {events:?}"
        );
        assert_eq!(driver.request_count(), 2);
        assert_eq!(consultant.request_count(), 1);
    }

    #[tokio::test]
    async fn on_stuck_escalate_puts_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _driver, _consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetOnStuck(Some(OnStuck::Escalate)))
            .expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("hands the turn to the next one")),
            "{:?}",
            notices(&events)
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            consulted(&events).is_empty(),
            "the policy is escalate, so nothing consults: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "{events:?}"
        );
        // The count says nothing here: the escalated tier *is* the consultant,
        // so it would be asked either way. The empty `consulted` above is the
        // assertion that matters.
    }

    #[tokio::test]
    async fn a_policy_notice_uses_short_names_not_addresses() {
        // The address belongs in the session panel. Repeating it in every notice
        // is how a one-line message becomes three wrapped ones, which is what
        // happened the first time this was written.
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = ScriptedProvider::new(Vec::new());
        let consultant = ScriptedProvider::new(Vec::new());

        let tiers = vec![
            Tier::new(
                "Local (http://127.0.0.1:8735/v1)".to_string(),
                "m0".to_string(),
                driver,
                Limits::default(),
            ),
            Tier::new(
                "Frontier (http://127.0.0.1:8736/v1)".to_string(),
                "m1".to_string(),
                consultant,
                Limits::default(),
            ),
        ];
        let chain = FallbackChain::new(tiers, true).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Consult).expect("send");
        let events = collect(&mut rx).await;
        tx.send(Command::SetOnStuck(Some(OnStuck::Consult)))
            .expect("send");
        let events2 = collect(&mut rx).await;

        let said = format!(
            "{} {}",
            notices(&events).join(" "),
            notices(&events2).join(" ")
        );
        assert!(said.contains("Frontier"), "{said}");
        assert!(
            !said.contains("http://"),
            "an address has no place in a notice: {said}"
        );
    }

    #[tokio::test]
    async fn consult_says_so_when_there_is_nobody_to_ask() {
        // Refused while the user is looking, rather than silently doing nothing
        // and leaving them to wonder why no consult happened.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("only");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Consult).expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");

        assert!(said.contains("last tier"), "{said}");
        assert!(said.contains("nobody to consult"), "{said}");
    }

    #[tokio::test]
    async fn on_stuck_consult_warns_when_the_chain_is_one_tier_deep() {
        // The policy is remembered, but it cannot ever take effect, and a
        // setting that silently does nothing is worse than one that says so.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _provider) = one_tier("only");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::SetOnStuck(Some(OnStuck::Consult)))
            .expect("send");
        let events = collect(&mut rx).await;
        let said = notices(&events).join(" ");

        assert!(said.contains("cannot take effect"), "{said}");
        assert!(said.contains("one tier"), "{said}");
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

    // ---- cancelling -------------------------------------------------------

    /// Talks forever without repeating itself, so the watchdog never trips and
    /// the only thing that can end the turn is a cancel.
    struct Endless {
        streamed: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Provider for Endless {
        fn describe(&self) -> String {
            "endless".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            loop {
                let n = self
                    .streamed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Distinct every time: identical lines would trip the repetition
                // detector and end the turn for the wrong reason.
                let _ = events.send(StreamEvent::Text(format!("chunk number {n}\n")));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    /// Never answers an approval, so a turn sits inside a tool until cancelled.
    struct NeverApproves;

    #[async_trait]
    impl Approver for NeverApproves {
        async fn decide(&self, _tool: &str, _preview: &str) -> Decision {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_and_is_not_spilled_to_the_next_tier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let second = Quiet::new("second answer");
        let (config, cancel) = config_with_cancel(dir.path(), DEFAULT_MAX_STEPS);
        let chain = FallbackChain::new(
            vec![
                Tier::new(
                    "Endless".to_string(),
                    "m".to_string(),
                    Arc::new(Endless {
                        streamed: std::sync::atomic::AtomicUsize::new(0),
                    }),
                    Limits::default(),
                ),
                Tier::new(
                    "DeepSeek".to_string(),
                    "m".to_string(),
                    second.clone(),
                    Limits::default(),
                ),
            ],
            true,
        )
        .expect("a chain");
        let (tx, mut rx) = spawn(
            config,
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        // Let it start producing, then stop it.
        tokio::time::sleep(Duration::from_millis(80)).await;
        cancel.cancel();

        let events = collect(&mut rx).await;
        let terminal = events.last().expect("a terminal event");

        assert!(
            matches!(terminal, AgentEvent::Cancelled { tier } if tier == "Endless"),
            "the turn should end as cancelled, not finished or spilled: {terminal:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "a cancel is not a reason to change models: {events:?}"
        );
        assert_eq!(
            second.requests(),
            0,
            "the next tier must not be asked to take over"
        );
    }

    #[tokio::test]
    async fn a_cancel_does_not_leak_into_the_next_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![answer("first"), answer("second")]);
        let (config, cancel) = config_with_cancel(dir.path(), DEFAULT_MAX_STEPS);
        let (tx, mut rx) = spawn(
            config,
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        // Cancelled while nothing was running: the flag must be cleared before
        // the next turn, or that turn would stop the instant it began.
        cancel.cancel();
        tx.send(Command::Prompt("go".to_string())).expect("send");

        let events = drain_from(&mut rx).await;
        assert!(
            matches!(events.last(), Some(AgentEvent::Finished { .. })),
            "the turn should have run normally: {:?}",
            events.last()
        );
    }

    #[tokio::test]
    async fn a_cancel_inside_a_tool_answers_its_call_so_the_history_stays_valid() {
        // A provider rejects an assistant message whose tool calls have no
        // results, so a cancel that lands mid-tool still has to record what
        // became of the call.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![
            calls_tool("write_file", r#"{"path":"new.txt","content":"hi"}"#),
            answer("understood"),
        ]);
        let (config, cancel) = config_with_cancel(dir.path(), DEFAULT_MAX_STEPS);
        let (tx, mut rx) = spawn(
            config,
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            // Stuck waiting for an approval that never comes, which is the
            // earliest a turn can be caught inside a tool.
            Arc::new(NeverApproves),
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();

        let events = collect(&mut rx).await;
        assert!(
            matches!(events.last(), Some(AgentEvent::Cancelled { .. })),
            "it should end as cancelled: {:?}",
            events.last()
        );
        assert!(
            !dir.path().join("new.txt").exists(),
            "nothing may have been written"
        );

        // The next turn must be sendable: every call answered by a result.
        tx.send(Command::Prompt("again".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;

        let last = provider
            .requests
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .expect("a second request");
        let calls: Vec<String> = last
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone()))
            .collect();
        let results: Vec<String> = last
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id.clone())
            .collect();

        assert!(
            !calls.is_empty(),
            "the cancelled call should still be there"
        );
        for id in &calls {
            assert!(
                results.contains(id),
                "call {id} has no result, so this request would be rejected"
            );
        }
    }
    // ---- usage reported before a stream ends ------------------------------

    /// Reports what it has spent, then goes quiet for good.
    ///
    /// Stands in for a stream that is killed before it can report a total: a
    /// loop, a stall, a cancel. The request was billed and the figure arrived,
    /// so losing it would be this program's error rather than the provider's.
    struct ReportsThenHangs {
        usage: Usage,
    }

    #[async_trait]
    impl Provider for ReportsThenHangs {
        fn describe(&self) -> String {
            "reports then hangs".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            let _ = events.send(StreamEvent::Usage(self.usage));
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    /// Reports usage as it goes *and* finishes with the same figure, which is
    /// what a real provider does.
    struct ReportsAndFinishes {
        usage: Usage,
    }

    #[async_trait]
    impl Provider for ReportsAndFinishes {
        fn describe(&self) -> String {
            "reports and finishes".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            let _ = events.send(StreamEvent::Usage(self.usage));
            let _ = events.send(StreamEvent::Text("done".to_string()));
            Ok(TurnSummary {
                text: "done".to_string(),
                stop_reason: Some("end_turn".to_string()),
                usage: Some(self.usage),
                ..TurnSummary::default()
            })
        }
    }

    fn spent_events(events: &[AgentEvent]) -> Vec<(String, Usage)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Spent { tier, usage } => Some((tier.clone(), *usage)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn usage_reported_before_a_stall_is_still_counted() {
        // The gap this closes: the tier said what it had spent, then the stream
        // was abandoned before it could report a total. Reading usage only off a
        // finished response threw that away, so the cost of a stalled tier —
        // exactly what someone is trying to measure — was invisible.
        let dir = tempfile::tempdir().expect("tempdir");
        let stalling = Arc::new(ReportsThenHangs {
            usage: Usage {
                prompt_tokens: 9_000,
                completion_tokens: 12,
                cache_read_tokens: 4_000,
                cache_write_tokens: 0,
            },
        });
        let healthy = Quiet::new("recovered");

        let tiers: Vec<(Arc<dyn Provider>, Limits)> =
            vec![(stalling, short_allowance()), (healthy, Limits::default())];
        let (tx, mut rx) = loop_over(dir.path(), chain_of(tiers));
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "the tier should have been abandoned: {events:?}"
        );

        let spent = spent_events(&events);
        let stalled = spent
            .iter()
            .find(|(tier, _)| tier == "Tier 0")
            .expect("the stalled tier's spend should be reported");
        assert_eq!(
            stalled.1.prompt_tokens, 9_000,
            "what it reported before the stall is what it spent"
        );
        assert_eq!(stalled.1.cache_read_tokens, 4_000);
    }

    #[tokio::test]
    async fn usage_reported_before_a_cancel_is_still_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stalling = Arc::new(ReportsThenHangs {
            usage: Usage {
                prompt_tokens: 5_000,
                completion_tokens: 8,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });

        let (config, cancel) = config_with_cancel(dir.path(), DEFAULT_MAX_STEPS);
        let (tx, mut rx) = spawn(
            config,
            single_tier(stalling),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();
        let events = collect(&mut rx).await;

        assert!(
            matches!(events.last(), Some(AgentEvent::Cancelled { .. })),
            "{events:?}"
        );
        let spent = spent_events(&events);
        assert_eq!(spent.len(), 1, "{events:?}");
        assert_eq!(
            spent[0].1.prompt_tokens, 5_000,
            "a stopped request was still billed"
        );
    }

    #[tokio::test]
    async fn usage_is_not_counted_twice_when_it_is_reported_and_then_summarised() {
        // A real provider does both: it reports usage as the frames arrive and
        // again in the finished response. Those are the same figure, so adding
        // them would double every turn — which is the mistake the first draft of
        // this made.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = Arc::new(ReportsAndFinishes {
            usage: Usage {
                prompt_tokens: 1_000,
                completion_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });

        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        let spent = spent_events(&events);
        assert_eq!(spent.len(), 1, "one request, one spend: {events:?}");
        assert_eq!(
            spent[0].1.prompt_tokens, 1_000,
            "the same figure twice is still one figure"
        );
        assert_eq!(spent[0].1.completion_tokens, 10);
    }

    // ---- consult ----------------------------------------------------------

    /// A driver that loops, and a consultant that answers.
    ///
    /// The driver is given a second turn so it can answer once it has been
    /// helped, which is what distinguishes a consult from an escalation.
    fn consultable(
        consult_cap: u32,
    ) -> (FallbackChain, Arc<ScriptedProvider>, Arc<ScriptedProvider>) {
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);
        let consultant = ScriptedProvider::new(vec![answer("Use a HashMap instead.")]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver.clone(),
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        first.consults_per_turn = consult_cap;

        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant.clone(),
            Limits::default(),
        );

        (
            FallbackChain::new(vec![first, second], true).expect("a chain"),
            driver,
            consultant,
        )
    }

    fn consulted(events: &[AgentEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Consulted {
                    driver, consultant, ..
                } => Some((driver.clone(), consultant.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_stuck_driver_consults_and_then_carries_on_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("make the tests pass".to_string()))
            .expect("send");
        let events = drain_from(&mut rx).await;

        // The point of consult: the turn was never handed over.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "a consult must not escalate: {events:?}"
        );
        assert_eq!(
            consulted(&events),
            vec![("Local".to_string(), "DeepSeek".to_string())],
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(AgentEvent::Finished { .. })),
            "the driver should have finished the turn: {:?}",
            events.last()
        );

        // Two requests at the driver: the one that looped, and the one after the
        // advice. One at the consultant.
        assert_eq!(driver.request_count(), 2);
        assert_eq!(consultant.request_count(), 1);
    }

    #[tokio::test]
    async fn the_advice_reaches_the_driver_as_context_it_can_act_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, driver, _consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("make the tests pass".to_string()))
            .expect("send");
        let _ = drain_from(&mut rx).await;

        let after = driver.request(1);
        let injected = after
            .messages
            .iter()
            .find(|message| message.content.contains("more capable model"))
            .expect("the advice should be in the driver's history");

        assert!(
            injected.content.contains("Use a HashMap instead."),
            "{}",
            injected.content
        );
        assert!(
            injected.content.contains("advice, not a report of work"),
            "the driver must not think the work is already done: {}",
            injected.content
        );
    }

    #[tokio::test]
    async fn the_consultant_is_offered_no_tools_so_it_must_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;

        let request = consultant.request(0);
        assert!(
            request.tools.is_empty(),
            "a consultant with tools would act instead of answering: {:?}",
            request.tools
        );
    }

    #[tokio::test]
    async fn the_question_is_built_from_the_users_words_and_the_raw_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _driver, consultant) = consultable(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("make the tests pass".to_string()))
            .expect("send");
        let _ = drain_from(&mut rx).await;

        let question = &consultant.request(0).messages[0].content;
        assert!(
            question.contains("make the tests pass"),
            "the goal must be the user's own words: {question}"
        );
        assert!(
            question.contains("repeated the same output"),
            "the raw reason must be there: {question}"
        );
        assert!(
            question.contains("the same line"),
            "the looped output is the evidence, so it goes in verbatim: {question}"
        );
    }

    #[tokio::test]
    async fn a_second_consult_is_told_the_first_advice_failed() {
        // Otherwise the likeliest outcome of consulting twice is paying for the
        // same advice again.
        let dir = tempfile::tempdir().expect("tempdir");

        let driver = ScriptedProvider::new(vec![
            looping_answer(),
            looping_answer(),
            answer("recovered"),
        ]);
        let consultant = ScriptedProvider::new(vec![
            answer("Try a HashMap."),
            answer("Then try a BTreeMap."),
        ]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver.clone(),
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        first.consults_per_turn = 3;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant.clone(),
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(consulted(&events).len(), 2, "{events:?}");
        let second_question = &consultant.request(1).messages[0].content;
        assert!(
            second_question.contains("already been helped"),
            "{second_question}"
        );
        assert!(
            second_question.contains("Try a HashMap."),
            "it should know what was already said: {second_question}"
        );
        assert!(
            second_question.contains("Do not repeat that advice"),
            "{second_question}"
        );
    }

    #[tokio::test]
    async fn the_cap_is_what_stops_a_stuck_driver_spinning_the_frontier() {
        // The whole reason the cap exists: without it a driver in a loop could
        // ask forever, and every ask is a frontier call.
        let dir = tempfile::tempdir().expect("tempdir");

        // Always loops, so nothing but the cap can end it.
        let driver = Arc::new(ScriptedProvider {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            fallback: looping_answer(),
            fail_with: None,
        });
        let consultant = ScriptedProvider::new(vec![answer("advice one"), answer("advice two")]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver.clone(),
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        first.consults_per_turn = 2;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant.clone(),
            Limits::default(),
        );
        // Two tiers only, so after the cap the driver escalates into the
        // consultant and then exhausts.
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(
            consulted(&events).len(),
            2,
            "it should consult exactly the cap, then stop asking: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "once the budget is spent the turn must escalate: {events:?}"
        );
        assert_eq!(
            consultant.request_count(),
            3,
            "twice as a consultant and once as the tier it escalated into"
        );
    }

    #[tokio::test]
    async fn a_consult_that_cannot_complete_escalates_rather_than_stranding_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A consultant that fails, so the consult cannot complete.
        let consultant = ScriptedProvider::failing("connection refused");
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant,
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("could not be consulted")),
            "the failure should be visible: {:?}",
            notices(&events)
        );
        assert!(
            consulted(&events).is_empty(),
            "a failed consult is not a consult: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "it must fall through to the path that always terminates: {events:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_answer_escalates_rather_than_injecting_nothing() {
        // An empty injection would cost context and say nothing, so it is not
        // treated as a successful consult.
        let dir = tempfile::tempdir().expect("tempdir");
        let consultant = ScriptedProvider::new(vec![answer("")]);
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant,
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(consulted(&events).is_empty(), "{events:?}");
        assert!(
            notices(&events)
                .iter()
                .any(|note| note.contains("said nothing")),
            "{:?}",
            notices(&events)
        );
    }

    #[tokio::test]
    async fn a_driver_with_no_tier_below_it_escalates_instead_of_consulting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);
        let mut only = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        only.on_stuck = OnStuck::Consult;
        let chain = FallbackChain::new(vec![only], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            consulted(&events).is_empty(),
            "there is nobody to ask: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(AgentEvent::Exhausted { .. })),
            "with no next tier the turn ends: {:?}",
            events.last()
        );
    }

    #[tokio::test]
    async fn the_default_policy_still_escalates_and_never_consults() {
        // Consult is opt-in; nothing about the old behaviour may have changed.
        let dir = tempfile::tempdir().expect("tempdir");
        let looping = ScriptedProvider::new(vec![looping_answer()]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> =
            vec![(looping, Limits::default()), (healthy, Limits::default())];
        let (tx, mut rx) = loop_over(dir.path(), chain_of(tiers));

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            consulted(&events).is_empty(),
            "the default tier must not consult: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn the_consultants_tokens_are_reported_as_the_consultants() {
        // `/cost` is the measurement consult exists to inform, so charging the
        // consultant's tokens to the driver would defeat the point.
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);
        let consultant = ScriptedProvider::new(vec![TurnSummary {
            text: "Use a HashMap.".to_string(),
            stop_reason: Some("end_turn".to_string()),
            usage: Some(Usage {
                prompt_tokens: 900,
                completion_tokens: 40,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            ..TurnSummary::default()
        }]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        let second = Tier::new(
            "DeepSeek".to_string(),
            "m1".to_string(),
            consultant,
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        // Two spends: the driver's attempt, then the consultant's answer.
        let spent: Vec<(String, Usage)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Spent { tier, usage } => Some((tier.clone(), *usage)),
                _ => None,
            })
            .collect();

        let consultant = spent
            .iter()
            .find(|(tier, _)| tier == "DeepSeek")
            .expect("the consultant's spend should be reported")
            .1;
        assert_eq!(consultant.prompt_tokens, 900);
        assert_eq!(consultant.completion_tokens, 40);
    }
}
