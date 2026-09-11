//! Application state and key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::agent::AgentEvent;
use crate::agent::approval::{ApprovalRequest, Decision};
use crate::agent::first_line;
use crate::agent::{Canceller, Command, Mode};
use crate::commands::{self, Input};
use crate::config::Config;

/// How much of the prompt a single paste may add, in characters.
///
/// Bracketed paste hands over whatever the clipboard held, and a stray
/// clipboard can hold a whole file. Without a limit that lands in the editor and
/// is then sent to a model; this is a guard against an accident, not a policy.
const MAX_PASTE: usize = 100_000;

/// How many lines PageUp and PageDown move the approval preview.
const APPROVAL_PAGE: i32 = 10;

/// How many turns of token history the session panel keeps for its sparkline.
const HISTORY: usize = 48;

/// How long the rail flashes a tier that was just spilled past, in redraws. At
/// the event loop's 90ms tick this is a little under a second: long enough to
/// catch the eye at the moment it matters, short enough not to become noise.
const FLASH_TICKS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
        }
    }
}

/// A tool waiting on the user's answer. The reply channel *is* the answer, so
/// dropping this without replying reads as a refusal at the other end.
pub struct PendingApproval {
    pub tool: String,
    pub preview: String,
    pub reply: oneshot::Sender<Decision>,
}

pub struct App {
    pub config: Config,
    pub messages: Vec<Message>,
    pub input: String,
    /// Lines scrolled back from the bottom of the transcript.
    pub scroll_back: u16,
    pub should_quit: bool,
    /// Where a prompt goes, once a tier is available to answer it.
    commands: Option<UnboundedSender<Command>>,
    /// Index of the assistant message currently being streamed into.
    streaming: Option<usize>,
    /// Index of the notice for a tool that is still running. The transcript puts
    /// a spinner here instead of the arrow it was written with.
    pub running: Option<usize>,
    pub approval: Option<PendingApproval>,
    pub busy: bool,

    /// Whether the fallback policy keeps the lower tier. Held here as well as in
    /// the chain because the session panel draws it.
    pub sticky: bool,
    /// What a turn is allowed to do. The agent enforces it; this copy is so the
    /// interface can say which mode is active.
    pub mode: Mode,
    /// How to stop a turn that is already running. `None` when no agent is
    /// attached, in which case there is nothing to cancel.
    cancel: Option<Canceller>,
    /// The last frame size, so key handling can work out how much of the
    /// approval modal is on screen without reaching into the renderer.
    pub viewport: Rect,
    /// How far the approval modal is scrolled down its preview.
    pub approval_scroll: u16,
    /// Per-tier totals, so `/cost` can attribute the session rather than only
    /// sum it. Keyed by the tier's label, in first-seen order.
    pub usage_by_tier: Vec<(String, crate::provider::Usage)>,
    /// Which command the menu has highlighted.
    pub menu_index: usize,
    /// Whether the help overlay is up.
    pub help: bool,

    /// Redraw counter, advanced by the event loop's timer. Drives the spinner
    /// and the streaming caret, the only two things here that move by
    /// themselves.
    pub tick: u64,
    /// The tick a tier was last spilled past on, so the rail can draw attention
    /// to the move for a moment rather than only recording it.
    pub escalated_at: Option<u64>,
    /// The configured tiers, under the label the agent reports them by, so an
    /// escalation can be matched back to a position in the rail. Empty until
    /// the agent is attached.
    pub tier_labels: Vec<String>,
    /// Which tier is answering, as an index into `tier_labels`.
    pub active_tier: usize,
    /// Which tiers were spilled past, aligned with `tier_labels`.
    pub tier_failed: Vec<bool>,
    /// Token totals for the session, summed across every tier and turn. This is
    /// what a fallback actually costs, which is otherwise invisible.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Input tokens for each completed turn, oldest first, capped. The session
    /// panel draws these as a sparkline, which is the only place the shape of a
    /// session's cost is visible rather than just its total.
    pub usage_history: Vec<u64>,
    /// User turns completed.
    pub turns: u32,
}

impl App {
    pub fn new(config: Config) -> Self {
        let messages = vec![Message::system(welcome(&config))];
        let sticky = config.general.sticky_fallback;
        Self {
            config,
            messages,
            input: String::new(),
            scroll_back: 0,
            should_quit: false,
            commands: None,
            streaming: None,
            running: None,
            approval: None,
            busy: false,
            sticky,
            mode: Mode::default(),
            cancel: None,
            viewport: Rect::new(0, 0, 80, 24),
            approval_scroll: 0,
            usage_by_tier: Vec::new(),
            menu_index: 0,
            help: false,
            tick: 0,
            escalated_at: None,
            tier_labels: Vec::new(),
            active_tier: 0,
            tier_failed: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cache_write: 0,
            usage_history: Vec::new(),
            turns: 0,
        }
    }

    /// Connect to a running agent, so prompts have somewhere to go.
    ///
    /// The tier labels are taken structurally rather than joined into a
    /// sentence, because the header rail needs to know which one is active and
    /// which have been spilled past.
    pub fn attach(
        &mut self,
        commands: UnboundedSender<Command>,
        cancel: Canceller,
        tier_labels: &[String],
        warning: Option<String>,
    ) {
        self.commands = Some(commands);
        self.cancel = Some(cancel);
        self.tier_labels = tier_labels.to_vec();
        self.tier_failed = vec![false; tier_labels.len()];
        self.active_tier = 0;
        if let Some(warning) = warning {
            self.messages.push(Message::system(warning));
        }
    }

    /// Where a tier sits in the configured chain.
    pub fn tier_index(&self, label: &str) -> Option<usize> {
        self.tier_labels.iter().position(|known| known == label)
    }

    /// Note that a tier was spilled past.
    pub fn fail_tier(&mut self, label: &str) {
        if let Some(index) = self.tier_index(label) {
            if let Some(failed) = self.tier_failed.get_mut(index) {
                *failed = true;
            }
        }
    }

    /// Follow the chain down to the tier now answering.
    pub fn activate_tier(&mut self, label: &str) {
        if let Some(index) = self.tier_index(label) {
            self.active_tier = index;
        }
    }

