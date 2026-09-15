//! The agent loop: stream a turn, run any tools the model asks for, feed the
//! results back, and repeat until the model answers without asking for one.

pub mod approval;
pub mod consult;
pub mod tools;
pub mod undo;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::agent::approval::{Approver, Decision};
use crate::agent::tools::{Registry, Risk, ToolOutcome, shell};
use crate::agent::undo::UndoStack;
use crate::allow::{AllowRules, Rule};
use crate::config::{Config, OnStuck, Origin};
use crate::detect::progress::ProgressDetector;
use crate::detect::{ErrorClass, StuckReason, Watchdog};
use crate::fallback::{FallbackChain, Tier};
use crate::provider::{ChatRequest, Provider, StreamEvent, TurnSummary, Usage};
use crate::session::{ChatMessage, Session, ToolCall};
use crate::session_store::{SessionFile, SessionStore};
use crate::stalls::{Counters, Miss, Policy, SpillEntry, SpillLog, Verdict};

/// Default cap on tool steps in a single turn, so a model that keeps calling
/// tools without concluding cannot spin forever.
#[cfg(test)]
pub use crate::config::DEFAULT_MAX_STEPS;

/// How many recent user turns compaction leaves intact.
pub const KEEP_TURNS: usize = 3;

/// Below this many user turns there is nothing worth compacting, so an
/// escalation does not churn the history of a short session.
const COMPACT_ABOVE_TURNS: usize = KEEP_TURNS + 2;

/// Stops a turn that is already running.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    pub fn system_prompt(self, workspace: &std::path::Path) -> String {
        match self {
            Self::Build => build_prompt(workspace),
            Self::Plan => plan_prompt(workspace),
        }
    }
}

/// What the interface asks the agent to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// An ordinary message, and a turn.
    Prompt(String),
    /// Change what a turn is allowed to do.
    SetMode(Mode),
    /// Move to the next tier now, and stay there.
    Escalate,
    /// Ask the next tier about the next stall, and keep the driver.
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
    /// Start a new session: empty transcript, first tier, no CLI resume.
    New,
    /// Open the session picker.
    ListSessions,
    /// Resume a saved conversation by id.
    OpenSession(String),
    /// Name the current session.
    RenameSession(String),
    /// Forget a saved conversation.
    DeleteSession(String),
    /// Report what is being sent each turn.
    Context,
    /// Put back an approved write.
    Undo,
    /// Add, list or drop the shell rules this session runs without asking about.
    Allow(AllowChange),
}

/// What `/allow` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowChange {
    /// Report the rules in force.
    List,
    /// Stick a rule for the rest of the session.
    Add(String),
    /// Stick a rule, and write the whole list in force into the config file.
    Save(String),
    /// Drop this session's rules.
    Clear,
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
    /// Internal reasoning, shown while it streams, not kept as the answer.
    Thought(String),
    /// The active tier stalled, looped, or failed, so the same turn is being
    /// retried on the tier below it.
    Escalated {
        from: String,
        to: String,
        reason: String,
    },
    /// The same move, announced before it happens.
    Spilling {
        from: String,
        to: String,
        reason: String,
    },
    /// The user picked a tier, or went back to the top of the chain.
    ///
    /// Distinct from `Escalated`: that marks the previous tier as spent. This
    /// just moves the rail, including back up.
    Switched { to: String },
    /// Tokens a tier spent on this turn.
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
    /// A tier was abandoned, with everything the decision was made from.
    Stalled { verdict: Box<Verdict> },
    /// A turn that finished, having come within one step of being abandoned.
    AlmostStalled { tier: String, miss: Miss },
    /// The named history for this workspace, for the picker.
    SessionList {
        current: String,
        entries: Vec<crate::session_store::SessionEntry>,
    },
    /// A saved conversation was opened; the interface replaces what it shows.
    SessionLoaded {
        id: String,
        title: String,
        messages: Vec<ChatMessage>,
        active_label: String,
        sticky: bool,
        on_stuck: Option<OnStuck>,
        mode: Mode,
    },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub max_steps: usize,
    /// How a running turn is stopped from outside.
    pub cancel: Canceller,
    /// Where the session is written so it can be resumed after a restart.
    pub store: Option<SessionStore>,
    /// Where spills are recorded, or `None` to keep no record.
    pub log: Option<SpillLog>,
    /// Shell commands that may run without being asked about.
    pub allow_shell: Vec<String>,
    /// Which configuration file is in force, so `/allow save` can write to it.
    pub origin: Origin,
}

/// A session to pick up where the last run left off.
#[derive(Debug, Clone)]
pub struct Seed {
    /// The conversation, without the system prompt, which is rebuilt from the
    /// restored mode.
    pub messages: Vec<ChatMessage>,
    pub mode: Mode,
    pub id: String,
    pub title: String,
    pub named: bool,
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
    /// The approved writes that can still be put back, newest first.
    undo: UndoStack,
    /// Which shell commands run without being asked about.
    allow: AllowRules,
    /// Whether the spill log has already been reported as unwritable.
    log_warned: bool,
    session_id: String,
    session_title: String,
    session_named: bool,
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
    spawn_seeded(config, chain, registry, approver, None)
}