    /// Add one turn's tokens to the session totals, and to the tier that spent
    /// them.
    ///
    /// Cache counts are summed as reported and never subtracted from the input
    /// figure: whether they are included in it differs by provider.
    pub fn record_usage_on(&mut self, tier: Option<&str>, usage: &crate::provider::Usage) {
        self.tokens_in = self.tokens_in.saturating_add(usage.prompt_tokens);
        self.tokens_out = self.tokens_out.saturating_add(usage.completion_tokens);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read_tokens);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write_tokens);

        self.usage_history.push(usage.prompt_tokens);
        // Keep only what the sparkline can show, so a long session does not grow
        // this without bound.
        if self.usage_history.len() > HISTORY {
            self.usage_history.remove(0);
        }

        if let Some(tier) = tier {
            match self
                .usage_by_tier
                .iter_mut()
                .find(|(known, _)| known == tier)
            {
                Some((_, total)) => {
                    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
                    total.completion_tokens = total
                        .completion_tokens
                        .saturating_add(usage.completion_tokens);
                    total.cache_read_tokens = total
                        .cache_read_tokens
                        .saturating_add(usage.cache_read_tokens);
                    total.cache_write_tokens = total
                        .cache_write_tokens
                        .saturating_add(usage.cache_write_tokens);
                }
                None => self.usage_by_tier.push((tier.to_string(), *usage)),
            }
        }
    }

    /// Add one turn's tokens to the session totals, with no tier to attribute
    /// them to.
    #[cfg(test)]
    pub fn record_usage(&mut self, usage: &crate::provider::Usage) {
        self.record_usage_on(None, usage);
    }

    /// Whether a tier was spilled past a moment ago, so the rail can flash the
    /// move. Time here is the redraw counter, which only runs during a turn —
    /// exactly the window an escalation happens in.
    pub fn recently_escalated(&self) -> bool {
        matches!(self.escalated_at, Some(at) if self.tick.saturating_sub(at) < FLASH_TICKS)
    }

    /// The tier now answering, by name, if the agent is attached.
    pub fn active_tier_name(&self) -> Option<&str> {
        self.tier_labels.get(self.active_tier).map(String::as_str)
    }

    /// The message currently being streamed into, if any. The transcript puts
    /// its caret here.
    pub fn streaming_index(&self) -> Option<usize> {
        self.streaming
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Terminals differ on whether they report repeats and releases; acting on
        // anything but a press makes one keytap move several lines.
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Ctrl-C quits from anywhere, before anything else can claim it. The
        // approval modal and the help overlay both swallow every key they do not
        // use, which left the universal way out dead in exactly the two places
        // someone might want it: a modal asking about a change they do not
        // understand, and an overlay they opened by accident.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.should_quit = true;
            return;
        }

        // While the agent is asking, the keys belong to the modal: the answer
        // keys answer it, and the arrows read a preview that does not fit.
        if self.approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.answer_approval(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_approval(false);
                }
                KeyCode::Up => self.scroll_approval(-1),
                KeyCode::Down => self.scroll_approval(1),
                KeyCode::PageUp => self.scroll_approval(-APPROVAL_PAGE),
                KeyCode::PageDown => self.scroll_approval(APPROVAL_PAGE),
                _ => {}
            }
            return;
        }

        // The help overlay is dismissed by anything, so it can never trap
        // someone who opened it by accident.
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                    self.help = false;
                }
                _ => {}
            }
            return;
        }

        // The command menu owns the arrow keys and tab while it is open, so
        // those keys navigate it instead of scrolling the transcript.
        if self.menu_open() {
            match key.code {
                KeyCode::Up => {
                    self.menu_index = self.menu_index.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    let last = self.menu_matches().len().saturating_sub(1);
                    self.menu_index = (self.menu_index + 1).min(last);
                    return;
                }
                KeyCode::Tab => {
                    self.complete_command();
                    return;
                }
                _ => {}
            }
        }

        // Shift+Tab toggles the mode, which is where the hand goes for it after
        // any other agent. Bare Tab does the same whenever the menu is not using
        // it, since there is nothing to indent in a prompt.
        if matches!(key.code, KeyCode::BackTab | KeyCode::Tab) {
            self.toggle_mode();
            return;
        }

        match key.code {
            KeyCode::Esc => {
                // Esc means "stop what you are doing" before it means "leave",
                // so a turn in flight is what it stops. Quitting out from under
                // a running model would throw the conversation away.
                if self.busy {
                    self.cancel_turn();
                } else if self.input.starts_with('/') {
                    // A half-typed command is cancelled, which is what Esc means
                    // everywhere else.
                    self.input.clear();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.input.pop();
                self.menu_index = 0;
            }
            KeyCode::Char('?') if self.input.is_empty() => self.help = true,
            KeyCode::Char(ch) => {
                self.input.push(ch);
                // A new prefix means the old highlight is meaningless.
                self.menu_index = 0;
            }
            KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(1),
            KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            _ => {}
        }
    }

    /// Whether the slash menu should be on screen.
    pub fn menu_open(&self) -> bool {
        commands::completion_prefix(&self.input).is_some() && !self.menu_matches().is_empty()
    }

    /// Stop the turn that is running.
    ///
    /// The turn is stopped where it stands rather than retried on another tier,
    /// so the model is not changed out from under a decision the user just made
    /// about it.
    fn cancel_turn(&mut self) {
        match &self.cancel {
            Some(cancel) => cancel.cancel(),
            None => self
                .messages
                .push(Message::system("there is nothing to cancel")),
        }
    }

    /// Take a pasted block of text into the prompt.
    ///
    /// Newlines are kept: a pasted snippet is often several lines, and flattening
    /// it would silently change what the model is asked. The prompt box already
    /// wraps and scrolls, so a multi-line paste is exactly what it handles.
    pub fn paste(&mut self, text: &str) {
        if self.approval.is_some() || self.help {
            // The keys belong elsewhere; a paste must not land in a prompt that
            // is not taking input.
            return;
        }
        let room = MAX_PASTE.saturating_sub(self.input.chars().count());
        if room == 0 {
            return;
        }
        let mut taken: String = text.chars().take(room).collect();
        if taken.chars().count() < text.chars().count() {
            taken.push('…');
            self.messages.push(Message::system(format!(
                "that paste was longer than {MAX_PASTE} characters, so only the beginning was \
                 kept"
            )));
        }
        // Control characters would corrupt the rendered layout; the wrapper
        // strips them at render time, but they should not be stored at all.
        for ch in taken.chars().filter(|ch| !ch.is_control() || *ch == '\n') {
            self.input.push(ch);
        }
        self.menu_index = 0;
    }

    /// Move the approval preview by `delta` lines, staying within it.
    ///
    /// Clamped against the same geometry the renderer uses, so holding Down at
    /// the end does not leave the offset stranded past the content where an Up
    /// press would appear to do nothing.
    fn scroll_approval(&mut self, delta: i32) {
        let Some(pending) = &self.approval else {
            return;
        };
        let theme = crate::ui::theme::Theme::detect();
        let max = crate::ui::approval::max_scroll(pending, &theme, self.viewport);

        let next = if delta < 0 {
            self.approval_scroll
                .saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.approval_scroll.saturating_add(delta as u16)
        };
        self.approval_scroll = next.min(max);
    }

    /// Switch between building and planning.
    ///
    /// The interface does not change a mode the agent cannot be told about: with
    /// no agent there is nothing to enforce it, and showing a read-only badge
    /// over a mode that nothing is honouring would be a lie.
    fn toggle_mode(&mut self) {
        let next = self.mode.toggled();
        let sent = matches!(&self.commands, Some(commands) if commands.send(Command::SetMode(next)).is_ok());
        if sent {
            self.mode = next;
        } else {
            self.messages.push(Message::system(
                "no agent is running, so the mode was not changed",
            ));
        }
    }

    /// The commands matching what has been typed.
    pub fn menu_matches(&self) -> Vec<&'static commands::Spec> {
        match commands::completion_prefix(&self.input) {
            Some(prefix) => commands::matching(prefix),
            None => Vec::new(),
        }
    }

    /// Put the highlighted command into the prompt line.
    fn complete_command(&mut self) {
        let matches = self.menu_matches();
        let Some(spec) = matches.get(self.menu_index.min(matches.len().saturating_sub(1))) else {
            return;
        };
        // A trailing space is offered only where there is an argument to type,
        // so Enter on a no-argument command runs it straight away.
        self.input = match spec.arg {
            commands::Arg::None => format!("/{}", spec.name),
            _ => format!("/{} ", spec.name),
        };
        self.menu_index = 0;
    }

    fn answer_approval(&mut self, approve: bool) {
        if let Some(pending) = self.approval.take() {
            let decision = if approve {
                Decision::Approve
            } else {
                Decision::Deny
            };
            let _ = pending.reply.send(decision);
        }
    }

    /// A tool needs an answer before it may run.
    pub fn set_approval(&mut self, request: ApprovalRequest) {
        self.approval = Some(PendingApproval {
            tool: request.tool,
            preview: request.preview,
            reply: request.reply,
        });
        self.approval_scroll = 0;
        self.scroll_back = 0;
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(chunk) => {
                // Consecutive chunks belong to the same message; anything else
                // in between (a tool, a notice) starts a new one.
                self.running = None;
                let index = match self.streaming {
                    Some(index) => index,
                    None => {
                        self.messages.push(Message::assistant(""));
                        let index = self.messages.len() - 1;
                        self.streaming = Some(index);
                        index
                    }
                };
                if let Some(message) = self.messages.get_mut(index) {
                    message.text.push_str(&chunk);
                }
            }
            AgentEvent::ToolStarted { name, preview } => {
                self.streaming = None;
                self.messages.push(Message::system(format!(
                    "→ {name}  {}",
                    first_line(&preview)
                )));
                // The transcript spins this one until its result lands.
                self.running = Some(self.messages.len() - 1);
            }
            AgentEvent::ToolFinished { name, ok, summary } => {
                self.streaming = None;
                self.running = None;
                let mark = if ok { "✓" } else { "✗" };
                self.messages
                    .push(Message::system(format!("{mark} {name}  {summary}")));
            }
            AgentEvent::Denied { tool } => {
                self.streaming = None;
                self.running = None;
                self.messages
                    .push(Message::system(format!("✗ {tool} — you declined")));
            }
            AgentEvent::Notice(message) => {
                self.streaming = None;
                self.running = None;
                self.messages.push(Message::system(message));
            }
            AgentEvent::Finished { stop_reason, usage } => {
                self.streaming = None;
                self.running = None;
                self.busy = false;
                // The flash belongs to the turn that was running; once it is
                // over, the move is history rather than news.
                self.escalated_at = None;
                if stop_reason.as_deref() == Some("length") {
                    self.messages.push(Message::system(
                        "! the model hit its output limit, so this answer is incomplete",
                    ));
                }
                if let Some(usage) = usage {
                    // The rail already follows every escalation, so the tier
                    // answering is known here without the agent having to say.
                    let tier = self.active_tier_name().map(str::to_string);
                    self.record_usage_on(tier.as_deref(), &usage);
                    self.messages.push(Message::system(usage_line(&usage)));
                }
            }
            AgentEvent::Escalated { from, to, reason } => {
                // What the failing tier streamed is not what the next tier will
                // continue from, so it must not sit in the transcript as though
                // it were an answer.
                self.discard_streaming_message();
                self.running = None;
                self.fail_tier(&from);
                self.activate_tier(&to);
                self.escalated_at = Some(self.tick);
                self.messages.push(Message::system(format!(
                    "✗ {from} {reason} — spilling over to {to}"
                )));
            }
            AgentEvent::Cancelled { tier } => {
                // The half-streamed answer never reached the conversation, so
                // showing it would leave the transcript claiming something the
                // model was never told. Everything before it stands.
                self.discard_streaming_message();
                self.streaming = None;
                self.running = None;
                self.busy = false;
                self.escalated_at = None;
                // A modal can only be up while a turn runs, so stopping the turn
                // takes it down too. The reply channel is already gone.
                self.approval = None;
                self.messages
                    .push(Message::system(format!("✗ you stopped {tier}")));
            }
            AgentEvent::Consulted {
                driver,
                consultant,
                about,
                nth,
                of,
                usage,
            } => {
                // The failed attempt is being thrown away in the conversation, so
                // it has to leave the transcript too. Without this the looped
                // output that got the tier stuck stays on screen looking like
                // part of the answer, which is exactly what the rollback exists
                // to prevent.
                self.discard_streaming_message();
                self.streaming = None;
                self.running = None;
                // The consultant's tokens belong to the consultant. Charging
                // them to the active tier would make `/cost` wrong in exactly the
                // comparison consult exists to inform.
                if let Some(usage) = usage {
                    self.record_usage_on(Some(&consultant), &usage);
                }
                // Where the consult sits in its budget, so a turn that consulted
                // twice is not mistaken for one that consulted once.
                let position = if of > 1 {
                    format!(" ({nth}/{of})")
                } else {
                    String::new()
                };
                // Short names, like the rail: the addresses belong in the
                // session panel. And the reason is stated once — an earlier
                // version repeated both names and the question inside this one
                // line, which wrapped across three rows saying one thing.
                self.messages.push(Message::system(format!(
                    "! {} consulted {}{position} — {about}",
                    crate::ui::short_label(&driver),
                    crate::ui::short_label(&consultant),
                )));
            }
            AgentEvent::Exhausted { reason } => {
                self.streaming = None;
                self.running = None;
                self.busy = false;
                self.escalated_at = None;
                self.messages
                    .push(Message::system(format!("✗ no tier could answer: {reason}")));
            }
        }
        self.scroll_back = 0;
    }

    /// Drop the assistant message being streamed, if it is still the last thing
    /// in the transcript.
    ///
    /// Only that one is removed: anything after it came from a tool that really
    /// ran, and the rollback between tiers undoes the conversation, not the
    /// side effects.
    fn discard_streaming_message(&mut self) {
        if let Some(index) = self.streaming.take() {
            if index + 1 == self.messages.len() {
                self.messages.remove(index);
            }
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }

        // A command is not a turn: most of them work while the model is busy,
        // and the ones that cannot say so themselves.
        if let Input::Command { name, argument } = commands::parse(&text) {
            self.input.clear();
            self.menu_index = 0;
            self.run_command(&name, &argument);
            return;
        }

        if self.busy {
            self.messages.push(Message::system(
                "still working on the previous message; wait for it to finish",
            ));
            return;
        }

        let Some(commands) = &self.commands else {
            self.input.clear();
            self.messages.push(Message::user(text));
            self.messages.push(Message::system(
                "no tier is available, so that message was not sent anywhere.",
            ));
            self.scroll_back = 0;
            return;
        };

        if commands.send(Command::Prompt(text.clone())).is_err() {
            self.messages
                .push(Message::system("the agent is no longer running"));
            return;
        }

        self.input.clear();
        self.streaming = None;
        self.busy = true;
        self.turns = self.turns.saturating_add(1);
        self.messages.push(Message::user(text));
        self.scroll_back = 0;
    }

    /// Act on a slash command.
    ///
    /// The ones whose state lives in the render loop are answered here; the rest
    /// are handed to the agent, which owns the chain and the conversation.
    fn run_command(&mut self, name: &str, argument: &str) {
        // Anything with nowhere to go is reported rather than dropped.
        let send = |app: &mut App, command: Command| match &app.commands {
            Some(commands) if commands.send(command).is_ok() => true,
            _ => {
                app.messages.push(Message::system(
                    "no agent is running, so that command had nowhere to go",
                ));
                false
            }
        };

        match name {
            "help" => self.help = true,

            "quit" => self.should_quit = true,

            "tier" if argument.is_empty() => {
                // No argument means "show me", which the rail and panel already
                // do — so say what the numbering is and what can be typed.
                self.messages.push(Message::system(self.describe_tiers()));
            }
            "tier" => {
                let query = argument.trim();
                let command = if query == "auto" {
                    Command::SetTier(None)
                } else {
                    Command::SetTier(Some(query.to_string()))
                };
                send(self, command);
            }

            "sticky" => match argument.trim().to_lowercase().as_str() {
                "on" | "true" | "yes" => {
                    // Recorded only once the chain has actually been told, so the
                    // session panel cannot report a policy that is not in effect.
                    if send(self, Command::SetSticky(true)) {
                        self.sticky = true;
                    }
                }
                "off" | "false" | "no" => {
                    if send(self, Command::SetSticky(false)) {
                        self.sticky = false;
                    }
                }
                other => {
                    let complaint = if other.is_empty() {
                        "usage: /sticky <on|off>".to_string()
                    } else {
                        format!("/sticky takes on or off, not {other:?}")
                    };
                    self.messages.push(Message::system(complaint));
                }
            },

            "cost" => self.messages.push(Message::system(self.describe_cost())),

            // These need the chain or the conversation, which the agent owns.
            "escalate" => {
                send(self, Command::Escalate);
            }
            "retry" => {
                let tier = argument.trim();
                // Busy only if the retry actually went somewhere. Setting it
                // regardless left the app waiting on a turn that was never
                // started: every later prompt was refused with "still working",
                // and there was no turn left to finish or cancel, so it never
                // recovered.
                if send(
                    self,
                    Command::Retry {
                        tier: (!tier.is_empty()).then(|| tier.to_string()),
                    },
                ) {
                    self.busy = true;
                }
            }
            "drop" => {
                send(self, Command::Drop);
            }
            "compact" => {
                send(self, Command::Compact);
            }
            "clear" => {
                // The transcript is the interface's own copy, so it is cleared
                // here too — but only once the agent has taken the command.
                // Clearing regardless wiped the very message explaining that
                // nothing had been cleared, leaving a blank screen and no reason
                // for it.
                if send(self, Command::Clear) {
                    self.messages.clear();
                    self.running = None;
                    self.streaming = None;
                    self.usage_by_tier.clear();
                    self.turns = 0;
                    self.usage_history.clear();
                    self.tokens_in = 0;
                    self.tokens_out = 0;
                    self.cache_read = 0;
                    self.cache_write = 0;
                    self.scroll_back = 0;
                }
            }
            "context" => {
                send(self, Command::Context);
            }

            // `commands::parse` only produces names from the catalogue, so this
            // is unreachable; it is a message rather than a panic because a
            // future catalogue edit should not be able to crash the app.
            other => self.messages.push(Message::system(format!(
                "there is no command called {other:?} — try /help"
            ))),
        }
    }

    /// The chain as the user can address it.
    fn describe_tiers(&self) -> String {
        if self.tier_labels.is_empty() {
            return "no tiers are attached yet".to_string();
        }

        let mut out = String::from("tiers, in the order they are tried:\n");
        for (index, label) in self.tier_labels.iter().enumerate() {
            let mark = if index == self.active_tier {
                "←"
            } else {
                " "
            };
            let state = if self.tier_failed.get(index).copied().unwrap_or(false) {
                " (spilled past)"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {} {}{state} {mark}\n",
                index + 1,
                crate::fallback::tier_name(label)
            ));
        }
        out.push_str(
            "\n/tier <name|number> chooses one, /tier auto returns to the configured order.",
        );
        out
    }

    /// Where the session's tokens went, tier by tier.
    fn describe_cost(&self) -> String {
        if self.usage_by_tier.is_empty() {
            return "nothing has been spent yet".to_string();
        }

        let mut out = String::from("tokens by tier, this session:\n");
        for (label, usage) in &self.usage_by_tier {
            out.push_str(&format!(
                "  {}: {} in, {} out",
                crate::fallback::tier_name(label),
                crate::text::thousands(usage.prompt_tokens),
                crate::text::thousands(usage.completion_tokens)
            ));
            // Cache figures only when a provider reports them, so a tier with
            // no cache does not get a line of zeros to explain away.
            if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
                out.push_str(&format!(
                    " · {} cached, {} written",
                    crate::text::thousands(usage.cache_read_tokens),
                    crate::text::thousands(usage.cache_write_tokens)
                ));
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "\ntotal: {} in, {} out · {} cached",
            crate::text::thousands(self.tokens_in),
            crate::text::thousands(self.tokens_out),
            crate::text::thousands(self.cache_read)
        ));
        // The point of attributing cost to a tier is comparing them, so say so.
        if self.usage_by_tier.len() > 1 {
            out.push_str(
                "\na cross-tier fallback is a cache miss no design can avoid, which is why the \
                 same prompt costs more on the tier it spills to.",
            );
        }
        out
    }
}

/// One line of token accounting for the transcript.
///
/// Cache figures are shown only when a provider reports them, so a tier with no
/// cache reads the same as it always did. The numbers are grouped the same way
/// the session panel groups them, because both are on screen at once and a
/// figure that reads "15360" in one place and "15,360" in the other looks like
/// two different numbers.
fn usage_line(usage: &crate::provider::Usage) -> String {
    let mut line = format!(
        "tokens: {} in, {} out",
        crate::text::thousands(usage.prompt_tokens),
        crate::text::thousands(usage.completion_tokens)
    );
    if usage.cache_read_tokens > 0 {
        line.push_str(&format!(
            " · {} cached",
            crate::text::thousands(usage.cache_read_tokens)
        ));
    }
    if usage.cache_write_tokens > 0 {
        line.push_str(&format!(
            " · {} written to cache",
            crate::text::thousands(usage.cache_write_tokens)
        ));
    }
    line
}

fn welcome(config: &Config) -> String {
    let mut text = String::from(
        "spill runs one prompt at a time through an ordered list of model tiers, and moves \
         down the list when the active tier stalls or repeats itself.",
    );

    // Only what nothing else on screen says. The chain, the tier answering, the
    // workspace and the fallback policy are all in the header and the session
    // panel, so repeating them here would show the same two facts twice on the
    // first screen. What the panel cannot fit is the explanation of what the
    // policy *means*:
    text.push_str(if config.general.sticky_fallback {
        "\n\nOnce a tier fails, spill stays on the lower tier for the rest of the session."
    } else {
        "\n\nAfter a tier fails, spill tries the first tier again on your next message."
    });

    if config.tiers.is_empty() {
        text.push_str(
            "\n\nNo tiers are configured yet. Copy config.example.toml to \
             ~/.config/spill/config.toml and point the first tier at a local server.",
        );
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn new_app() -> App {
        App::new(Config::default())
    }

    #[test]
    fn typing_builds_the_prompt() {
        let mut app = new_app();
        for ch in "hello".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Char('a')));
        app.handle_key(press(KeyCode::Char('b')));
        app.handle_key(press(KeyCode::Backspace));
        assert_eq!(app.input, "a");
    }

    #[test]
    fn enter_sends_and_clears_the_prompt() {
        let mut app = new_app();
        for ch in "hi".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Enter));
        assert!(app.input.is_empty());
        assert_eq!(
            app.messages.last().map(|m| m.text.as_str()),
            Some("no tier is available, so that message was not sent anywhere.")
        );
        assert_eq!(
            app.messages.get(app.messages.len() - 2).map(|m| m.role),
            Some(Role::User)
        );
    }

    #[test]
    fn enter_on_empty_input_does_nothing() {
        let mut app = new_app();
        let before = app.messages.len();
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.messages.len(), before);
    }

    #[test]
    fn whitespace_only_input_is_not_sent() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Char(' ')));
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.messages.len(), 1, "welcome message only");
    }

    #[test]
    fn escape_and_ctrl_c_both_quit() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Esc));
        assert!(app.should_quit);

        let mut app = new_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn scrolling_saturates_rather_than_underflowing() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.scroll_back, 0);
        app.handle_key(press(KeyCode::PageUp));
        assert_eq!(app.scroll_back, 10);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut app = new_app();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        app.handle_key(release);
        assert!(app.input.is_empty());
    }

    #[test]
    fn welcome_states_what_the_header_cannot_show() {
        let config = Config::parse(
            std::path::Path::new("test.toml"),
            r#"
            [general]
            workspace = "/work"
            sticky_fallback = false

            [[tier]]
            id = "local"
            name = "Local model"
            kind = "openai"
            base_url = "http://localhost:1234/v1"
            model = "qwen3-coder"

            [[tier]]
            id = "grok"
            kind = "cli"
            preset = "grok"
            approve_all = true
            "#,
        )
        .expect("config should parse");

        let text = welcome(&config);
        assert!(
            text.contains("first tier again"),
            "it explains what a per-turn policy means: {text}"
        );
        // The chain, the workspace and the policy's own value are all drawn in
        // the header and the session panel, so repeating them here would put the
        // same facts on screen twice.
        assert!(
            !text.contains("Local model"),
            "the header carries the chain now: {text}"
        );
        assert!(
            !text.contains("/work"),
            "the session panel carries the workspace: {text}"
        );
    }

    #[test]
    fn welcome_says_so_when_nothing_is_configured() {
        let text = welcome(&Config::default());
        assert!(text.contains("No tiers are configured yet"), "got: {text}");
    }

    #[test]
    fn welcome_explains_whichever_fallback_policy_is_set() {
        let mut sticky = Config::default();
        sticky.general.sticky_fallback = true;
        assert!(
            welcome(&sticky).contains("stays on the lower tier"),
            "{}",
            welcome(&sticky)
        );

        let mut per_turn = Config::default();
        per_turn.general.sticky_fallback = false;
        assert!(
            welcome(&per_turn).contains("first tier again"),
            "{}",
            welcome(&per_turn)
        );
    }

    #[test]
    fn welcome_leaves_the_workspace_to_the_session_panel() {
        let mut config = Config::default();
        config.general.workspace = "/somewhere".to_string();
        // The panel shows it whenever the panel exists, and on a terminal too
        // narrow for one the user has just configured this path themselves.
        assert!(!welcome(&config).contains("/somewhere"));
    }

    fn attached_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(tx, Canceller::default(), &["Local model".to_string()], None);
        (app, rx)
    }

    fn type_and_send(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Enter));
    }

    #[test]
    fn a_prompt_reaches_the_agent_when_attached() {
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "hello there");

        assert_eq!(
            commands.try_recv().expect("the prompt should be sent"),
            Command::Prompt("hello there".to_string())
        );
        assert!(app.input.is_empty());
        assert!(app.busy);
    }

    #[test]
    fn a_second_prompt_is_refused_while_a_turn_is_running() {
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "first");
        let _ = commands.try_recv();

        type_and_send(&mut app, "second");

        assert!(
            commands.try_recv().is_err(),
            "the second prompt must not be sent"
        );
        assert_eq!(app.input, "second", "the prompt should be kept for later");
        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("still working"))
        );
    }

    #[test]
    fn streamed_text_accumulates_into_a_single_message() {
        let mut app = new_app();
        let before = app.messages.len();

        for chunk in ["Hel", "lo", " there"] {
            app.handle_agent_event(AgentEvent::Text(chunk.to_string()));
        }

        assert_eq!(app.messages.len(), before + 1, "chunks share one message");
        let message = app.messages.last().expect("a message");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.text, "Hello there");
    }

    #[test]
    fn a_tool_call_starts_a_new_message_after_streamed_text() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Text("thinking".to_string()));
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            ok: true,
            summary: "notes.txt (3 lines)".to_string(),
        });
        app.handle_agent_event(AgentEvent::Text("done".to_string()));

        let texts: Vec<&str> = app
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert!(texts.iter().any(|t| t.contains("→ read_file")));
        assert!(texts.iter().any(|t| t.contains("✓ read_file")));
        // The trailing text must not be appended to the pre-tool message.
        assert_eq!(texts.last(), Some(&"done"));
    }

    #[test]
    fn an_exhausted_turn_clears_the_working_state() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        assert!(app.busy);

        app.handle_agent_event(AgentEvent::Exhausted {
            reason: "connection reset".to_string(),
        });

        assert!(!app.busy, "exhaustion must not leave the app stuck as busy");
        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("connection reset"))
        );
    }

    #[test]
    fn escalating_keeps_the_turn_going_and_shows_the_move() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        app.handle_agent_event(AgentEvent::Text("half an answer".to_string()));

        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated the same output 4 times".to_string(),
        });

        assert!(app.busy, "the turn is still running on the next tier");
        let texts: Vec<&str> = app.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.contains("half an answer")),
            "the abandoned tier's partial answer should be gone: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("spilling over to DeepSeek")),
            "the move should be visible: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("repeated the same output 4 times")),
            "the reason should be visible: {texts:?}"
        );
    }

    #[test]
    fn a_tool_that_already_ran_survives_an_escalation() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        app.handle_agent_event(AgentEvent::Text("thinking".to_string()));
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });

        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "went quiet for 60s".to_string(),
        });

        assert!(
            app.messages.iter().any(|m| m.text.contains("→ read_file")),
            "a tool that really ran should still be reported"
        );
    }

    #[test]
    fn finishing_clears_the_working_state() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");

        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: None,
        });

        assert!(!app.busy);
    }

    #[test]
    fn token_usage_is_reported_when_the_server_gives_it() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(crate::provider::Usage {
                prompt_tokens: 120,
                completion_tokens: 45,
                ..Default::default()
            }),
        });

        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("120 in, 45 out"))
        );
    }

    #[test]
    fn cache_hits_are_shown_alongside_the_token_counts() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(crate::provider::Usage {
                prompt_tokens: 15360,
                completion_tokens: 2,
                cache_read_tokens: 7424,
                cache_write_tokens: 0,
            }),
        });

        let line = &app.messages.last().expect("a message").text;
        assert!(line.contains("15,360 in, 2 out"), "{line}");
        assert!(line.contains("7,424 cached"), "{line}");
        assert!(
            !line.contains("written"),
            "a zero cache write should not be mentioned: {line}"
        );
    }

    #[test]
    fn a_tier_with_no_cache_reads_exactly_as_before() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(crate::provider::Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                ..Default::default()
            }),
        });

        assert_eq!(
            app.messages.last().expect("a message").text,
            "tokens: 10 in, 2 out"
        );
    }

    #[test]
    fn a_truncated_answer_is_called_out() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("length".to_string()),
            usage: None,
        });

        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("incomplete"))
        );
    }

    fn pending_write() -> (App, oneshot::Receiver<Decision>) {
        let mut app = new_app();
        let (reply, answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });
        (app, answer)
    }

    #[test]
    fn y_approves_a_pending_tool() {
        let (mut app, mut answer) = pending_write();
        app.handle_key(press(KeyCode::Char('y')));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Approve);
        assert!(app.approval.is_none(), "the modal should close");
    }

    #[test]
    fn enter_also_approves() {
        let (mut app, mut answer) = pending_write();
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(answer.try_recv().expect("an answer"), Decision::Approve);
    }

    #[test]
    fn n_denies_a_pending_tool() {
        let (mut app, mut answer) = pending_write();
        app.handle_key(press(KeyCode::Char('n')));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Deny);
        assert!(app.approval.is_none());
    }

    #[test]
    fn escape_denies_rather_than_quitting_while_a_tool_waits() {
        let (mut app, mut answer) = pending_write();
        app.handle_key(press(KeyCode::Esc));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Deny);
        assert!(!app.should_quit, "Esc must not quit while answering a tool");
    }

    #[test]
    fn typing_is_ignored_while_a_tool_waits() {
        let (mut app, answer) = pending_write();
        app.handle_key(press(KeyCode::Char('h')));
        app.handle_key(press(KeyCode::Char('i')));

        assert!(app.input.is_empty(), "the prompt must not take text");
        assert!(app.approval.is_some(), "the modal stays open");
        drop(answer);
    }

    #[test]
    fn a_dropped_answer_channel_does_not_panic() {
        let mut app = new_app();
        let (reply, answer) = oneshot::channel();
        drop(answer);
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });

        // The agent is gone; the send fails and that is not fatal.
        app.handle_key(press(KeyCode::Char('y')));
        assert!(app.approval.is_none());
    }

    #[test]
    fn attaching_takes_the_chain_for_the_rail() {
        let (app, _commands) = attached_app();

        assert_eq!(app.tier_labels, vec!["Local model".to_string()]);
        assert_eq!(app.active_tier, 0, "the top tier answers first");
        assert_eq!(app.tier_failed, vec![false]);
        assert_eq!(app.active_tier_name(), Some("Local model"));
        // The chain is drawn in the header, so it is not also said in prose.
        assert!(
            !app.messages
                .iter()
                .any(|m| m.text.contains("tiers, in order")),
            "the rail carries this now"
        );
    }

    #[test]
    fn attaching_reports_tiers_that_were_left_out() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(
            tx,
            Canceller::default(),
            &["Local model".to_string()],
            Some("not in the chain yet: grok (cli)".to_string()),
        );

        assert!(
            app.messages
                .iter()
                .any(|m| m.text.contains("not in the chain yet: grok (cli)"))
        );
    }

    #[test]
    fn escalating_moves_the_rail_down_the_chain() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(
            tx,
            Canceller::default(),
            &[
                "Local".to_string(),
                "DeepSeek".to_string(),
                "Grok".to_string(),
            ],
            None,
        );

        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated itself 4 times".to_string(),
        });

        assert_eq!(app.active_tier, 1);
        assert_eq!(app.tier_failed, vec![true, false, false]);
        assert_eq!(app.active_tier_name(), Some("DeepSeek"));

        // A second spill continues down rather than jumping back up.
        app.handle_agent_event(AgentEvent::Escalated {
            from: "DeepSeek".to_string(),
            to: "Grok".to_string(),
            reason: "went quiet".to_string(),
        });
        assert_eq!(app.active_tier, 2);
        assert_eq!(app.tier_failed, vec![true, true, false]);
    }

    #[test]
    fn a_label_the_chain_does_not_know_does_not_move_the_rail() {
        // Labels come from the same source as the chain, so this should not
        // happen; if it ever does, the rail holds rather than pointing at the
        // wrong tier.
        let (mut app, _commands) = attached_app();
        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local model".to_string(),
            to: "Somewhere else".to_string(),
            reason: "unknown".to_string(),
        });

        assert_eq!(app.active_tier, 0);
        assert_eq!(app.active_tier_name(), Some("Local model"));
    }

    #[test]
    fn an_escalation_flashes_briefly_and_then_settles() {
        let (mut app, _commands) = attached_app();
        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local model".to_string(),
            to: "Local model".to_string(),
            reason: "repeated itself".to_string(),
        });
        assert!(
            app.recently_escalated(),
            "the move should be marked at once"
        );

        app.tick += FLASH_TICKS;
        assert!(
            !app.recently_escalated(),
            "the flash must not outstay its welcome"
        );
    }

    #[test]
    fn finishing_a_turn_ends_the_flash() {
        let (mut app, _commands) = attached_app();
        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local model".to_string(),
            to: "Local model".to_string(),
            reason: "repeated itself".to_string(),
        });
        assert!(app.recently_escalated());

        // The flash belongs to the turn; once it is over, the move is history.
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: None,
        });
        assert!(!app.recently_escalated());
    }

    #[test]
    fn the_usage_history_keeps_only_the_recent_turns() {
        let mut app = new_app();
        for turn in 0..(HISTORY as u64 + 30) {
            app.record_usage(&crate::provider::Usage {
                prompt_tokens: turn,
                ..Default::default()
            });
        }

        assert_eq!(app.usage_history.len(), HISTORY);
        assert_eq!(
            *app.usage_history.last().expect("the newest turn"),
            HISTORY as u64 + 29,
            "the oldest are dropped, not the newest"
        );
    }

    #[test]
    fn a_turn_is_counted_when_it_is_sent() {
        let (mut app, _commands) = attached_app();
        assert_eq!(app.turns, 0);

        type_and_send(&mut app, "hello");
        assert_eq!(app.turns, 1);

        type_and_send(&mut app, "again");
        assert_eq!(app.turns, 1, "a refused prompt is not a turn");
    }

    #[test]
    fn token_totals_accumulate_across_turns() {
        let (mut app, _commands) = attached_app();
        let turn = crate::provider::Usage {
            prompt_tokens: 15_360,
            completion_tokens: 2,
            cache_read_tokens: 7_424,
            cache_write_tokens: 0,
        };

        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(turn),
        });
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(turn),
        });

        assert_eq!(app.tokens_in, 30_720);
        assert_eq!(app.tokens_out, 4);
        assert_eq!(app.cache_read, 14_848);
        assert_eq!(app.cache_write, 0);
    }

    // ---- slash commands ---------------------------------------------------

    fn last_message(app: &App) -> &str {
        app.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[test]
    fn a_command_is_never_sent_to_the_model_as_a_prompt() {
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "/cost");

        assert!(
            commands.try_recv().is_err(),
            "a command is not a turn and must not reach the agent"
        );
        assert!(!app.busy, "it does not start a turn either");
        assert!(
            app.messages
                .iter()
                .any(|m| m.text.contains("nothing has been spent")),
            "{:?}",
            app.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_slash_that_is_not_a_command_still_reaches_the_model() {
        // A question about a path is a question, not an instruction.
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "/usr/local/bin is on my path");

        assert_eq!(
            commands.try_recv().expect("a prompt"),
            Command::Prompt("/usr/local/bin is on my path".to_string())
        );
        assert!(app.busy);
    }

    #[test]
    fn the_commands_with_nowhere_else_to_live_are_forwarded() {
        let cases: &[(&str, Command)] = &[
            ("/escalate", Command::Escalate),
            ("/drop", Command::Drop),
            ("/compact", Command::Compact),
            ("/context", Command::Context),
            ("/clear", Command::Clear),
            ("/retry", Command::Retry { tier: None }),
            (
                "/retry 2",
                Command::Retry {
                    tier: Some("2".to_string()),
                },
            ),
            ("/tier Grok", Command::SetTier(Some("Grok".to_string()))),
            ("/tier auto", Command::SetTier(None)),
            ("/sticky off", Command::SetSticky(false)),
        ];

        for (typed, expected) in cases {
            let (mut app, mut commands) = attached_app();
            type_and_send(&mut app, typed);

            assert_eq!(
                commands.try_recv().ok(),
                Some(expected.clone()),
                "{typed} forwarded the wrong command"
            );
        }
    }

    #[test]
    fn sticky_records_its_own_state_and_refuses_nonsense() {
        let (mut app, _commands) = attached_app();
        assert!(app.sticky, "sticky is the default");

        type_and_send(&mut app, "/sticky off");
        assert!(!app.sticky);

        type_and_send(&mut app, "/sticky on");
        assert!(app.sticky);

        type_and_send(&mut app, "/sticky maybe");
        assert!(app.sticky, "a bad argument must not change anything");
        assert!(
            last_message(&app).contains("on or off"),
            "{}",
            last_message(&app)
        );

        // The bare form explains itself rather than guessing.
        type_and_send(&mut app, "/sticky");
        assert!(
            last_message(&app).contains("usage"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn tier_with_no_argument_lists_the_chain() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "/tier");

        let said = last_message(&app);
        assert!(
            said.contains("1 Local model"),
            "the number and name: {said}"
        );
        assert!(said.contains("/tier auto"), "and how to undo it: {said}");
    }

    #[test]
    fn help_opens_with_the_question_mark_and_closes_with_escape() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Char('?')));
        assert!(app.help);

        app.handle_key(press(KeyCode::Esc));
        assert!(!app.help);
        assert!(!app.should_quit, "closing help must not quit");
    }

    #[test]
    fn help_also_opens_from_the_command() {
        let mut app = new_app();
        type_and_send(&mut app, "/help");
        assert!(app.help);
    }

    #[test]
    fn escape_cancels_a_half_typed_command_rather_than_quitting() {
        let mut app = new_app();
        for ch in "/tie".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Esc));

        assert!(app.input.is_empty(), "the command should be abandoned");
        assert!(!app.should_quit, "but the app should stay");
    }

    #[test]
    fn the_menu_opens_on_a_slash_and_tab_completes_the_highlighted_command() {
        let mut app = new_app();
        app.handle_key(press(KeyCode::Char('/')));
        assert!(app.menu_open(), "a lone slash opens the menu");

        // Narrow to tier, then take it. It takes an argument, so a space is
        // offered and the menu closes behind it.
        for ch in "tie".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.input, "/tier ", "an argument is awaited");
        assert!(!app.menu_open(), "the menu closes on a space");

        // A command with no argument is completed without the trailing space,
        // so enter runs it rather than waiting for input.
        app.input = "/cos".to_string();
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.input, "/cost");
    }

    #[test]
    fn an_unknown_prefix_shows_no_menu() {
        let mut app = new_app();
        for ch in "/zzz".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        assert!(!app.menu_open());
    }

    #[test]
    fn clear_empties_the_transcript_and_the_session_totals() {
        let (mut app, _commands) = attached_app();
        app.messages.push(Message::assistant("an answer"));
        app.turns = 4;
        app.record_usage(&crate::provider::Usage {
            prompt_tokens: 100,
            ..Default::default()
        });

        type_and_send(&mut app, "/clear");

        // The transcript is the interface's own copy, so it goes at once. The
        // agent's confirmation arrives as a notice in a real run, which is the
        // next thing to land.
        assert!(app.messages.is_empty(), "{:?}", app.messages.len());
        assert_eq!(app.turns, 0);
        assert_eq!(app.tokens_in, 0, "the cost of the old session is gone");
        assert!(app.usage_by_tier.is_empty());

        app.handle_agent_event(AgentEvent::Notice("conversation cleared".to_string()));
        assert_eq!(app.messages.len(), 1);
        assert!(last_message(&app).contains("cleared"));
    }

    #[test]
    fn cost_attributes_the_session_to_the_tiers_that_spent_it() {
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek V4 Flash".to_string()];
        app.attach(
            tokio::sync::mpsc::unbounded_channel().0,
            Canceller::default(),
            &app.tier_labels.clone(),
            None,
        );

        app.record_usage_on(
            Some("Local"),
            &crate::provider::Usage {
                prompt_tokens: 1_000,
                completion_tokens: 10,
                ..Default::default()
            },
        );
        app.record_usage_on(
            Some("DeepSeek V4 Flash"),
            &crate::provider::Usage {
                prompt_tokens: 15_360,
                completion_tokens: 2,
                cache_read_tokens: 7_424,
                ..Default::default()
            },
        );

        type_and_send(&mut app, "/cost");
        let said = last_message(&app);

        assert!(said.contains("Local: 1,000 in, 10 out"), "{said}");
        assert!(
            said.contains("DeepSeek V4 Flash: 15,360 in, 2 out"),
            "{said}"
        );
        assert!(said.contains("7,424 cached"), "{said}");
        assert!(said.contains("total: 16,360 in"), "{said}");
        // A tier with no cache must not get a line of zeros.
        let local_line = said
            .lines()
            .find(|line| line.contains("Local:"))
            .expect("the local line");
        assert!(!local_line.contains("cached"), "{local_line}");
    }

    #[test]
    fn a_command_that_needs_an_agent_is_refused_when_there_is_none() {
        // No tiers attached, so there is nothing to forward to.
        let mut app = new_app();
        type_and_send(&mut app, "/compact");

        assert!(
            last_message(&app).contains("nowhere to go"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn retry_with_no_agent_does_not_wedge_the_app() {
        // `busy` is what gates the next prompt, so setting it with nothing
        // running left the app refusing every later message with "still
        // working" — and there was no turn left to finish or cancel, so it
        // never recovered.
        let mut app = new_app();
        type_and_send(&mut app, "/retry");

        assert!(!app.busy, "nothing was started, so nothing is running");
        assert!(
            last_message(&app).contains("nowhere to go"),
            "{}",
            last_message(&app)
        );

        // The proof that it is not wedged: a prompt still gets through to the
        // point of being sent, rather than being refused as busy.
        type_and_send(&mut app, "hello");
        assert!(
            last_message(&app).contains("no tier is available"),
            "the app is still refusing prompts: {}",
            last_message(&app)
        );
        assert!(!app.busy);
    }

    #[test]
    fn sticky_with_no_agent_reports_the_policy_that_is_actually_in_effect() {
        let mut app = new_app();
        assert!(app.sticky, "sticky is the default");

        type_and_send(&mut app, "/sticky off");

        assert!(
            app.sticky,
            "the chain was never told, so the panel must not claim otherwise"
        );
        assert!(
            last_message(&app).contains("nowhere to go"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn clear_with_no_agent_leaves_the_transcript_alone() {
        // Clearing regardless wiped the very message explaining that nothing had
        // been cleared, leaving a blank screen and no reason for it.
        let mut app = new_app();
        let before = app.messages.len();

        type_and_send(&mut app, "/clear");

        assert_eq!(app.messages.len(), before + 1, "only the explanation");
        assert!(
            last_message(&app).contains("nowhere to go"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn ctrl_c_quits_even_while_a_tool_waits() {
        // The modal swallows every key it does not use, so the universal way out
        // was dead in the one place someone might most want it: a change they do
        // not understand.
        let mut app = new_app();
        let (reply, _answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.should_quit, "the way out must not be swallowed");
    }

    #[test]
    fn ctrl_c_quits_even_with_the_help_overlay_open() {
        let mut app = new_app();
        app.help = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.should_quit);
    }

    #[test]
    fn escape_still_denies_a_tool_rather_than_quitting() {
        // Ctrl-C leaving the modal must not have cost Esc its meaning there.
        let mut app = new_app();
        let (reply, mut answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });

        app.handle_key(press(KeyCode::Esc));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Deny);
        assert!(
            !app.should_quit,
            "Esc in the modal answers it, it does not quit"
        );
    }

    // ---- modes ------------------------------------------------------------

    fn shift_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    #[test]
    fn shift_tab_toggles_the_mode_and_tells_the_agent() {
        let (mut app, mut commands) = attached_app();
        assert_eq!(app.mode, Mode::Build, "build is where it starts");

        app.handle_key(shift_tab());

        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(
            commands.try_recv().expect("the mode should be sent"),
            Command::SetMode(Mode::Plan)
        );

        app.handle_key(shift_tab());
        assert_eq!(app.mode, Mode::Build);
        assert_eq!(
            commands.try_recv().expect("the mode should be sent back"),
            Command::SetMode(Mode::Build)
        );
    }

    #[test]
    fn bare_tab_toggles_the_mode_when_no_command_is_being_typed() {
        // Tab is what the user asked for; Shift+Tab is what the hand does. Both
        // work, and neither is swallowed by the other.
        let (mut app, mut commands) = attached_app();
        app.handle_key(press(KeyCode::Tab));

        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(
            commands.try_recv().expect("the mode should be sent"),
            Command::SetMode(Mode::Plan)
        );
    }

    #[test]
    fn tab_still_completes_a_command_rather_than_toggling_the_mode() {
        // The menu owns Tab while it is open, so completion is not lost.
        let (mut app, mut commands) = attached_app();
        for ch in "/cos".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Tab));

        assert_eq!(app.input, "/cost", "it should have completed");
        assert_eq!(app.mode, Mode::Build, "and not changed the mode");
        assert!(commands.try_recv().is_err(), "nothing was sent");
    }

    #[test]
    fn a_mode_is_not_changed_when_no_agent_can_be_told_about_it() {
        // Showing a read-only badge over a mode nothing is enforcing would be a
        // lie, so with no agent the toggle does nothing but say so.
        let mut app = new_app();
        app.handle_key(shift_tab());

        assert_eq!(app.mode, Mode::Build);
        assert!(
            last_message(&app).contains("no agent is running"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn the_mode_does_not_change_when_the_agent_channel_is_gone() {
        let (mut app, commands) = attached_app();
        drop(commands);

        app.handle_key(shift_tab());

        assert_eq!(app.mode, Mode::Build, "a failed send must not change it");
        assert!(
            last_message(&app).contains("no agent is running"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn plan_mode_is_reported_by_the_agent_as_a_notice() {
        let (mut app, _commands) = attached_app();
        app.handle_agent_event(AgentEvent::Notice("plan mode: it can read".to_string()));

        assert!(
            last_message(&app).contains("plan mode"),
            "{}",
            last_message(&app)
        );
    }

    // ---- cancelling a turn ------------------------------------------------

    /// An attached app whose canceller the test also holds.
    fn attached_with_cancel() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
        Canceller,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Canceller::default();
        let mut app = new_app();
        app.attach(tx, cancel.clone(), &["Local model".to_string()], None);
        (app, rx, cancel)
    }

    #[test]
    fn escape_stops_a_running_turn_instead_of_quitting() {
        let (mut app, _commands, cancel) = attached_with_cancel();
        type_and_send(&mut app, "do something");
        assert!(app.busy);

        app.handle_key(press(KeyCode::Esc));

        assert!(cancel.is_cancelled(), "the turn should have been stopped");
        assert!(
            !app.should_quit,
            "quitting would throw away the conversation the user is in the middle of"
        );
    }

    #[test]
    fn escape_still_quits_when_nothing_is_running() {
        let (mut app, _commands, cancel) = attached_with_cancel();
        assert!(!app.busy);

        app.handle_key(press(KeyCode::Esc));

        assert!(app.should_quit);
        assert!(!cancel.is_cancelled(), "nothing to cancel");
    }

    #[test]
    fn escape_cancels_a_half_typed_command_before_it_quits() {
        // The menu's own meaning wins while it is open, running turn or not.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        for ch in "/tie".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }

        app.handle_key(press(KeyCode::Esc));

        assert!(app.input.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn a_cancelled_event_clears_the_working_state_and_says_so() {
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "do something");
        app.handle_agent_event(AgentEvent::Text("half an answer".to_string()));

        app.handle_agent_event(AgentEvent::Cancelled {
            tier: "Local model".to_string(),
        });

        assert!(!app.busy, "the app must not stay stuck as working");
        assert!(
            last_message(&app).contains("you stopped"),
            "{}",
            last_message(&app)
        );
        // The half-streamed answer was never in the conversation, so it must not
        // be left in the transcript as though it were.
        assert!(
            !app.messages
                .iter()
                .any(|m| m.text.contains("half an answer")),
            "{:?}",
            app.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_cancelled_event_takes_down_a_pending_approval() {
        let (mut app, _commands, _cancel) = attached_with_cancel();
        let (reply, _answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });

        app.handle_agent_event(AgentEvent::Cancelled {
            tier: "Local model".to_string(),
        });

        assert!(
            app.approval.is_none(),
            "the modal would otherwise sit there with no turn behind it"
        );
    }

    #[test]
    fn cancelling_with_no_agent_says_so_rather_than_pretending() {
        let mut app = new_app();
        app.busy = true;
        app.handle_key(press(KeyCode::Esc));

        assert!(
            last_message(&app).contains("nothing to cancel"),
            "{}",
            last_message(&app)
        );
    }

    // ---- pasting ----------------------------------------------------------

    #[test]
    fn a_paste_lands_in_the_prompt_as_one_block() {
        let mut app = new_app();
        app.paste("first line\nsecond line");

        assert_eq!(app.input, "first line\nsecond line");
    }

    #[test]
    fn a_paste_keeps_its_line_breaks() {
        // A pasted snippet is often several lines, and flattening it would
        // silently change what the model is asked.
        let mut app = new_app();
        app.paste("fn main() {\n    todo!()\n}\n");

        assert!(app.input.contains('\n'), "{:?}", app.input);
        assert_eq!(app.input.lines().count(), 3);
    }

    #[test]
    fn a_paste_is_ignored_while_the_keys_belong_elsewhere() {
        let mut app = new_app();
        app.help = true;
        app.paste("should not land");
        assert!(app.input.is_empty(), "the help overlay owns the keys");

        let mut app = new_app();
        let (reply, _answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create a.txt".to_string(),
            reply,
        });
        app.paste("should not land either");
        assert!(app.input.is_empty(), "the modal owns the keys");
    }

    #[test]
    fn an_enormous_paste_is_cut_short_and_says_so() {
        // A stray clipboard can hold a whole file, and it would go straight to a
        // model.
        let mut app = new_app();
        let huge = "x".repeat(MAX_PASTE + 500);
        app.paste(&huge);

        assert!(app.input.chars().count() <= MAX_PASTE + 1);
        assert!(
            last_message(&app).contains("longer than"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn control_characters_are_stripped_from_a_paste_but_newlines_are_not() {
        let mut app = new_app();
        app.paste("a\u{7}b\u{1b}[31mc\nd");
        assert_eq!(app.input, "ab[31mc\nd");
    }

    // ---- scrolling an approval -------------------------------------------

    /// A preview long enough to need scrolling in the default viewport.
    fn long_preview_approval() -> (App, oneshot::Receiver<Decision>) {
        let mut app = new_app();
        // The default test viewport is 80x24, whose capacity is well under this.
        let preview = (0..80)
            .map(|n| format!("+ line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (reply, answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "edit_file".to_string(),
            preview,
            reply,
        });
        (app, answer)
    }

    #[test]
    fn the_approval_preview_scrolls_and_stops_at_both_ends() {
        let (mut app, _answer) = long_preview_approval();
        assert_eq!(app.approval_scroll, 0);

        // Up at the top does nothing rather than wrapping or panicking.
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.approval_scroll, 0);

        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.approval_scroll, 2);

        // Down at the bottom stops at the content, so an Up afterwards responds
        // immediately instead of unwinding a stranded offset.
        for _ in 0..500 {
            app.handle_key(press(KeyCode::Down));
        }
        let bottom = app.approval_scroll;
        assert!(bottom > 0, "it should have scrolled");
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.approval_scroll, bottom - 1);
    }

    #[test]
    fn page_keys_move_further_than_the_arrows() {
        let (mut app, _answer) = long_preview_approval();
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.approval_scroll, APPROVAL_PAGE as u16);

        app.handle_key(press(KeyCode::PageUp));
        assert_eq!(app.approval_scroll, 0);
    }

    // ---- consulting -------------------------------------------------------

    fn consulted_event(usage: Option<crate::provider::Usage>) -> AgentEvent {
        AgentEvent::Consulted {
            driver: "Local".to_string(),
            consultant: "DeepSeek".to_string(),
            about: "repeated the same output 4 times".to_string(),
            nth: 1,
            of: 2,
            usage,
        }
    }

    #[test]
    fn a_consult_discards_the_attempt_that_got_the_tier_stuck() {
        // The rollback throws the failed attempt out of the conversation, so it
        // has to leave the transcript too. Otherwise the looped output that
        // caused the stall stays on screen looking like part of the answer.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "make the tests pass");
        app.handle_agent_event(AgentEvent::Text("the same line\n".to_string()));
        app.handle_agent_event(AgentEvent::Text("the same line\n".to_string()));
        assert!(
            app.messages
                .iter()
                .any(|m| m.text.contains("the same line")),
            "the looped output should be streaming before the consult"
        );

        app.handle_agent_event(consulted_event(None));

        assert!(
            !app.messages
                .iter()
                .any(|m| m.text.contains("the same line")),
            "the abandoned attempt must not survive as an answer: {:?}",
            app.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(
            last_message(&app).contains("consulted DeepSeek"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn the_consult_line_says_each_thing_once_and_uses_short_names() {
        // An earlier version repeated both names and the question inside a single
        // line, with full addresses, so it wrapped across three rows saying one
        // thing. The addresses belong in the session panel.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        app.tier_labels = vec![
            "Looping Local (http://127.0.0.1:8735/v1)".to_string(),
            "Frontier Helper (http://127.0.0.1:8736/v1)".to_string(),
        ];
        type_and_send(&mut app, "go");

        app.handle_agent_event(AgentEvent::Consulted {
            driver: "Looping Local (http://127.0.0.1:8735/v1)".to_string(),
            consultant: "Frontier Helper (http://127.0.0.1:8736/v1)".to_string(),
            about: "repeated the same output 4 times".to_string(),
            nth: 1,
            of: 2,
            usage: None,
        });

        let line = last_message(&app);
        assert_eq!(
            line,
            "! Looping Local consulted Frontier Helper (1/2) — repeated the same output 4 times",
            "the message should read as one sentence"
        );
        assert!(!line.contains("http://"), "no addresses: {line}");
        assert_eq!(
            line.matches("Looping Local").count(),
            1,
            "the driver should be named once: {line}"
        );
        assert_eq!(
            line.matches("Frontier Helper").count(),
            1,
            "the consultant should be named once: {line}"
        );
    }

    #[test]
    fn a_consult_keeps_the_turn_alive() {
        // Unlike an escalation, the turn is still running: the driver carries on.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "go");
        assert!(app.busy);

        app.handle_agent_event(consulted_event(None));

        assert!(app.busy, "the driver is still working on the same turn");
        assert!(!app.should_quit);
    }

    #[test]
    fn the_consultants_tokens_are_charged_to_the_consultant() {
        let (mut app, _commands, _cancel) = attached_with_cancel();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];
        type_and_send(&mut app, "go");
        let _ = app.messages.pop();

        app.handle_agent_event(consulted_event(Some(crate::provider::Usage {
            prompt_tokens: 900,
            completion_tokens: 40,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })));

        // The active tier is the driver, so charging there would be wrong.
        let spent = |name: &str| {
            app.usage_by_tier
                .iter()
                .find(|(tier, _)| tier == name)
                .map(|(_, usage)| usage.prompt_tokens)
        };
        assert_eq!(spent("DeepSeek"), Some(900), "{:?}", app.usage_by_tier);
        assert_eq!(spent("Local"), None, "the driver spent nothing");
        assert_eq!(app.tokens_in, 900);
    }

    #[test]
    fn the_transcript_says_where_the_consult_sits_in_its_budget() {
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "go");

        app.handle_agent_event(AgentEvent::Consulted {
            driver: "Local".to_string(),
            consultant: "DeepSeek".to_string(),
            about: "repeated the same output 4 times".to_string(),
            nth: 2,
            of: 2,
            usage: None,
        });

        assert!(
            last_message(&app).contains("(2/2)"),
            "a second consult should be distinguishable: {}",
            last_message(&app)
        );
    }

    #[test]
    fn a_single_consult_does_not_show_a_position() {
        // "1/1" would be noise when there is only one.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "go");

        app.handle_agent_event(AgentEvent::Consulted {
            driver: "Local".to_string(),
            consultant: "DeepSeek".to_string(),
            about: "repeated the same output 4 times".to_string(),
            nth: 1,
            of: 1,
            usage: None,
        });

        assert!(
            !last_message(&app).contains("(1/1)"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn a_short_preview_cannot_be_scrolled_at_all() {
        let mut app = new_app();
        let (reply, _answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "write_file".to_string(),
            preview: "create /tmp/a.txt".to_string(),
            reply,
        });

        app.handle_key(press(KeyCode::Down));

        assert_eq!(app.approval_scroll, 0, "there is nothing to scroll to");
    }

    #[test]
    fn a_new_approval_starts_at_the_top() {
        let (mut app, _answer) = long_preview_approval();
        app.handle_key(press(KeyCode::PageDown));
        assert!(app.approval_scroll > 0);

        let (reply, _answer) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "edit_file".to_string(),
            preview: "edit /tmp/a.txt".to_string(),
            reply,
        });

        assert_eq!(
            app.approval_scroll, 0,
            "a fresh preview must not open part-way down"
        );
    }

    #[test]
    fn the_answer_keys_still_work_with_a_scrolled_preview() {
        let (mut app, mut answer) = long_preview_approval();
        app.handle_key(press(KeyCode::PageDown));

        app.handle_key(press(KeyCode::Char('y')));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Approve);
        assert!(app.approval.is_none());
    }
}