/// The same, picking up a conversation saved by an earlier run.
pub fn spawn_seeded(
    config: AgentConfig,
    chain: FallbackChain,
    registry: Arc<Registry>,
    approver: Arc<dyn Approver>,
    seed: Option<Seed>,
) -> (UnboundedSender<Command>, UnboundedReceiver<AgentEvent>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Built once, so both branches of the loop below start from the same rules.
    let rules = AllowRules::new(&config.allow_shell);

    tokio::spawn(async move {
        let mut state = match seed {
            Some(seed) => Loop {
                // The system prompt is rebuilt rather than restored, so a
                // conversation can never carry instructions from a mode it is
                // no longer in.
                session: Session::restore(
                    seed.mode.system_prompt(&config.workspace),
                    seed.messages,
                ),
                chain,
                mode: seed.mode,
                last: None,
                // Undo does not survive a restart, so a resumed session starts
                // with nothing to put back — which is honest, since the bytes it
                // would restore were never written down.
                undo: UndoStack::default(),
                allow: rules,
                log_warned: false,
                session_id: seed.id,
                session_title: seed.title,
                session_named: seed.named,
            },
            None => Loop {
                session: Session::with_system_prompt(
                    Mode::default().system_prompt(&config.workspace),
                ),
                chain,
                mode: Mode::default(),
                last: None,
                undo: UndoStack::default(),
                allow: rules,
                log_warned: false,
                session_id: uuid::Uuid::new_v4().to_string(),
                session_title: "untitled".to_string(),
                session_named: false,
            },
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

/// Everything about this session that is worth keeping.
fn snapshot(config: &AgentConfig, state: &Loop) -> SessionFile {
    let chain = state.chain.state();

    let mut file = SessionFile::new(config.workspace.clone());
    file.id = state.session_id.clone();
    file.title = state.session_title.clone();
    file.named = state.session_named;
    file.active_tier = chain.active;
    file.pinned_tier = chain.pinned;
    file.sticky = chain.sticky;
    file.on_stuck = chain.on_stuck;
    file.mode = state.mode;
    file.messages = state.session.conversation();
    // Keyed by configured id, which is what will still name the same tier on
    // the next start even if the file was reordered in between.
    for tier in state.chain.tiers() {
        if let Some(id) = tier.provider.session_id() {
            file.cli_sessions.insert(tier.id.clone(), id);
        }
    }
    file.refresh_title();
    file
}

fn adopt_session(state: &mut Loop, config: &AgentConfig, file: &SessionFile) {
    state.session = Session::restore(
        file.mode.system_prompt(&config.workspace),
        file.messages.clone(),
    );
    state.last = None;
    state.undo = UndoStack::default();
    state.mode = file.mode;
    state.chain.restore_state(&file.chain_state());
    for tier in state.chain.tiers() {
        tier.provider
            .set_session(file.cli_sessions.get(&tier.id).cloned());
    }
    state.session_id = file.id.clone();
    state.session_title = file.title.clone();
    state.session_named = file.named;
}

fn session_loaded(state: &Loop) -> AgentEvent {
    AgentEvent::SessionLoaded {
        id: state.session_id.clone(),
        title: state.session_title.clone(),
        messages: state.session.conversation(),
        active_label: state.chain.active().label.clone(),
        sticky: state.chain.state().sticky,
        on_stuck: state.chain.state().on_stuck,
        mode: state.mode,
    }
}

/// Write the session out, reporting a failure rather than interrupting anything.
fn persist(config: &AgentConfig, state: &mut Loop, events: &UnboundedSender<AgentEvent>) {
    let Some(store) = &config.store else {
        return;
    };
    let file = snapshot(config, state);
    state.session_id = file.id.clone();
    state.session_title = file.title.clone();
    if let Err(error) = store.save(&file) {
        let _ = events.send(AgentEvent::Notice(format!(
            "this session could not be saved to {} ({error}), so it will not be resumed next time",
            store.path().display()
        )));
    }
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
                &mut state.undo,
                &state.allow,
                &mut state.log_warned,
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
            let from = state.chain.active().label.clone();
            let stepped = state.chain.escalate().map(|tier| tier.label.clone());
            match stepped {
                Some(to) => {
                    let index = state.chain.active_index();
                    let total = state.chain.len();
                    // A hand-issued escalation is a choice, not a fallback, so
                    // it is pinned: otherwise a per-turn chain would snap back
                    // to the top on the next message and the command would look
                    // broken.
                    state.chain.pin(index);
                    // The rail only moves on this event. A Notice alone left it
                    // looking like the old tier was still answering.
                    let _ = events.send(AgentEvent::Escalated {
                        from,
                        to: to.clone(),
                        reason: "you asked".to_string(),
                    });
                    let _ = events.send(AgentEvent::Notice(format!(
                        "moving to {to} ({} of {total}) — /deescalate or /tier auto goes back",
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
                        let to = state.chain.pin(index).map(|tier| tier.label.clone());
                        if let Some(to) = to {
                            let _ = events.send(AgentEvent::Switched { to });
                        }
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
                &mut state.undo,
                &state.allow,
                &mut state.log_warned,
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
                state.chain.return_to_top();
                let to = state.chain.active().label.clone();
                let _ = events.send(AgentEvent::Switched { to: to.clone() });
                let _ = events.send(AgentEvent::Notice(format!(
                    "back to the configured order — {to} answers next"
                )));
            }
            Some(query) => match state.chain.resolve(&query) {
                Some(index) => {
                    let label = state.chain.pin(index).map(|tier| tier.label.clone());
                    if let Some(to) = label {
                        let _ = events.send(AgentEvent::Switched { to: to.clone() });
                        let _ = events.send(AgentEvent::Notice(format!(
                            "{to} will answer from here, until you say /tier auto"
                        )));
                    }
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

        Command::New => {
            if let Some(store) = &config.store {
                let _ = store.save(&snapshot(config, state));
                let file = store.start_new();
                adopt_session(state, config, &file);
            } else {
                state.session.reset();
                state.last = None;
                state.chain.forget_sessions();
                state.chain.return_to_top();
                state.chain.set_on_stuck(None);
                state.session_id = uuid::Uuid::new_v4().to_string();
                state.session_title = "untitled".to_string();
                state.session_named = false;
            }
            let to = state.chain.active().label.clone();
            let _ = events.send(AgentEvent::Switched { to: to.clone() });
            let _ = events.send(AgentEvent::Notice(format!("new session — {to} answers")));
        }

        Command::ListSessions => {
            let Some(store) = &config.store else {
                let _ = events.send(AgentEvent::Notice(
                    "sessions are not being saved, so there is nothing to pick from".to_string(),
                ));
                return;
            };
            let _ = store.save(&snapshot(config, state));
            let _ = events.send(AgentEvent::SessionList {
                current: store.current_id().unwrap_or_default(),
                entries: store.list(),
            });
        }

        Command::OpenSession(id) => {
            let Some(store) = &config.store else {
                return;
            };
            let _ = store.save(&snapshot(config, state));
            let Some(file) = store.open(&id) else {
                let _ = events.send(AgentEvent::Notice(format!("no session matches {id:?}")));
                return;
            };
            adopt_session(state, config, &file);
            let _ = events.send(session_loaded(state));
        }

        Command::RenameSession(title) => {
            let Some(store) = &config.store else {
                return;
            };
            match store.rename(&title) {
                Some(title) => {
                    state.session_title = title.clone();
                    state.session_named = true;
                    let _ = events.send(AgentEvent::Notice(format!("session named {title:?}")));
                }
                None => {
                    let _ = events.send(AgentEvent::Notice(
                        "usage: /session rename <name>".to_string(),
                    ));
                }
            }
        }

        Command::DeleteSession(id) => {
            let Some(store) = &config.store else {
                return;
            };
            match store.delete(&id) {
                Ok(Some(file)) => {
                    if file.id != state.session_id {
                        adopt_session(state, config, &file);
                        let _ = events.send(session_loaded(state));
                    }
                    let _ = events.send(AgentEvent::Notice("session deleted".to_string()));
                    let _ = events.send(AgentEvent::SessionList {
                        current: store.current_id().unwrap_or_default(),
                        entries: store.list(),
                    });
                }
                Ok(None) => {
                    let _ = events.send(AgentEvent::Notice(
                        "that session was already gone".to_string(),
                    ));
                }
                Err(error) => {
                    let _ = events.send(AgentEvent::Notice(format!(
                        "could not delete the session: {error}"
                    )));
                }
            }
        }

        Command::Context => {
            let _ = events.send(AgentEvent::Notice(describe_context(
                &state.session,
                &state.chain,
            )));
        }

        // Popped rather than read, so one undo reaches exactly one write: the
        // stack shrinks by what was put back, and going deeper is a second
        // deliberate command rather than something one press did quietly.
        Command::Undo => match state.undo.pop() {
            Some(entry) => match entry.restore().await {
                Ok(said) => {
                    let _ = events.send(AgentEvent::Notice(format!(
                        "· {said}{}",
                        reachable(state.undo.depth())
                    )));
                }
                Err(refused) => {
                    // Put back on a refusal: nothing was changed, so this is
                    // still the newest write to reach for, and a user who puts
                    // their own edit back by hand can try again. Said this way
                    // because the entries behind it are blocked until then.
                    let _ = events.send(AgentEvent::Notice(format!(
                        "✗ {refused}{}",
                        behind(state.undo.depth())
                    )));
                    state.undo.restore(entry);
                }
            },
            None => {
                let _ = events.send(AgentEvent::Notice(
                    "nothing to undo — no approved write is still on the stack".to_string(),
                ));
            }
        },

        Command::Allow(change) => {
            let notice = match change {
                AllowChange::List => describe_rules(&state.allow),
                AllowChange::Add(text) => add_rule(&mut state.allow, &text, None),
                AllowChange::Save(text) => add_rule(&mut state.allow, &text, Some(&config.origin)),
                AllowChange::Clear => {
                    let dropped = state.allow.clear_session();
                    if dropped == 0 {
                        "· no rules were stuck this session, so nothing changed".to_string()
                    } else {
                        format!(
                            "· {dropped} session rule(s) dropped. The ones in your configuration \
                             are unchanged, so /allow still lists them."
                        )
                    }
                }
            };
            let _ = events.send(AgentEvent::Notice(notice));
        }
    }

    // One write point for the whole session. Every command is here — a prompt
    // runs its turn inside this function — so a completed turn, a tier change,
    // a policy change, and `/clear` are all saved by the same call, and a future
    // command cannot forget to save by not being listed anywhere.
    persist(config, state, events);
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
fn short(label: &str) -> &str {
    crate::fallback::tier_name(label)
}

/// What one tier made of a turn, and what finding out cost.
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
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: &AgentConfig,
    chain: &mut FallbackChain,
    mode: Mode,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    session: &mut Session,
    undo: &mut UndoStack,
    allow: &AllowRules,
    log_warned: &mut bool,
    prompt: String,
) {
    session.push(ChatMessage::user(prompt));
    // Which turn of the conversation this is, counted the way a person would:
    // in things they asked for.
    let turn = session
        .messages()
        .iter()
        .filter(|message| message.role == crate::session::Role::User)
        .count();
    chain.begin_turn();
    let checkpoint = session.messages().len();
    // A cancel from a previous turn must not stop this one.
    config.cancel.arm();
    // Consults spent within this turn, and what each one said. Both are
    // per-turn: the cap exists to bound one stuck episode, and the answers are
    // only relevant to the consult that follows them.
    let mut consulted: Vec<consult::Previous> = Vec::new();

    loop {
        let (label, tier_id, outcome, counters) = {
            let tier = chain.active();
            let (outcome, counters) = try_tier(
                config, tier, mode, registry, approver, events, session, undo, allow,
            )
            .await;
            (tier.label.clone(), tier.id.clone(), outcome, counters)
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
                // A turn that stayed can still have been close, and a near miss
                // nobody is told about teaches nothing about whether the
                // thresholds are right. Sent before the finish so the aside lands
                // above the answer rather than after it.
                if let Some(miss) = counters.closest_miss() {
                    let _ = events.send(AgentEvent::AlmostStalled {
                        tier: short(&label).to_string(),
                        miss,
                    });
                }
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
                // Built here, before the move is decided, so the evidence is on
                // record whether the turn is handed over, consulted about, or
                // simply ends — and identical to what the log gets.
                let verdict = Verdict {
                    tier_id,
                    tier_name: short(&label).to_string(),
                    reason: reason.clone(),
                    counters,
                };
                let _ = events.send(AgentEvent::Stalled {
                    verdict: Box::new(verdict.clone()),
                });
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
                        record_spill(
                            config,
                            log_warned,
                            events,
                            &verdict,
                            turn,
                            chain.consultant().map(|tier| tier.id.clone()),
                            Policy::Consult,
                        );

                        // Back to the same tier, with the answer in hand.
                        continue;
                    }
                }

                // Announced before the attempt is discarded, so the interface can
                // narrate the reason rather than presenting the move as a fait
                // accompli. The tier below is read without escalating, because
                // `escalate` is what commits the move and it has not happened yet.
                if let Some(to) = chain.consultant().map(|tier| tier.label.clone()) {
                    let _ = events.send(AgentEvent::Spilling {
                        from: from.clone(),
                        to,
                        reason: reason.summary(),
                    });
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
                        record_spill(
                            config,
                            log_warned,
                            events,
                            &verdict,
                            turn,
                            Some(next.id.clone()),
                            Policy::Escalate,
                        );
                        compact_before_falling(session, chain, events);
                    }
                    None => {
                        let _ = events.send(AgentEvent::Exhausted {
                            reason: reason.summary(),
                        });
                        // Logged even though nothing took over, because this is
                        // the only ending a single-tier setup can have and its
                        // thresholds are the ones most worth tuning.
                        record_spill(
                            config,
                            log_warned,
                            events,
                            &verdict,
                            turn,
                            None,
                            Policy::Ended,
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Write one spill, and say so once if the log cannot be written.
#[allow(clippy::too_many_arguments)]
fn record_spill(
    config: &AgentConfig,
    warned: &mut bool,
    events: &UnboundedSender<AgentEvent>,
    verdict: &Verdict,
    turn: usize,
    to: Option<String>,
    policy: Policy,
) {
    let Some(log) = config.log.as_ref() else {
        return;
    };

    let entry = SpillEntry::from_verdict(
        verdict,
        crate::stalls::timestamp(std::time::SystemTime::now()),
        turn,
        to,
        policy,
    );

    if let Err(error) = log.append(&entry) {
        if !*warned {
            *warned = true;
            let _ = events.send(AgentEvent::Notice(format!(
                "could not write the spill log at {}: {error}",
                log.path().display()
            )));
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

    // A consult is one reply that cannot act, and everything below assumes it.
    // An HTTP consultant is held to that by being sent no tools. A CLI runs its
    // own harness, so the only lever is a read-only flag, and a CLI with none is
    // refused here — with the reason said out loud — rather than asked politely
    // and trusted. `None` escalates the turn, which is where a refusal belongs.
    if let Some(reason) = consultant.provider.consult_refusal() {
        let _ = events.send(AgentEvent::Notice(format!(
            "{} cannot be consulted: {reason} — handing the turn over instead",
            consultant.label
        )));
        return None;
    }

    let question = consult::build(goal, reason, evidence, previous);

    // No tools attached, which is what makes "the answer is prose" true for an
    // HTTP consultant rather than hoped for: it has nothing to call, so it must
    // answer, and the call is one round trip rather than a tool loop. A CLI
    // consultant turns the same request into a read-only run with a hard
    // instruction not to act, which is the most that can be done for a harness
    // spill does not run.
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
        TurnKind::Consult,
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
#[allow(clippy::too_many_arguments)]
async fn try_tier(
    config: &AgentConfig,
    tier: &Tier,
    mode: Mode,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    events: &UnboundedSender<AgentEvent>,
    session: &mut Session,
    undo: &mut UndoStack,
    allow: &AllowRules,
) -> (Attempt, Counters) {
    // A read-only turn is not offered the tools that could change anything. The
    // refusal in `run_tool` is what makes that a guarantee; this is what stops a
    // cooperative model from wasting turns on calls that would be refused.
    let tools = registry.specs_permitting(mode.risk_ceiling());
    let mut watchdog = Watchdog::new(&tier.limits);
    let mut progress = ProgressDetector::new(tier.limits.max_repeat_run as usize);
    // Every request this tier makes is billed, one per step, so the total is
    // carried across the loop rather than read off the last step.
    let mut spent: Option<Usage> = None;
    // How many requests have been made, which is what the step budget is spent
    // in. Counted rather than derived from the loop index, because the answer a
    // cancel gives at the top of a step is one less than the answer a completed
    // request gives, and both are read by the report.
    let mut steps_used = 0;

    let attempt = 'attempt: {
        for step in 0..config.max_steps {
            steps_used = step;
            // Stopped between steps, so a cancel that arrives while tools are being
            // run does not buy another round trip.
            if config.cancel.is_cancelled() {
                break 'attempt Attempt::Cancelled(spent);
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
                TurnKind::Normal,
            )
            .await;

            // A request has now been made, whatever became of it.
            steps_used = step + 1;

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
                    break 'attempt Attempt::Cancelled(spent);
                }
                Err(reason) => {
                    crate::provider::accumulate(&mut spent, observed);
                    break 'attempt Attempt::Stuck(reason, spent);
                }
            };

            session.push(ChatMessage::assistant(
                summary.text.clone(),
                summary.tool_calls.clone(),
            ));

            if summary.tool_calls.is_empty() {
                break 'attempt Attempt::Answered {
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
                    allow,
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
                        break 'attempt Attempt::Cancelled(spent);
                    }
                };

                // The means to reverse this, if it changed a file. Lifted out before
                // the result moves into the session, and only for a write that
                // succeeded — the tool leaves it `None` otherwise.
                let mut outcome = outcome;
                if let Some(entry) = outcome.undo.take() {
                    undo.push(*entry);
                }

                // Classified before the result moves into the session, so the kind of
                // failure is available to the detector. `Other` for anything
                // unrecognised, which is judged by the tier's own allowance rather
                // than a tighter one.
                let failure = outcome.is_error.then(|| {
                    outcome
                        .class
                        .unwrap_or_else(|| ErrorClass::classify(&outcome.content))
                });
                if let Some(reason) = progress.record(&call.name, &call.arguments, failure) {
                    break 'attempt Attempt::Stuck(reason, spent);
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
    };

    // Read after the loop rather than at each exit: the counters are the
    // detectors' own state, so they describe the attempt wherever it ended and
    // there is no second copy to keep in step with the first.
    // `steps_used` is moved out of the loop above, so this is the one place the
    // figure is turned into a report.
    let counters = Counters {
        steps_used,
        steps_allowed: config.max_steps,
        repetition: watchdog.repetition_counters(),
        progress: progress.counters(),
        timing: watchdog.timing(),
    };

    (attempt, counters)
}

/// Which entry point of a provider a turn runs through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnKind {
    Normal,
    Consult,
}

/// Stream one turn from one tier, cutting it off if it goes quiet or loops.
async fn stream_turn(
    provider: &Arc<dyn Provider>,
    request: ChatRequest,
    events: &UnboundedSender<AgentEvent>,
    watchdog: &mut Watchdog,
    cancel: &Canceller,
    kind: TurnKind,
) -> (Result<TurnSummary, StuckReason>, Option<Usage>) {
    // Timing is per request: the gap since the previous request's last frame
    // would include the tool call that ran in between.
    watchdog.begin_request();
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let provider = provider.clone();
    let mut task = tokio::spawn(async move {
        match kind {
            TurnKind::Normal => provider.stream(request, delta_tx).await,
            TurnKind::Consult => provider.consult(request, delta_tx).await,
        }
    });

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
                    Err(_) => break Err(watchdog.timed_out(allowance)),
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
        StreamEvent::Thought(text) => {
            // Thinking is progress, not the answer: it must not feed the
            // repetition detector, or a model that reasons in circles looks stuck.
            watchdog.note_activity();
            let _ = events.send(AgentEvent::Thought(text));
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

/// The same borrow split as `try_tier`: the undo stack is `&mut` while the rules
/// are `&`, so a single `&mut Loop` here would borrow the loop twice.
#[allow(clippy::too_many_arguments)]
async fn run_tool(
    mode: Mode,
    registry: &Arc<Registry>,
    approver: &Arc<dyn Approver>,
    workspace: &std::path::Path,
    call: &ToolCall,
    allow: &AllowRules,
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
    // A rule is a prefix of words, so it can only be read against a command, and a
    // command is what `run_shell` takes. A file write is never covered by one: the
    // diff it shows is the whole reason it asks.
    let covered = (call.name == shell::NAME)
        .then(|| arguments.get("command").and_then(serde_json::Value::as_str))
        .flatten()
        .and_then(|command| allow.allows(command));

    if tool.risk() == Risk::Write && covered.is_none() {
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
    } else if let Some(rule) = covered {
        // Said out loud rather than done quietly. The command itself is shown by
        // the tool line that follows; what this adds is the reason nobody was
        // asked, which is the thing a rule could otherwise hide.
        let _ = events.send(AgentEvent::Notice(format!(
            "· {} runs without asking (rule: {})",
            call.name,
            rule.text()
        )));
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

/// How much of the undo stack is still reachable, in words.
fn reachable(depth: usize) -> String {
    match depth {
        0 => " — that was the last write on the stack".to_string(),
        1 => " — 1 more write can still be put back".to_string(),
        count => format!(" — {count} more writes can still be put back"),
    }
}

/// The same, for a refusal: the entry stays, so this counts what is behind it.
fn behind(depth: usize) -> String {
    match depth {
        0 => String::new(),
        1 => " (1 more write is behind it)".to_string(),
        count => format!(" ({count} more writes are behind it)"),
    }
}

/// The rules in force, as `/allow` reports them.
fn describe_rules(allow: &AllowRules) -> String {
    if allow.is_empty() {
        return "no shell command runs without asking. `/allow <words>` sticks one for this \
                session, and `/allow save <words>` writes it into your configuration."
            .to_string();
    }

    let (from_config, session) = allow.texts_by_source();
    let mut out = String::from("shell commands that run without asking:\n");
    // Packed rather than wrapped, so a rule like `git rev-parse` is never split
    // across two lines and read as two rules that nobody wrote.
    out.push_str(&crate::text::pack(
        from_config.iter().map(String::as_str),
        "  ",
        74,
    ));

    // Marked rather than mixed in: the difference between "until the file
    // changes" and "until spill exits" is the whole reason to list them at all.
    if !session.is_empty() {
        out.push_str(&crate::text::pack(
            session.iter().map(String::as_str),
            "* ",
            74,
        ));
        out.push_str("  (* stuck for this session — a new one asks again)\n");
    }

    out.trim_end().to_string()
}

/// Stick a rule, and optionally write the list in force into the config.
fn add_rule(allow: &mut AllowRules, text: &str, save: Option<&Origin>) -> String {
    let rule = match Rule::parse(text) {
        Ok(rule) => rule,
        Err(error) => return format!("✗ {error}"),
    };

    let words = rule.text();
    let added = allow.add_session(rule);

    let Some(origin) = save else {
        return if added {
            format!(
                "· this session will run {words:?} without asking. A new session will ask again — \
                 `/allow save {words}` writes it into your configuration."
            )
        } else {
            format!("· {words:?} already runs without asking, so nothing changed")
        };
    };

    if let Err(error) = origin.writable_path() {
        return format!(
            "✗ {error}. {}",
            if added {
                format!("{words:?} is in force for this session regardless.")
            } else {
                "Nothing changed.".to_string()
            }
        );
    }

    // Everything in force, not just this rule: the key *is* the list, so writing
    // one rule would drop the read-only set that was in force a moment ago.
    let rules = allow.texts();
    let path = origin.writable_path().expect("checked above");

    match Config::save_allow(path, &rules) {
        Ok(()) => format!(
            "· {} rules written to {}{}",
            rules.len(),
            path.display(),
            if added {
                format!(", including {words:?}")
            } else {
                String::new()
            }
        ),
        Err(error) => format!("✗ {error}"),
    }
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
            // Most tests are about a turn, not about what outlives the process;
            // the ones that are set a store on the config they build.
            store: None,
            log: None,
            // A fixture, so it behaves as though the configuration said nothing:
            // no rule covers anything, and there is no file for `/allow save` to
            // write to. The tests that are about either set what they need.
            allow_shell: Vec::new(),
            origin: Origin::default(),
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
            store: None,
            log: None,
            // A fixture, so it behaves as though the configuration said nothing:
            // no rule covers anything, and there is no file for `/allow save` to
            // write to. The tests that are about either set what they need.
            allow_shell: Vec::new(),
            origin: Origin::default(),
        };
        (config, cancel)
    }

    /// A chain of one, which is what most of these tests want.
    fn single_tier(provider: Arc<dyn Provider>) -> FallbackChain {
        chain_of(vec![(provider, Limits::default())])
    }

    /// Scripted tiers in order, so escalation can be driven end to end.
    fn chain_of(tiers: Vec<(Arc<dyn Provider>, Limits)>) -> FallbackChain {
        chain_of_with(tiers, OnStuck::default())
    }

    /// The same, with the stuck policy named on every tier.
    fn chain_of_with(tiers: Vec<(Arc<dyn Provider>, Limits)>, policy: OnStuck) -> FallbackChain {
        let tiers = tiers
            .into_iter()
            .enumerate()
            .map(|(index, (provider, limits))| {
                let mut tier = Tier::new(
                    format!("Tier {index}"),
                    format!("model-{index}"),
                    provider,
                    limits,
                );
                tier.on_stuck = policy;
                tier
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

    /// Like `run_one`, with a configuration of the test's own.
    async fn run_one_with(
        config: AgentConfig,
        script: Vec<TurnSummary>,
        approver: Arc<dyn Approver>,
        prompt: &str,
    ) -> (Vec<AgentEvent>, Arc<ScriptedProvider>) {
        let provider = ScriptedProvider::new(script);
        let (tx, rx) = spawn(
            config,
            single_tier(provider.clone()),
            Arc::new(Registry::with_default_tools()),
            approver,
        );
        tx.send(Command::Prompt(prompt.to_string()))
            .expect("send prompt");

        (drain(rx).await, provider)
    }

    /// The configuration a person gets who has written nothing about rules.
    fn config_with_default_rules(workspace: &std::path::Path) -> AgentConfig {
        AgentConfig {
            allow_shell: crate::allow::READ_ONLY_SHELL
                .iter()
                .map(|rule| rule.to_string())
                .collect(),
            ..config(workspace, DEFAULT_MAX_STEPS)
        }
    }

    /// A turn that asks for one shell command.
    fn shell_call(command: &str) -> TurnSummary {
        TurnSummary {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_shell".to_string(),
                name: shell::NAME.to_string(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        }
    }

    /// A turn that asks to write a file, for the rule that must not cover it.
    fn write_call(path: &str) -> TurnSummary {
        TurnSummary {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_write".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({ "path": path, "content": "hello" }).to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        }
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
            chain_of_with(tiers, OnStuck::Escalate),
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
            chain_of_with(tiers, OnStuck::Escalate),
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
            chain_of_with(tiers, OnStuck::Escalate),
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

    /// A two-tier chain whose first tier repeats itself and whose second
    /// answers, with the ids a log would name.
    fn looping_into_a_second(times: usize) -> (FallbackChain, Arc<Loops>) {
        let looping = Loops::new("the same line", times);
        let mut first = Tier::new(
            "Local (http://10.0.0.1:1234/v1)".to_string(),
            "m0".to_string(),
            looping.clone(),
            Limits::default(),
        );
        first.id = "local".to_string();
        first.on_stuck = OnStuck::Escalate;
        let mut second = Tier::new(
            "Frontier".to_string(),
            "m1".to_string(),
            Quiet::new("answered"),
            Limits::default(),
        );
        second.id = "frontier".to_string();
        (
            FallbackChain::new(vec![first, second], false).expect("a chain"),
            looping,
        )
    }

    /// The same, with one tier, so a stall has nowhere to go.
    fn looping_alone(times: usize) -> FallbackChain {
        let mut only = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            Loops::new("the same line", times),
            Limits::default(),
        );
        only.id = "local".to_string();
        FallbackChain::new(vec![only], false).expect("a chain")
    }

    /// A run whose spills are recorded, and the file they land in.
    fn loop_over_logging(
        dir: &std::path::Path,
        chain: FallbackChain,
    ) -> (
        UnboundedSender<Command>,
        UnboundedReceiver<AgentEvent>,
        std::path::PathBuf,
    ) {
        let path = dir.join("spills.jsonl");
        let mut config = config(dir, DEFAULT_MAX_STEPS);
        config.log = Some(crate::stalls::SpillLog::at(path.clone()));
        let (tx, rx) = spawn(
            config,
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        (tx, rx, path)
    }

    /// Every record in a spill log, in order.
    fn spills(path: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str(line).expect("each line is a record"))
            .collect()
    }

    fn verdicts(events: &[AgentEvent]) -> Vec<&Verdict> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Stalled { verdict } => Some(verdict.as_ref()),
                _ => None,
            })
            .collect()
    }

    fn near_misses(events: &[AgentEvent]) -> Vec<(String, Miss)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::AlmostStalled { tier, miss } => Some((tier.clone(), *miss)),
                _ => None,
            })
            .collect()
    }

    /// Collect whatever the agent emits, stopping once it goes quiet.
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
        let (from, to, reason) = escalation(&events).expect("/escalate must move the rail");
        assert!(from.starts_with("Local"), "{from}");
        assert!(to.starts_with("DeepSeek"), "{to}");
        assert_eq!(reason, "you asked");

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

    // ---- the stall call, made inspectable ----------------------------------

    #[tokio::test]
    async fn a_stall_is_reported_with_the_counters_behind_it() {
        // The point of the whole thing: the reason says what tripped, and the
        // counters say how close everything else came. A reason on its own is
        // not arguable, and a user who cannot argue with it turns it off.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, looping) = looping_into_a_second(4);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        let verdict = verdicts(&events);
        assert_eq!(verdict.len(), 1, "one stall, one verdict: {events:?}");
        let verdict = verdict[0];

        assert!(
            matches!(verdict.reason, StuckReason::Repetition { repeats: 4, .. }),
            "{:?}",
            verdict.reason
        );
        assert_eq!(verdict.counters.repetition.threshold, 4);
        assert_eq!(
            verdict.counters.steps_used, 1,
            "it never got past the first request"
        );
        assert_eq!(verdict.counters.steps_allowed, DEFAULT_MAX_STEPS);
        assert_eq!(verdict.counters.progress.failure_run, 0);
        assert_eq!(looping.requests(), 1);
    }

    #[tokio::test]
    async fn a_verdict_names_the_tier_without_its_address() {
        // The report is read by a person, and the address belongs in the
        // session panel; repeating it in every line is how one line becomes
        // three.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(4);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;
        let verdict = verdicts(&events)[0];

        assert_eq!(verdict.tier_name, "Local");
        assert!(
            !verdict.report().contains("http://"),
            "no address in the report: {}",
            verdict.report()
        );
        assert_eq!(verdict.tier_id, "local", "but the log keeps the real id");
    }

    #[tokio::test]
    async fn a_verdict_arrives_whether_or_not_the_turn_was_handed_over() {
        // A turn that ends because there is nowhere to go is still a stall, and
        // is the only ending a single-tier setup can have. Withholding the
        // evidence for it would make `/why` useless to exactly that user.
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = loop_over(dir.path(), looping_alone(4));

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert_eq!(verdicts(&events).len(), 1, "{events:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Exhausted { .. })),
            "and the turn still says it ran out of tiers: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_clean_turn_has_nothing_to_explain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = one_tier("a perfectly ordinary answer");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert!(verdicts(&events).is_empty(), "{events:?}");
        assert!(near_misses(&events).is_empty(), "{events:?}");
    }

    // ---- turns that nearly went the same way -------------------------------

    #[tokio::test]
    async fn a_turn_that_stayed_is_reported_when_it_was_one_repeat_away() {
        // Three repeats against an allowance of four: one more and the turn
        // would have been taken away. Silent almost-failures are what make a
        // threshold impossible to set.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(3);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Finished { .. })),
            "it answered: {events:?}"
        );
        assert_eq!(
            near_misses(&events),
            vec![(
                "Local".to_string(),
                Miss::Repeats {
                    seen: 3,
                    allowed: 4
                }
            )]
        );
    }

    #[tokio::test]
    async fn a_turn_two_short_of_the_allowance_says_nothing() {
        // The warning has to mean something. A tier that repeated itself twice
        // against an allowance of four is not about to be abandoned, and saying
        // so every turn would be noise.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(2);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert!(near_misses(&events).is_empty(), "{events:?}");
    }

    #[tokio::test]
    async fn a_stall_does_not_also_report_a_near_miss() {
        // The warning is for turns that stayed. One that was abandoned has the
        // verdict instead, and saying both would be two lines about one thing.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(4);
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert_eq!(verdicts(&events).len(), 1);
        assert!(near_misses(&events).is_empty(), "{events:?}");
    }

    // ---- the record on disk ------------------------------------------------

    #[tokio::test]
    async fn a_spill_is_logged_with_its_trigger_and_both_tiers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(4);
        let (tx, mut rx, path) = loop_over_logging(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        let records = spills(&path);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["trigger"], "repetition");
        assert_eq!(records[0]["from"], "local");
        assert_eq!(records[0]["to"], "frontier");
        assert_eq!(records[0]["policy"], "escalate");
        assert_eq!(records[0]["turn"], 1);
    }

    #[tokio::test]
    async fn the_log_carries_the_numbers_a_threshold_is_moved_with() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = looping_into_a_second(4);
        let (tx, mut rx, path) = loop_over_logging(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        let record = &spills(&path)[0];
        assert_eq!(record["repeats"]["lines"], 4);
        assert_eq!(record["repeats"]["allowed"], 4);
        assert_eq!(record["steps"]["used"], 1);
        assert_eq!(record["steps"]["allowed"], DEFAULT_MAX_STEPS);
        // The allowances themselves, because these are the numbers that get
        // changed and the counts mean nothing without them.
        assert!(record["wait"]["first_token_ms"].as_u64().unwrap() > 0);
        assert!(record["wait"]["idle_ms"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn a_consult_is_logged_as_a_consult_rather_than_a_handover() {
        // Which of the two ran is the question the log exists to answer, and
        // they are indistinguishable from the trigger alone.
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _, _) = consultable(2);
        let (tx, mut rx, path) = loop_over_logging(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        let records = spills(&path);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["policy"], "consult");
        assert_eq!(records[0]["from"], "Local");
        assert_eq!(records[0]["to"], "DeepSeek");
    }

    #[tokio::test]
    async fn a_turn_that_ended_for_want_of_a_tier_logs_no_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx, path) = loop_over_logging(dir.path(), looping_alone(4));

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        let records = spills(&path);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["policy"], "ended");
        assert!(records[0]["to"].is_null(), "{}", records[0]);
        assert_eq!(records[0]["trigger"], "repetition");
    }

    #[tokio::test]
    async fn a_clean_turn_writes_nothing_to_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (chain, _) = one_tier("an ordinary answer");
        let (tx, mut rx, path) = loop_over_logging(dir.path(), chain);

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = collect(&mut rx).await;

        assert!(
            !path.exists(),
            "the log is a record of spills, so a clean turn creates no file"
        );
    }

    #[tokio::test]
    async fn two_spills_in_one_session_are_two_records() {
        // Appended, not overwritten: the file is what a pattern is read from.
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx, path) = loop_over_logging(dir.path(), looping_alone(4));

        for _ in 0..2 {
            tx.send(Command::Prompt("go".to_string())).expect("send");
            let _ = collect(&mut rx).await;
        }

        assert_eq!(spills(&path).len(), 2);
    }

    #[tokio::test]
    async fn an_unwritable_log_is_reported_once_and_the_turn_carries_on() {
        // A log that has quietly stopped is worse than no log, because the
        // thresholds would go on being tuned from a file that is not growing.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spills.jsonl");
        // A directory where the file should be: whatever the platform, this
        // cannot be opened for appending.
        std::fs::create_dir_all(&path).expect("make a directory");

        let mut config = config(dir.path(), DEFAULT_MAX_STEPS);
        config.log = Some(crate::stalls::SpillLog::at(path));
        let (tx, mut rx) = spawn(
            config,
            looping_alone(4),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        let mut all = Vec::new();
        for _ in 0..2 {
            tx.send(Command::Prompt("go".to_string())).expect("send");
            all.extend(collect(&mut rx).await);
        }

        // Two spills happened, and the complaint is made once: a full disk
        // would otherwise print the same line on every stall of the session.
        let complaints: Vec<String> = notices(&all)
            .into_iter()
            .filter(|note| note.contains("spill log"))
            .collect();
        assert_eq!(complaints.len(), 1, "{complaints:?}");
        assert!(
            complaints[0].contains("spills.jsonl"),
            "it should say which file: {complaints:?}"
        );
    }

    #[tokio::test]
    async fn a_log_that_cannot_be_written_does_not_stop_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spills.jsonl");
        std::fs::create_dir_all(&path).expect("make a directory");

        let mut config = config(dir.path(), DEFAULT_MAX_STEPS);
        config.log = Some(crate::stalls::SpillLog::at(path));
        let (tx, mut rx) = spawn(
            config,
            one_tier("the answer anyway").0,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = collect(&mut rx).await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Finished { .. })),
            "the turn is not the log's business: {events:?}"
        );
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
        // The second tier is the helper's and says nothing about its policy, so
        // this is the default being named — which is the point of the message.
        assert!(said.contains("DeepSeek consult"), "{said}");
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

    #[tokio::test]
    async fn new_returns_to_the_first_tier_and_forgets_cli_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tiers, first, second) = two_tiers();
        let chain = FallbackChain::new(tiers, true).expect("a chain");
        let (tx, mut rx) = loop_over(dir.path(), chain);

        tx.send(Command::Escalate).expect("send");
        let _ = collect(&mut rx).await;

        tx.send(Command::New).expect("send");
        let events = collect(&mut rx).await;
        assert!(
            notices(&events).iter().any(|n| n.contains("new session")),
            "{:?}",
            notices(&events)
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Switched { to } if to.starts_with("Local"))),
            "the rail has to go back to the first tier: {events:?}"
        );
        assert!(first.forgotten() >= 1 || second.forgotten() >= 1);
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

    /// Emits one line over and over, which is the shape the repetition detector
    /// exists to catch. `times` below the tier's allowance ends in an answer and
    /// is how a near miss is provoked.
    struct Loops {
        line: String,
        times: usize,
        requests: std::sync::atomic::AtomicUsize,
    }

    impl Loops {
        fn new(line: &str, times: usize) -> Arc<Self> {
            Arc::new(Self {
                line: line.to_string(),
                times,
                requests: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn requests(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for Loops {
        fn describe(&self) -> String {
            "loops".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for _ in 0..self.times {
                let _ = events.send(StreamEvent::Text(format!("{}\n", self.line)));
            }
            Ok(TurnSummary {
                text: self.line.clone(),
                stop_reason: Some("end_turn".to_string()),
                ..TurnSummary::default()
            })
        }
    }

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
    async fn the_default_policy_consults_and_keeps_the_driver() {
        // The default, end to end and leaning on nothing: neither tier says a
        // word about its policy, and the one that stalls is asked a question
        // instead of losing the turn.
        //
        // What it is worth is what escalating would have cost. The turn would go
        // to the tier below whole, and because a spilled session stays spilled,
        // the cheap model would be gone for every turn after it — one bad answer
        // bought at the price of the rest of the session. That is the mistake
        // the default is chosen to avoid.
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);
        let consultant = ScriptedProvider::new(vec![answer("Use a HashMap instead.")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (driver.clone(), Limits::default()),
            (consultant.clone(), Limits::default()),
        ];
        let (tx, mut rx) = loop_over(dir.path(), chain_of(tiers));

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert_eq!(
            consulted(&events),
            vec![("Tier 0".to_string(), "Tier 1".to_string())],
            "silence in the configuration now means consult: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Escalated { .. })),
            "and the driver keeps the turn rather than handing it over: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(AgentEvent::Finished { .. })),
            "the driver should have finished it: {:?}",
            events.last()
        );
        // Two requests at the driver: the one that looped, and the one after the
        // advice. One at the consultant, which is the whole saving.
        assert_eq!(driver.request_count(), 2);
        assert_eq!(consultant.request_count(), 1);
    }

    #[tokio::test]
    async fn escalate_is_still_what_a_tier_that_asks_for_it_gets() {
        // Escalation is opt-in now, which makes it the thing to keep honest: a
        // tier that asks for it must hand the turn over whole, not ask a question
        // first and then hand it over anyway.
        let dir = tempfile::tempdir().expect("tempdir");
        let looping = ScriptedProvider::new(vec![looping_answer()]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> =
            vec![(looping, Limits::default()), (healthy, Limits::default())];
        let (tx, mut rx) = loop_over(dir.path(), chain_of_with(tiers, OnStuck::Escalate));

        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            consulted(&events).is_empty(),
            "a tier set to escalate must not consult: {events:?}"
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

    /// A tier that answers as a driver but must never be consulted.
    struct RefusesConsult;

    #[async_trait]
    impl Provider for RefusesConsult {
        fn describe(&self) -> String {
            "refuser".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            Ok(answer("escalated here"))
        }

        fn consult_refusal(&self) -> Option<String> {
            Some("it runs its own tools and has no read-only mode configured".to_string())
        }

        async fn consult(
            &self,
            _request: ChatRequest,
            _events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            panic!("a consultant that cannot run read-only must never be asked")
        }
    }

    #[tokio::test]
    async fn a_consultant_that_cannot_run_read_only_escalates_instead() {
        // A CLI with tools of its own and no read-only flag is the hole this
        // closes: "please do not act" is not a guarantee, so it is never asked.
        // The turn escalates, which is where it would have gone without consult
        // at all, and the reason is stated rather than left to be guessed.
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = ScriptedProvider::new(vec![looping_answer()]);

        let mut first = Tier::new(
            "Local".to_string(),
            "m0".to_string(),
            driver,
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        let second = Tier::new(
            "Grok".to_string(),
            "m1".to_string(),
            Arc::new(RefusesConsult),
            Limits::default(),
        );
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("make the tests pass".to_string()))
            .expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            consulted(&events).is_empty(),
            "nothing may be consulted: {events:?}"
        );
        let said = notices(&events).join("\n");
        assert!(said.contains("Grok cannot be consulted"), "{said}");
        assert!(
            said.contains("read-only"),
            "the reason should be named, not just the refusal: {said}"
        );
        assert!(
            escalation(&events).is_some(),
            "the turn must be handed over instead: {events:?}"
        );
    }

    // ---- persistence ------------------------------------------------------

    /// A provider that holds a conversation, the way a CLI tier does.
    struct SessionedProvider(Mutex<Option<String>>);

    #[async_trait]
    impl Provider for SessionedProvider {
        fn describe(&self) -> String {
            "sessioned".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            Ok(answer("ok"))
        }

        fn session_id(&self) -> Option<String> {
            self.0.lock().expect("lock").clone()
        }

        fn set_session(&self, id: Option<String>) {
            *self.0.lock().expect("lock") = id;
        }
    }

    /// A store with nowhere to write, for checking the failure path.
    fn store_in(dir: &std::path::Path) -> SessionStore {
        SessionStore::at(dir.to_path_buf(), dir.to_path_buf())
    }

    /// Run one prompt through a chain that has somewhere to save.
    async fn saved_turn(
        dir: &std::path::Path,
        chain: FallbackChain,
        prompt: &str,
    ) -> (Vec<AgentEvent>, SessionStore) {
        let mut agent_config = config(dir, DEFAULT_MAX_STEPS);
        agent_config.store = Some(store_in(dir));

        let (tx, mut rx) = spawn_seeded(
            agent_config,
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
            None,
        );
        tx.send(Command::Prompt(prompt.to_string())).expect("send");
        let events = drain_from(&mut rx).await;
        // The write happens after the terminal event is sent, in the same poll
        // of the agent task, so it has already run — but yielding makes that a
        // fact of the schedule rather than an assumption about it.
        tokio::task::yield_now().await;
        (events, store_in(dir))
    }

    #[test]
    fn a_snapshot_captures_the_chain_the_mode_and_the_holds_of_each_tier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessioned = Arc::new(SessionedProvider(Mutex::new(Some("sess-1".to_string()))));

        let mut tier = Tier::with_id("grok", "Grok", "m", sessioned, Limits::default());
        tier.on_stuck = OnStuck::Escalate;
        let mut chain = FallbackChain::new(vec![tier], false).expect("a chain");
        // The tier's own policy and the session's choice are different things,
        // and only the second is session state: the first lives in the config
        // file and is re-read on every start.
        chain.set_on_stuck(Some(OnStuck::Consult));

        let mut session = Session::with_system_prompt("be brief");
        session.push(ChatMessage::user("hello"));
        let state = Loop {
            session,
            chain,
            mode: Mode::Plan,
            last: None,
            undo: UndoStack::default(),
            allow: AllowRules::default(),
            log_warned: false,
            session_id: "snap".to_string(),
            session_title: "hello".to_string(),
            session_named: false,
        };

        let agent_config = AgentConfig {
            workspace: dir.path().to_path_buf(),
            max_steps: DEFAULT_MAX_STEPS,
            cancel: Canceller::default(),
            store: None,
            log: None,
            // A fixture, so it behaves as though the configuration said nothing:
            // no rule covers anything, and there is no file for `/allow save` to
            // write to. The tests that are about either set what they need.
            allow_shell: Vec::new(),
            origin: Origin::default(),
        };
        let file = snapshot(&agent_config, &state);

        assert_eq!(file.mode, Mode::Plan);
        assert_eq!(file.workspace, dir.path());
        assert_eq!(file.active_tier.as_deref(), Some("grok"));
        assert_eq!(
            file.on_stuck,
            Some(OnStuck::Consult),
            "the session's choice is saved; each tier's own policy is not session state"
        );
        assert!(
            !file
                .messages
                .iter()
                .any(|m| m.role == crate::session::Role::System),
            "the prompt is rebuilt on load, not stored"
        );
        assert_eq!(file.messages.len(), 1, "just the one user turn");
        assert_eq!(
            file.cli_sessions.get("grok").map(String::as_str),
            Some("sess-1"),
            "a CLI's own conversation has to travel with the session"
        );
    }

    #[tokio::test]
    async fn a_finished_turn_is_on_disk_by_the_time_it_is_reported() {
        // This is the whole feature: quit, come back, and the conversation is
        // there. A turn that ends without saving is a turn that is lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let scripted = ScriptedProvider::new(vec![answer("all done")]);

        let (events, store) = saved_turn(
            dir.path(),
            single_tier(scripted.clone()),
            "make the tests pass",
        )
        .await;

        assert!(
            matches!(events.last(), Some(AgentEvent::Finished { .. })),
            "{events:?}"
        );

        let saved = store.load().expect("the session should have been saved");
        let text: String = saved.messages.iter().map(|m| m.content.clone()).collect();
        assert!(text.contains("make the tests pass"), "{text}");
        assert!(text.contains("all done"), "{text}");
    }

    #[tokio::test]
    async fn clearing_the_conversation_forgets_it_on_disk_too() {
        // `/clear` has to be more than a wiped screen: coming back to the
        // conversation it was supposed to have dropped would make it a lie.
        let dir = tempfile::tempdir().expect("tempdir");
        let scripted = ScriptedProvider::new(vec![answer("all done")]);
        let (_, store) = saved_turn(
            dir.path(),
            single_tier(scripted.clone()),
            "make the tests pass",
        )
        .await;
        assert!(store.load().is_some(), "there is something to clear");

        let mut agent_config = config(dir.path(), DEFAULT_MAX_STEPS);
        agent_config.store = Some(store.clone());
        let (tx, mut rx) = spawn(
            agent_config,
            single_tier(scripted.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Clear).expect("send");

        // `/clear` answers with a notice and no terminal event, so wait for the
        // notice rather than for a turn to finish.
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the clear should be acknowledged")
            .expect("an event");
        tokio::task::yield_now().await;

        let saved = store.load().expect("the slot remains in history");
        assert!(
            saved.is_empty(),
            "a cleared session has nothing to resume as a conversation"
        );
    }

    /// Collect events until the stream goes quiet.
    async fn drain_quiet(rx: &mut UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(150), rx.recv()).await
        {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn a_session_that_cannot_be_saved_says_so_without_stopping() {
        // Losing the session is worth saying out loud; it is never a reason to
        // abandon a turn that is working.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "in the way").expect("write");

        let scripted = ScriptedProvider::new(vec![answer("all done")]);
        let mut agent_config = config(dir.path(), DEFAULT_MAX_STEPS);
        // A file where the directory should be, so the write cannot succeed.
        agent_config.store = Some(SessionStore::at(blocker.clone(), dir.path().to_path_buf()));

        let (tx, mut rx) = spawn(
            agent_config,
            single_tier(scripted.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_quiet(&mut rx).await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Finished { .. })),
            "the turn must still finish: {events:?}"
        );
        let said = notices(&events).join("\n");
        assert!(said.contains("could not be saved"), "{said}");
    }

    #[tokio::test]
    async fn a_seeded_run_starts_from_the_saved_conversation() {
        // What resuming means for the model: the earlier turns go out ahead of
        // the new one, so the work is not explained again.
        let dir = tempfile::tempdir().expect("tempdir");
        let scripted = ScriptedProvider::new(vec![answer("carrying on")]);

        let seed = Seed {
            messages: vec![
                ChatMessage::user("earlier question"),
                ChatMessage::assistant("earlier answer", Vec::new()),
            ],
            mode: Mode::Build,
            id: "seed-1".to_string(),
            title: "earlier question".to_string(),
            named: false,
        };

        let (tx, mut rx) = spawn_seeded(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(scripted.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
            Some(seed),
        );
        tx.send(Command::Prompt("and now?".to_string()))
            .expect("send");
        let _ = drain_from(&mut rx).await;

        let sent = scripted.request(0).messages;
        assert_eq!(
            sent[0].role,
            crate::session::Role::System,
            "a conversation always opens with its instructions"
        );
        let text: String = sent.iter().map(|m| m.content.clone()).collect();
        assert!(text.contains("earlier question"), "{text}");
        assert!(text.contains("earlier answer"), "{text}");
        assert!(text.contains("and now?"), "{text}");
    }

    #[tokio::test]
    async fn a_seeded_run_carries_the_saved_mode_into_the_prompt() {
        // A conversation resumed in plan mode must be told it is read-only, or
        // the first turn after a restart quietly gains write powers.
        let dir = tempfile::tempdir().expect("tempdir");
        let scripted = ScriptedProvider::new(vec![answer("planning")]);

        let seed = Seed {
            messages: vec![ChatMessage::user("earlier")],
            mode: Mode::Plan,
            id: "seed-2".to_string(),
            title: "earlier".to_string(),
            named: false,
        };
        let (tx, mut rx) = spawn_seeded(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(scripted.clone()),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
            Some(seed),
        );
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;

        let prompt = &scripted.request(0).messages[0].content;
        assert!(
            prompt.contains("PLAN MODE"),
            "the restored mode's prompt must lead: {prompt}"
        );
    }

    // ---- undo -------------------------------------------------------------

    /// One approved `write_file`, driven through the loop.
    fn writing_over(path: &str, contents: &str) -> Vec<TurnSummary> {
        vec![
            calls_tool(
                "write_file",
                &format!(r#"{{"path":"{path}","content":"{contents}"}}"#),
            ),
            answer("wrote it"),
        ]
    }

    #[tokio::test]
    async fn an_approved_write_can_be_put_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "the original\n").expect("write");

        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(ScriptedProvider::new(writing_over("note.txt", "junk"))),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("do it".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;
        assert_eq!(std::fs::read_to_string(&note).expect("read"), "junk");

        tx.send(Command::Undo).expect("send");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("restored"), "{said}");
        assert!(said.contains("write_file"), "{said}");
        assert_eq!(
            std::fs::read_to_string(&note).expect("read"),
            "the original\n",
            "the write should have been reversed"
        );
    }

    #[tokio::test]
    async fn one_undo_reaches_one_write_and_then_says_so() {
        // Not a stack: a second `/undo` has to be honest that there is nothing
        // left, rather than flipping the file back and forth.
        let dir = tempfile::tempdir().expect("tempdir");
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "the original\n").expect("write");

        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(ScriptedProvider::new(writing_over("note.txt", "junk"))),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("do it".to_string())).expect("send");
        let _ = drain_from(&mut rx).await;

        tx.send(Command::Undo).expect("send");
        let _ = drain_quiet(&mut rx).await;
        tx.send(Command::Undo).expect("send");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("nothing to undo"), "{said}");
        assert_eq!(
            std::fs::read_to_string(&note).expect("read"),
            "the original\n",
            "and it must not have touched the file a second time"
        );
    }

    #[tokio::test]
    async fn the_write_a_spilled_tier_made_can_still_be_put_back() {
        // The pairing this exists for: the cheap model wrote junk, then looped
        // and was abandoned. The junk is still on disk, and the tier that took
        // over has no idea it is there.
        let dir = tempfile::tempdir().expect("tempdir");
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "the original\n").expect("write");

        let stubborn = ScriptedProvider::new(vec![
            calls_tool("write_file", r#"{"path":"note.txt","content":"junk"}"#),
            // Then it loses the plot and is spilled past.
            looping_answer(),
        ]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let chain = chain_of_with(
            vec![(stubborn, Limits::default()), (healthy, Limits::default())],
            OnStuck::Escalate,
        );
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("do it".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            escalation(&events).is_some(),
            "the tier should have spilled: {events:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&note).expect("read"),
            "junk",
            "the abandoned tier's side effect stands, which is why undo matters"
        );

        tx.send(Command::Undo).expect("send");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("restored"), "{said}");
        assert_eq!(
            std::fs::read_to_string(&note).expect("read"),
            "the original\n",
            "the junk from the tier that spilled should be gone"
        );
    }

    #[tokio::test]
    async fn undoing_with_nothing_recorded_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(ScriptedProvider::new(Vec::new())),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Undo).expect("send");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("nothing to undo"), "{said}");
    }

    #[tokio::test]
    async fn a_shell_command_is_never_offered_for_undo() {
        // It can do anything, so there is no honest reversal. `/undo` after one
        // must not claim to have put anything back.
        let dir = tempfile::tempdir().expect("tempdir");
        let (shell, flag) = crate::provider::cli::portable_shell();

        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(ScriptedProvider::new(vec![
                calls_tool(
                    "run_shell",
                    &format!(r#"{{"command":"{shell} {flag} 'echo hi'"}}"#),
                ),
                answer("ran it"),
            ])),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("run it".to_string()))
            .expect("send");
        let _ = drain_from(&mut rx).await;

        tx.send(Command::Undo).expect("send");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("nothing to undo"), "{said}");
    }

    // ---- what may run without being asked ----------------------------------

    #[tokio::test]
    async fn a_read_only_command_runs_without_being_asked_about() {
        // The default list, through the real loop: the approver is never reached,
        // which is the whole claim. Asserted on what the approver recorded rather
        // than on the absence of an event, because a modal that was never shown
        // and one that was answered instantly look the same from the transcript.
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        let (events, _) = run_one_with(
            config_with_default_rules(dir.path()),
            vec![shell_call("git status"), step_two()],
            approver.clone(),
            "what changed?",
        )
        .await;

        assert!(
            approver.asked.lock().expect("lock").is_empty(),
            "a read-only command should not reach the approver"
        );
        // And it is not silent: what ran without asking is said out loud, since a
        // rule is otherwise invisible by design.
        let said = notices(&events).join("\n");
        assert!(said.contains("runs without asking"), "{said}");
        assert!(said.contains("rule: git status"), "{said}");
    }

    #[tokio::test]
    async fn a_command_the_defaults_do_not_cover_is_still_asked_about() {
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        run_one_with(
            config_with_default_rules(dir.path()),
            vec![shell_call("cargo test"), step_two()],
            approver.clone(),
            "run the tests",
        )
        .await;

        let asked = approver.asked.lock().expect("lock").clone();
        assert_eq!(asked.len(), 1, "the modal should have been shown once");
        assert_eq!(asked[0].0, shell::NAME);
        assert!(asked[0].1.contains("cargo test"), "{}", asked[0].1);
    }

    #[tokio::test]
    async fn an_empty_list_asks_about_everything_including_reading() {
        // The escape hatch, and the thing that makes shipping a default list
        // defensible: anyone who wants the old behaviour has it in one line.
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        run_one_with(
            config(dir.path(), DEFAULT_MAX_STEPS),
            vec![shell_call("git status"), step_two()],
            approver.clone(),
            "what changed?",
        )
        .await;

        assert_eq!(
            approver.asked.lock().expect("lock").len(),
            1,
            "with no rules, everything asks"
        );
    }

    #[tokio::test]
    async fn a_command_holding_a_shell_operator_is_asked_about_even_when_it_starts_well() {
        // The case the whole word-prefix design exists for: a rule for `git
        // status` must not cover a line that begins with it and goes on to do
        // something else.
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        run_one_with(
            config_with_default_rules(dir.path()),
            vec![shell_call("git status; rm -rf important.txt"), step_two()],
            approver.clone(),
            "go",
        )
        .await;

        assert_eq!(approver.asked.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn a_rule_never_covers_a_file_write() {
        // A rule names a command. A write is not one, and the diff it shows is the
        // whole reason it asks — so nothing about the rules can make it quiet.
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        let mut config = config_with_default_rules(dir.path());
        // A rule that would cover the *path* if anything were matching text
        // rather than a command.
        config.allow_shell.push("notes.txt".to_string());

        run_one_with(
            config,
            vec![write_call("notes.txt"), step_two()],
            approver.clone(),
            "write it",
        )
        .await;

        assert_eq!(approver.asked.lock().expect("lock").len(), 1);
        assert!(dir.path().join("notes.txt").exists(), "and it still ran");
    }

    #[tokio::test]
    async fn plan_mode_refuses_a_shell_command_a_rule_would_have_allowed() {
        // The guarantee that outranks a rule: read-only is read-only. The mode's
        // ceiling is checked before a rule is consulted, so a rule cannot widen
        // what plan mode may do.
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());

        let mut config = config_with_default_rules(dir.path());
        config.max_steps = DEFAULT_MAX_STEPS;
        let provider = ScriptedProvider::new(vec![shell_call("git status"), step_two()]);
        let (tx, rx) = spawn(
            config,
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            approver.clone(),
        );
        tx.send(Command::SetMode(Mode::Plan)).expect("set mode");
        tx.send(Command::Prompt("go".to_string())).expect("prompt");

        let events = drain(rx).await;
        let said = notices(&events).join("\n");

        assert!(said.contains("plan mode is read-only"), "{said}");
        assert!(
            approver.asked.lock().expect("lock").is_empty(),
            "and it was refused before the approver was even reached"
        );
    }

    #[tokio::test]
    async fn allow_sticks_a_rule_for_the_session_and_clear_takes_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let approver = Arc::new(AlwaysApprove::default());
        let provider = ScriptedProvider::new(vec![
            shell_call("cargo test"),
            step_two(),
            shell_call("cargo test"),
            step_two(),
        ]);
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            approver.clone(),
        );

        // With no rules, `cargo test` asks.
        tx.send(Command::Prompt("run the tests".to_string()))
            .expect("prompt");
        drain_from(&mut rx).await;
        assert_eq!(approver.asked.lock().expect("lock").len(), 1);

        // Sticking it is answered with a notice, and says what it covers.
        tx.send(Command::Allow(AllowChange::Add("cargo test".to_string())))
            .expect("allow");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(said.contains("this session will run"), "{said}");
        assert!(said.contains("cargo test"), "{said}");

        // Now the same command runs without asking.
        tx.send(Command::Prompt("run the tests again".to_string()))
            .expect("prompt");
        drain_from(&mut rx).await;
        assert_eq!(
            approver.asked.lock().expect("lock").len(),
            1,
            "the second one should have been covered by the rule"
        );

        // And `/allow clear` puts the question back.
        tx.send(Command::Allow(AllowChange::Clear)).expect("clear");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(said.contains("1 session rule(s) dropped"), "{said}");
    }

    #[tokio::test]
    async fn allow_lists_the_rules_in_force_and_marks_the_session_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![step_two()]);
        let (tx, mut rx) = spawn(
            config_with_default_rules(dir.path()),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Allow(AllowChange::List)).expect("list");
        let listed = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(
            listed.contains("shell commands that run without asking"),
            "{listed}"
        );
        assert!(listed.contains("git status"), "{listed}");
        assert!(
            listed.contains("git rev-parse"),
            "the longer rules too: {listed}"
        );
        assert!(!listed.contains("* "), "nothing stuck yet: {listed}");

        tx.send(Command::Allow(AllowChange::Add("cargo test".to_string())))
            .expect("add");
        drain_quiet(&mut rx).await;
        tx.send(Command::Allow(AllowChange::List))
            .expect("list again");
        let listed = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(listed.contains("* cargo test"), "{listed}");
        assert!(listed.contains("stuck for this session"), "{listed}");
    }

    #[tokio::test]
    async fn a_rule_that_is_not_a_rule_is_refused_with_the_reason() {
        // A rule is matched against a command with no operators in it, so one
        // holding an operator could never match: say so rather than accept
        // something that would silently never apply.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![step_two()]);
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Allow(AllowChange::Add("ls; rm -rf ~".to_string())))
            .expect("add");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("✗"), "{said}");
        assert!(said.contains("cannot contain"), "{said}");
    }

    #[tokio::test]
    async fn saving_with_no_configuration_file_keeps_the_rule_and_says_why_it_could_not_write() {
        // `Origin::Text` is a run with no file at all — the same honesty as the
        // rest of the save path: refuse, name the reason, and do not pretend the
        // rule is permanent.
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = ScriptedProvider::new(vec![step_two()]);
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Allow(AllowChange::Save("cargo test".to_string())))
            .expect("save");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("no configuration file"), "{said}");
        assert!(said.contains("in force for this session"), "{said}");
    }

    #[tokio::test]
    async fn saving_writes_every_rule_in_force_including_the_defaults() {
        // The key *is* the list, so writing only the new rule would drop the
        // read-only set: next start would ask about `ls`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\nworkspace = \"~\"\n").expect("write");

        let provider = ScriptedProvider::new(vec![step_two()]);
        // The real defaults, because the point of the test is what happens to
        // them: the key *is* the list, so writing one rule would drop them.
        let mut config = config_with_default_rules(dir.path());
        config.origin = Origin::Given(path.clone());
        let (tx, mut rx) = spawn(
            config,
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Allow(AllowChange::Save("cargo test".to_string())))
            .expect("save");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");

        assert!(said.contains("rules written to"), "{said}");
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.contains("git status"), "{written}");
        assert!(written.contains("cargo test"), "{written}");
        // And what was written is what a reload gives back.
        let reloaded = Config::load(Some(&path)).expect("valid");
        assert!(
            reloaded
                .general
                .allow_shell
                .contains(&"cargo test".to_string())
        );
        assert!(reloaded.general.allow_shell.contains(&"ls".to_string()));
    }

    // ---- the stack ---------------------------------------------------------

    #[tokio::test]
    async fn undo_reaches_back_through_several_writes_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.txt");

        let write = |content: &str| TurnSummary {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_write".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({ "path": "note.txt", "content": content })
                    .to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        };

        let provider = ScriptedProvider::new(vec![write("first"), write("second"), step_two()]);
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            single_tier(provider),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("write it twice".to_string()))
            .expect("prompt");
        drain_from(&mut rx).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        // The newest first: back to the first write.
        tx.send(Command::Undo).expect("undo");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert!(
            said.contains("1 more write can still be put back"),
            "{said}"
        );

        // And then past it: back to no file at all.
        tx.send(Command::Undo).expect("undo again");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(
            !path.exists(),
            "the first write created it, so undoing removes it"
        );
        assert!(said.contains("last write on the stack"), "{said}");

        // A third says there is nothing, rather than flipping anything back.
        tx.send(Command::Undo).expect("undo once more");
        let said = notices(&drain_quiet(&mut rx).await).join("\n");
        assert!(said.contains("nothing to undo"), "{said}");
    }

    // ---- text is never a side effect ---------------------------------------

    /// Says something and then falls over. What an abandoned turn looks like from
    /// the outside: the text arrived, the turn did not finish.
    struct SaysThenFails {
        text: String,
    }

    #[async_trait]
    impl Provider for SaysThenFails {
        fn describe(&self) -> String {
            "says-then-fails".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            let _ = events.send(StreamEvent::Text(self.text.clone()));
            Err(ProviderError::Broken {
                target: "says-then-fails".to_string(),
                detail: "the connection went away mid-answer".to_string(),
            })
        }
    }

    /// Text that reads exactly like an applied patch and a command, and must stay
    /// text all the same.
    fn a_patch_in_prose() -> String {
        "Here is the fix, already applied:\n\
         ```diff\n\
         --- a/notes.txt\n\
         +++ b/notes.txt\n\
         @@ -1 +1 @@\n\
         -original\n\
         +overwritten\n\
         ```\n\
         I have written the file. I also ran: rm -rf /tmp/spill-nothing\n"
            .to_string()
    }

    #[tokio::test]
    async fn a_patch_in_an_abandoned_turns_text_is_never_applied() {
        // The guarantee, from the outside. A tier's text is never a source of a
        // side effect: what can change the world is a structured tool call that was
        // approved and ran, and this turn had neither — so nothing happened, and
        // the turn was handed over as though it had said nothing at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let note = dir.path().join("notes.txt");
        std::fs::write(&note, "original\n").expect("write");

        let abandoned = Arc::new(SaysThenFails {
            text: a_patch_in_prose(),
        });
        let next = ScriptedProvider::new(vec![answer("done")]);
        let chain = chain_of_with(
            vec![
                (abandoned as Arc<dyn Provider>, Limits::default()),
                (next.clone(), Limits::default()),
            ],
            OnStuck::Escalate,
        );
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain,
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("fix the file".to_string()))
            .expect("send");
        let events = drain(rx).await;

        assert!(
            escalation(&events).is_some(),
            "the turn should have been handed over rather than finished"
        );
        assert_eq!(
            std::fs::read_to_string(&note).expect("read"),
            "original\n",
            "a patch in prose is prose"
        );
        assert!(
            !std::path::Path::new("/tmp/spill-nothing").exists(),
            "and a command in prose is prose too"
        );

        // And the next tier never sees it: the abandoned attempt is rolled back to
        // the checkpoint before another model reads anything.
        let sent: String = next
            .request(0)
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect();
        assert!(
            !sent.contains("Here is the fix"),
            "the abandoned text should not reach the next tier: {sent}"
        );
    }

    #[tokio::test]
    async fn a_consultants_tool_call_is_ignored_because_only_its_text_is_read() {
        // The consultant is offered no tools, but a model can still emit one. Only
        // the answer's text is read, so this changes nothing — the boundary that
        // makes "the consultant's answer is prose" a fact rather than a hope.
        let dir = tempfile::tempdir().expect("tempdir");
        let escaped = dir.path().join("escaped.txt");

        let driver = ScriptedProvider::new(vec![looping_answer(), answer("recovered")]);
        let consultant = ScriptedProvider::new(vec![TurnSummary {
            text: "Ignore the failing test and overwrite the file instead.".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_escape".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": escaped.display().to_string(),
                    "content": "written by a consultant",
                })
                .to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        }]);

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
        let chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        let (tx, mut rx) = loop_over(dir.path(), chain);
        tx.send(Command::Prompt("go".to_string())).expect("send");
        let events = drain_from(&mut rx).await;

        assert!(
            !escaped.exists(),
            "a consultant's tool call must not be run: {}",
            escaped.display()
        );
        // What the driver was handed is the answer's text, and nothing else.
        let sent: String = driver
            .request(1)
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect();
        assert!(
            sent.contains("overwrite the file instead"),
            "the answer is fed back as prose: {sent}"
        );
        assert!(
            !consulted(&events).is_empty(),
            "and the consult happened at all: {:?}",
            consulted(&events)
        );
    }

    // ---- the two detectors, and which one is being asked --------------------

    /// A turn that says something new and asks for the same thing again.
    fn narrates_and_repeats_the_call(prose: &str) -> TurnSummary {
        TurnSummary {
            text: prose.to_string(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"note.txt"}"#.to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        }
    }

    #[tokio::test]
    async fn a_tier_that_keeps_asking_for_the_same_thing_in_new_words_is_caught() {
        // The case the two detectors split between them, and the one that says
        // which is which: the prose is different every step, so the *text* loop
        // detector has nothing to go on, and the model is still stuck. Only the
        // identical-call detector can catch this, and a refactor that fed the
        // wrong thing to either one would show up here.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");

        let stubborn = ScriptedProvider::new(vec![
            narrates_and_repeats_the_call("Let me look at the note."),
            narrates_and_repeats_the_call("I will read that file again."),
            narrates_and_repeats_the_call("Reading the same file once more."),
            narrates_and_repeats_the_call("Checking that note a final time."),
        ]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (stubborn.clone(), Limits::default()),
            (healthy, Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of_with(tiers, OnStuck::Escalate),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("read the note".to_string()))
            .expect("send");
        let events = drain(rx).await;

        let verdict = verdicts(&events)[0];
        assert!(
            matches!(
                verdict.reason,
                StuckReason::RepeatedToolCall { ref tool, times: 4 } if tool == "read_file"
            ),
            "the call loop is what is wrong here: {:?}",
            verdict.reason
        );
        assert_eq!(verdict.counters.progress.same_run, 4);
        assert!(
            verdict.counters.repetition.span_repeats < verdict.counters.repetition.threshold,
            "the prose never repeated, so the text detector should not be near it: {:?}",
            verdict.counters.repetition
        );
        assert!(
            !matches!(verdict.reason, StuckReason::Repetition { .. }),
            "and it must not be reported as a text loop: {:?}",
            verdict.reason
        );
    }

    #[tokio::test]
    async fn a_model_written_out_of_arguments_is_still_stuck() {
        // The same loop, spelled differently each time. Sampling the call twice can
        // move the space or reorder the keys, and neither makes it a different
        // request — so the detector has to be reading the arguments rather than the
        // text of them, or a model can rewrite its way out of being caught.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");

        let spelled = |arguments: &str| TurnSummary {
            text: "reading".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: arguments.to_string(),
            }],
            stop_reason: Some("tool_calls".to_string()),
            ..TurnSummary::default()
        };

        let stubborn = ScriptedProvider::new(vec![
            spelled(r#"{"path":"note.txt"}"#),
            spelled(r#"{"path": "note.txt"}"#),
            spelled(r#"{ "path" : "note.txt" }"#),
            spelled(r#"{"path":"note.txt"}"#),
        ]);
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tiers: Vec<(Arc<dyn Provider>, Limits)> = vec![
            (stubborn.clone(), Limits::default()),
            (healthy, Limits::default()),
        ];
        let (tx, rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of_with(tiers, OnStuck::Escalate),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );
        tx.send(Command::Prompt("read the note".to_string()))
            .expect("send");
        let events = drain(rx).await;

        let verdict = verdicts(&events)[0];
        assert!(
            matches!(
                verdict.reason,
                StuckReason::RepeatedToolCall { times: 4, .. }
            ),
            "four readings of one file, however they were written: {:?}",
            verdict.reason
        );
    }

    /// Answers once, with a tool call, and then never answers again.
    struct AnswersThenHangs {
        first: TurnSummary,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl AnswersThenHangs {
        fn new(first: TurnSummary) -> Arc<Self> {
            Arc::new(Self {
                first,
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Provider for AnswersThenHangs {
        fn describe(&self) -> String {
            "answers-then-hangs".to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = events.send(StreamEvent::Text("looking at the note".to_string()));
                return Ok(self.first.clone());
            }
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }
    }

    #[tokio::test]
    async fn silence_after_a_tool_is_judged_against_the_idle_budget() {
        // Silence means opposite things either side of the first frame, and the
        // interesting side is the second one: a tier that has answered, run a tool,
        // and then gone quiet is wedged, where one that has not answered yet may
        // still be loading its weights. The budgets are set far apart so the two
        // readings are unmistakable — the generous one would take two seconds and
        // the idle one fifty milliseconds — and the counters say which was used.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");

        let hanging = AnswersThenHangs::new(calls_tool("read_file", r#"{"path":"note.txt"}"#));
        let healthy = ScriptedProvider::new(vec![answer("recovered")]);

        let tight = Limits {
            first_token_timeout_ms: 2_000,
            idle_timeout_ms: 50,
            max_repeat_run: 4,
        };
        let tiers: Vec<(Arc<dyn Provider>, Limits)> =
            vec![(hanging.clone(), tight), (healthy, Limits::default())];
        let (tx, mut rx) = spawn(
            config(dir.path(), DEFAULT_MAX_STEPS),
            chain_of_with(tiers, OnStuck::Escalate),
            Arc::new(Registry::with_default_tools()),
            Arc::new(AlwaysApprove::default()),
        );

        tx.send(Command::Prompt("read the note".to_string()))
            .expect("send");
        let events = drain_from(&mut rx).await;

        let verdict = verdicts(&events)[0];
        assert!(
            matches!(verdict.reason, StuckReason::Stall { .. }),
            "a tier with the tool result in front of it that stops talking has stalled: {:?}",
            verdict.reason
        );
        assert_eq!(
            verdict.counters.timing.worst_phase,
            crate::detect::Phase::Idle,
            "the wait after a tool is an idle wait, not a cold start"
        );
        assert_eq!(
            verdict.counters.timing.worst_allowance_ms, 50,
            "and it is the idle budget that measured it, not the first-token one"
        );
    }
}
