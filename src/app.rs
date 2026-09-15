//! Application state and key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::agent::AgentEvent;
use crate::agent::approval::{ApprovalRequest, Decision};
use crate::agent::first_line;
use crate::agent::{AllowChange, Canceller, Command, Mode};
use crate::commands::{self, Input};
use crate::config::Config;
use crate::config::OnStuck;
use crate::session_store::SessionFile;
use crate::stalls::Verdict;

/// How much of the prompt a single paste may add, in characters.
const MAX_PASTE: usize = 100_000;

/// How many lines PageUp and PageDown move the approval preview.
const APPROVAL_PAGE: i32 = 10;

/// How many turns of token history the session panel keeps for its sparkline.
const HISTORY: usize = 48;

/// How long the rail flashes the tier being abandoned, in redraws. At the event
/// loop's 90ms tick this is ~360ms: enough that the reason registers rather than
/// being a single frame nobody sees, short enough that it is a beat and not a
/// pause in the work.
pub(crate) const HANDOFF_TICKS: u64 = 4;

/// How often the interface redraws while a turn is running.
pub const TICK: std::time::Duration = std::time::Duration::from_millis(90);

/// Characters per token assumed until a turn reports real usage. English prose
/// runs around four; code and tool arguments run lower, so the first reply that
/// counts itself corrects this.
const ASSUMED_CHARS_PER_TOKEN: f64 = 4.0;

/// The band a learned ratio is kept in. A reply that reported two tokens for a
/// long message, or a flood of them for a short one, would otherwise teach a
/// ratio that makes every rate after it nonsense.
const CHARS_PER_TOKEN_MIN: f64 = 2.0;
const CHARS_PER_TOKEN_MAX: f64 = 8.0;

/// Completion tokens a turn must report before its own ratio is allowed to teach
/// anything. Below this the sample says less than the guess it would replace.
const CALIBRATE_MIN_TOKENS: u64 = 20;

/// The least stream a rate can be read from. A few characters over a fraction of
/// a second divides out to a number that swings wildly without saying anything,
/// so below either floor no rate is shown at all — which is honest in a way a
/// noisy figure is not.
const RATE_MIN_CHARS: usize = 24;
const RATE_MIN_TICKS: u64 = 3;

/// A handoff that has been announced and is still being shown.
#[derive(Debug, Clone, Copy)]
pub struct Handoff {
    /// The tier being abandoned, as a position in the rail.
    pub from: Option<usize>,
    /// The redraw counter at which the beat ends.
    pub until: u64,
}

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
    /// Characters streamed into the message being written, and the tick its first
    /// one arrived on. Reset when a new message starts, because a rate describes
    /// the text arriving now rather than the turn that text is part of.
    stream_chars: usize,
    stream_started: Option<u64>,
    /// How many characters of this model's output make a token. A conventional
    /// guess until a turn reports real usage, and learned from one after that.
    chars_per_token: f64,
    /// Whether a tool has run during this turn. A tool call's arguments arrive as
    /// structure rather than as text, so a reply that called one has completion
    /// tokens the character count never saw; a ratio learned from it would read
    /// every later rate high, so those turns are not allowed to teach.
    tool_ran_this_turn: bool,
    /// Index of the notice for a tool that is still running. The transcript puts
    /// a spinner here instead of the arrow it was written with.
    pub running: Option<usize>,
    pub approval: Option<PendingApproval>,
    pub busy: bool,

    /// Whether the fallback policy keeps the lower tier. Held here as well as in
    /// the chain because the session panel draws it.
    pub sticky: bool,
    /// A stuck policy chosen for this session, if one was. `None` means each
    /// tier's own from configuration, which the panel resolves per tier.
    pub on_stuck: Option<OnStuck>,
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
    /// The handoff currently being narrated, if one is.
    /// Presentation only: it decides which tier the rail draws as being
    /// abandoned, and for how long. The move itself has already happened by the
    /// time this is set.
    pub handoff: Option<Handoff>,
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
    /// What this turn has cost so far, summed across every tier and consult it
    /// has used.
    turn_usage: Option<crate::provider::Usage>,
    /// User turns completed.
    pub turns: u32,
    /// The most recent stall and everything the verdict was made from.
    pub last_stall: Option<Verdict>,
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
            stream_chars: 0,
            stream_started: None,
            chars_per_token: ASSUMED_CHARS_PER_TOKEN,
            tool_ran_this_turn: false,
            running: None,
            approval: None,
            busy: false,
            sticky,
            on_stuck: None,
            mode: Mode::default(),
            cancel: None,
            viewport: Rect::new(0, 0, 80, 24),
            approval_scroll: 0,
            usage_by_tier: Vec::new(),
            menu_index: 0,
            help: false,
            tick: 0,
            handoff: None,
            tier_labels: Vec::new(),
            active_tier: 0,
            tier_failed: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cache_write: 0,
            usage_history: Vec::new(),
            turn_usage: None,
            turns: 0,
            last_stall: None,
        }
    }

    /// Connect to a running agent, so prompts have somewhere to go.
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

    /// Pick up where the last run in this workspace left off.
    pub fn restore(&mut self, saved: &SessionFile, active_index: usize) {
        self.sticky = saved.sticky;
        self.on_stuck = saved.on_stuck;
        self.mode = saved.mode;
        self.active_tier = active_index.min(self.tier_labels.len().saturating_sub(1));

        let resumed = render_session(&saved.messages);
        let count = resumed.len();
        self.messages.extend(resumed);

        // Said out loud, because a transcript that appears from nowhere is
        // confusing, and because the one thing a resumed session must not be is
        // silent about being resumed.
        self.messages.push(Message::system(format!(
            "resumed this session — {count} message{}, last saved {}",
            if count == 1 { "" } else { "s" },
            ago(saved.saved_at)
        )));
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
            // Choosing a tier, or landing on it, means it is answering — not
            // spent. Otherwise /tier 1 after a spill left the chip on ✗.
            if let Some(failed) = self.tier_failed.get_mut(index) {
                *failed = false;
            }
        }
    }

    /// Add one turn's tokens to the session totals, and to the tier that spent
    /// them.
    pub fn record_usage_on(&mut self, tier: Option<&str>, usage: &crate::provider::Usage) {
        self.tokens_in = self.tokens_in.saturating_add(usage.prompt_tokens);
        self.tokens_out = self.tokens_out.saturating_add(usage.completion_tokens);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read_tokens);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write_tokens);

        // The per-turn history is deliberately *not* recorded here. This is
        // called once per attempt, and a turn can make several — one per tier it
        // tried, plus any consult — so pushing here would put several bars in
        // the graph for a single prompt and quietly change what it means.
        // `close_turn` records the turn's total once, when the turn is over.
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

    /// Finish the current turn's tally, returning the line to show for it.
    fn close_turn(&mut self) -> Option<String> {
        let usage = self.turn_usage.take()?;

        // One bar per turn, which is what the graph claims to show.
        self.usage_history.push(usage.prompt_tokens);
        if self.usage_history.len() > HISTORY {
            self.usage_history.remove(0);
        }

        Some(usage_line(&usage))
    }

    /// Add one turn's tokens to the session totals, with no tier to attribute
    /// them to.
    #[cfg(test)]
    pub fn record_usage(&mut self, usage: &crate::provider::Usage) {
        self.record_usage_on(None, usage);
    }

    /// Put the app in the state a streaming reply leaves it in, so the rail can be
    /// asked to draw a rate with no model behind it: `chars` characters arriving
    /// over the last `ticks` redraws.
    #[cfg(test)]
    pub fn pretend_to_stream(&mut self, chars: usize, ticks: u64) {
        self.streaming = Some(0);
        self.stream_chars = chars;
        self.stream_started = Some(self.tick.saturating_sub(ticks));
    }

    /// Whether a tier is being narrated as abandoned right now, and which one.
    pub fn abandoning_tier(&self) -> Option<usize> {
        match self.handoff {
            Some(handoff) if self.tick < handoff.until => handoff.from,
            _ => None,
        }
    }

    /// The stuck policy actually in force, for the rail to state.
    pub fn on_stuck(&self) -> OnStuck {
        if let Some(chosen) = self.on_stuck {
            return chosen;
        }
        self.config
            .tiers
            .get(self.active_tier)
            .map(|tier| tier.on_stuck)
            .unwrap_or_default()
    }

    /// Whether the policy is this session's choice rather than the config's.
    pub fn on_stuck_is_chosen(&self) -> bool {
        self.on_stuck.is_some()
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
    fn cancel_turn(&mut self) {
        match &self.cancel {
            Some(cancel) => cancel.cancel(),
            None => self
                .messages
                .push(Message::system("there is nothing to cancel")),
        }
    }

    /// Take a pasted block of text into the prompt.
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

    /// How fast the model is writing, in tokens per second.
    pub fn stream_rate(&self) -> Option<f64> {
        // Nothing is being written, so there is no rate to report.
        self.streaming?;

        // And until the first character has arrived there is no clock to time it
        // against either.
        let started = self.stream_started?;
        let ticks = self.tick.saturating_sub(started);
        if ticks < RATE_MIN_TICKS || self.stream_chars < RATE_MIN_CHARS {
            return None;
        }

        let seconds = ticks as f64 * TICK.as_secs_f64();
        let tokens = self.stream_chars as f64 / self.chars_per_token;

        Some(tokens / seconds)
    }

    /// Learn what a token is worth in characters from a reply that reported one.
    fn learn_chars_per_token(&mut self, tier: &str, completion_tokens: u64) {
        let streamed = std::mem::take(&mut self.stream_chars);

        if self.tool_ran_this_turn
            || self.active_tier_name() != Some(tier)
            || streamed == 0
            || completion_tokens < CALIBRATE_MIN_TOKENS
        {
            return;
        }

        let measured = streamed as f64 / completion_tokens as f64;
        self.chars_per_token = measured.clamp(CHARS_PER_TOKEN_MIN, CHARS_PER_TOKEN_MAX);
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
                        // A new message is a new measurement: the rate describes
                        // the text arriving now. Its clock starts at the first
                        // character, not at the request, so the seconds a local
                        // model spends reading the prompt are not charged to
                        // writing and a fast model does not look slow.
                        self.stream_chars = 0;
                        self.stream_started = Some(self.tick);
                        index
                    }
                };
                self.stream_chars += chunk.chars().count();
                if let Some(message) = self.messages.get_mut(index) {
                    message.text.push_str(&chunk);
                }
            }
            AgentEvent::Stalled { verdict } => {
                // Nothing is shown here: the handoff or the consult narrates
                // itself, and a second line saying the same thing in different
                // words is noise. This is kept so `/why` can answer with the
                // numbers rather than with the sentence already on screen.
                self.last_stall = Some(*verdict);
            }
            AgentEvent::AlmostStalled { tier, miss } => {
                self.streaming = None;
                // One line, and it has to stay one: this lands in the middle of
                // a transcript, and a warning that wraps is a wall. Hence the
                // counters as "3 of 4" rather than a sentence about what would
                // have happened, and the tersest possible pointer at `/why`.
                self.messages.push(Message::system(format!(
                    "nearly spilled · {tier} {} · /why",
                    miss.sentence()
                )));
            }
            AgentEvent::ToolStarted { name, preview } => {
                self.streaming = None;
                // A tool call's arguments arrive as structure, never as text, so
                // from here this turn's completion tokens and its streamed
                // characters are no longer the same thing. Remembered for the
                // rest of the turn, because usage is reported per attempt and
                // arrives after the tool has run.
                self.tool_ran_this_turn = true;
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
            AgentEvent::Spent { tier, usage } => {
                // The one place tokens are counted, so nothing can be missed or
                // counted twice: an attempt that was abandoned, one that
                // answered, and a consult all arrive here.
                self.record_usage_on(Some(&tier), &usage);
                self.learn_chars_per_token(&tier, usage.completion_tokens);
                crate::provider::accumulate(&mut self.turn_usage, Some(usage));
            }
            AgentEvent::Finished { stop_reason } => {
                self.streaming = None;
                self.running = None;
                self.busy = false;
                // The turn is over, so the next one starts with a clean sheet: a
                // turn that ran a tool must not go on disqualifying the turns that
                // follow it, and no measurement outlives the stream it described.
                self.tool_ran_this_turn = false;
                self.stream_chars = 0;
                self.stream_started = None;
                // The beat belongs to a turn that is still running. It has to be
                // dropped here rather than left to expire on its own, because the
                // tick counter stops when nothing is busy — a beat left set would
                // freeze mid-flash until the next turn.
                self.handoff = None;
                if stop_reason.as_deref() == Some("length") {
                    self.messages.push(Message::system(
                        "! the model hit its output limit, so this answer is incomplete",
                    ));
                }
                // The whole turn, not the last request in it: a turn that read
                // four files made five requests, and this is what they cost.
                if let Some(line) = self.close_turn() {
                    self.messages.push(Message::system(line));
                }
            }
            AgentEvent::Spilling { from, to, reason } => {
                // The verdict has landed, so the attempt's partial output is dead
                // matter and goes now — doing it here rather than at `Escalated`
                // keeps the transcript index of the line below stable.
                self.discard_streaming_message();
                self.running = None;

                // Narrated before the move rather than with it. The tier is
                // already stopped when this arrives; what the beat buys is that
                // the reason is on screen for a moment before the next tier's
                // answer starts arriving under it.
                self.messages.push(Message::system(format!(
                    "✗ {from} {reason} — spilling over to {to}"
                )));
                self.handoff = Some(Handoff {
                    from: self.tier_index(&from),
                    until: self.tick.saturating_add(HANDOFF_TICKS),
                });
            }
            AgentEvent::Escalated { from, to, .. } => {
                // The narration and the flash were started by `Spilling`, which
                // always precedes this. All that is left is to record the move.
                // `/escalate` sends this without `Spilling`, so the rail still
                // has to move.
                self.fail_tier(&from);
                self.activate_tier(&to);
            }
            AgentEvent::Switched { to } => {
                self.activate_tier(&to);
            }
            AgentEvent::Cancelled { tier } => {
                // The half-streamed answer never reached the conversation, so
                // showing it would leave the transcript claiming something the
                // model was never told. Everything before it stands.
                self.discard_streaming_message();
                self.streaming = None;
                self.running = None;
                self.busy = false;
                self.handoff = None;
                // A modal can only be up while a turn runs, so stopping the turn
                // takes it down too. The reply channel is already gone.
                self.approval = None;
                self.messages
                    .push(Message::system(format!("✗ you stopped {tier}")));
                // Requests were made and billed before the stop, so the turn
                // closes with what it cost rather than dropping it.
                if let Some(line) = self.close_turn() {
                    self.messages.push(Message::system(line));
                }
            }
            AgentEvent::Consulted {
                driver,
                consultant,
                about,
                nth,
                of,
            } => {
                // The failed attempt is being thrown away in the conversation, so
                // it has to leave the transcript too. Without this the looped
                // output that got the tier stuck stays on screen looking like
                // part of the answer, which is exactly what the rollback exists
                // to prevent.
                self.discard_streaming_message();
                self.streaming = None;
                self.running = None;
                // The consultant's tokens were already counted, and charged to
                // the consultant rather than to the driver, by the `Spent` event
                // that arrived just before this one. `/cost` compares the two
                // tiers, so it has to be the tier that did the work.
                //
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
                self.handoff = None;
                // Names the next command, because this is the message someone
                // sees when their configuration has stopped working and they
                // have no idea which of the three commands to reach for.
                self.messages.push(Message::system(format!(
                    "✗ no tier could answer: {reason}\n{}",
                    crate::tiers::UNREACHABLE_NEXT_STEPS
                )));
                // Every tier that was tried had been billed for it.
                if let Some(line) = self.close_turn() {
                    self.messages.push(Message::system(line));
                }
            }
        }
        self.scroll_back = 0;
    }

    /// Drop the assistant message being streamed, if it is still the last thing
    /// in the transcript.
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

            "why" => self.messages.push(Message::system(self.describe_why())),

            // These need the chain or the conversation, which the agent owns.
            "escalate" => {
                send(self, Command::Escalate);
            }

            "deescalate" => {
                send(self, Command::SetTier(None));
            }

            "consult" => {
                send(self, Command::Consult);
            }

            "on-stuck" => match argument.trim().to_lowercase().as_str() {
                "escalate" | "consult" | "auto" => {
                    // Recorded only once the chain has been told, so the panel
                    // cannot report a policy that is not in effect.
                    let chosen = match argument.trim().to_lowercase().as_str() {
                        "escalate" => Some(OnStuck::Escalate),
                        "consult" => Some(OnStuck::Consult),
                        // Not a third policy: the absence of one, which is the
                        // only way back to a chain whose tiers differ.
                        _ => None,
                    };
                    if send(self, Command::SetOnStuck(chosen)) {
                        self.on_stuck = chosen;
                    }
                }
                other => {
                    let complaint = if other.is_empty() {
                        "usage: /on-stuck <escalate|consult|auto>".to_string()
                    } else {
                        format!("/on-stuck takes escalate, consult or auto, not {other:?}")
                    };
                    self.messages.push(Message::system(complaint));
                }
            },
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
                    self.turn_usage = None;
                    self.tool_ran_this_turn = false;
                    self.stream_chars = 0;
                    self.stream_started = None;
                    self.scroll_back = 0;
                }
            }
            "context" => {
                send(self, Command::Context);
            }
            "undo" => {
                // Nothing to do here: the pre-image lives with the agent, which
                // is what ran the tool, and the result comes back as a notice.
                send(self, Command::Undo);
            }

            "allow" => {
                // The rules live with the agent too, for the same reason: it is
                // what runs a tool, and it answers with a notice. So there is one
                // copy of the list and no second one to keep in step.
                match allow_change(argument.trim()) {
                    Ok(Some(change)) => {
                        send(self, Command::Allow(change));
                    }
                    Ok(None) => {
                        send(self, Command::Allow(AllowChange::List));
                    }
                    Err(usage) => self.messages.push(Message::system(usage)),
                }
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
            "\n/tier <name|number> chooses one. /tier auto or /deescalate returns to the first.",
        );
        out
    }

    /// Where the session's tokens went, tier by tier: what `/cost` shows.
    pub(crate) fn describe_why(&self) -> String {
        let Some(verdict) = self.last_stall.as_ref() else {
            return "nothing has spilled this session, so there is nothing to explain".to_string();
        };

        let mut out = verdict.report();
        // Saying where the record is, because the report answers one stall and
        // the log is what answers a pattern of them.
        match crate::stalls::SpillLog::default_path() {
            Some(path) => out.push_str(&format!(
                "\n\nevery spill is recorded at {}",
                crate::ui::short_path(&path)
            )),
            None => out.push_str("\n\nno state directory, so spills are not being recorded"),
        }
        out
    }

    pub(crate) fn describe_cost(&self) -> String {
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
fn allow_change(argument: &str) -> Result<Option<AllowChange>, &'static str> {
    /// The one word that makes `save` unmistakable for a rule.
    const USAGE: &str = "usage: /allow save <words>, such as /allow save git status";

    if argument.is_empty() {
        return Ok(None);
    }

    // Subcommands before rules, so a bare `save` is a mistake explained rather
    // than a rule for a program nobody has called `save` on purpose.
    if argument == "clear" {
        return Ok(Some(AllowChange::Clear));
    }

    if let Some((first, rest)) = argument.split_once(char::is_whitespace) {
        if first == "save" {
            let words = rest.trim();
            return if words.is_empty() {
                Err(USAGE)
            } else {
                Ok(Some(AllowChange::Save(words.to_string())))
            };
        }
    }

    if argument == "save" {
        return Err(USAGE);
    }

    // Everything else is a rule, including anything the agent will refuse: the
    // message that explains why is better than one that quietly does nothing.
    Ok(Some(AllowChange::Add(argument.to_string())))
}

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

/// Render a saved conversation into the transcript the interface draws.
fn render_session(messages: &[crate::session::ChatMessage]) -> Vec<Message> {
    use crate::session::Role as Wire;

    let mut rendered = Vec::new();
    for message in messages {
        match message.role {
            // There is exactly one system message and it is rebuilt on load;
            // anything else here is not conversation.
            Wire::System => continue,
            Wire::User => rendered.push(Message {
                role: Role::User,
                text: message.content.clone(),
            }),
            Wire::Assistant => {
                if !message.content.trim().is_empty() {
                    rendered.push(Message {
                        role: Role::Assistant,
                        text: message.content.clone(),
                    });
                }
                // A turn that only called tools has no text, so the calls are
                // what happened and are shown as such rather than dropped.
                for call in &message.tool_calls {
                    rendered.push(Message::system(format!("⚙ {}", call.name)));
                }
            }
            Wire::Tool => rendered.push(Message::system(format!(
                "↳ {}",
                first_line(&message.content)
            ))),
        }
    }
    rendered
}

/// How long ago something happened, in the few words a one-line notice wants.
fn ago(saved_at: u64) -> String {
    let seconds = crate::session_store::now_epoch().saturating_sub(saved_at);
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
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

    /// Stream a reply of `chars` characters, long enough to clear the floor a rate
    /// needs so that a test is about the rate rather than about the minimum.
    fn send_chars(app: &mut App, chars: usize) {
        app.handle_agent_event(AgentEvent::Text("x".repeat(chars)));
    }

    /// A completion report for the answering tier, as the agent sends one.
    fn reports(app: &mut App, tier: &str, completion_tokens: u64) {
        app.handle_agent_event(AgentEvent::Spent {
            tier: tier.to_string(),
            usage: crate::provider::Usage {
                completion_tokens,
                ..Default::default()
            },
        });
    }

    #[test]
    fn no_rate_is_offered_until_there_is_a_stream_to_read() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");

        assert!(
            app.stream_rate().is_none(),
            "a turn that has not started writing has no rate"
        );

        // Text on this very tick leaves no elapsed time to divide by.
        send_chars(&mut app, 400);
        assert!(app.stream_rate().is_none(), "nothing has elapsed yet");

        // Enough time, but too few characters to be anything but noise.
        app.stream_chars = 4;
        app.tick = 100;
        assert!(app.stream_rate().is_none(), "four characters is noise");
    }

    #[test]
    fn the_rate_is_measured_from_the_first_chunk_not_from_the_request() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");

        // The prompt has been in the model's hands for a while: a local model
        // reading a long prompt spends seconds here before writing anything.
        app.tick = 100;
        send_chars(&mut app, 360);

        // 360 characters at the assumed four to a token is 90 tokens, over the 3.6s
        // since the first character arrived. Charging the prefill as well would
        // report 90/12.6, and make every local model look slow.
        app.tick = 140;
        let rate = app.stream_rate().expect("a rate once text is arriving");

        assert!(
            (rate - 25.0).abs() < 0.01,
            "expected 25 tok/s from the first chunk, got {rate}"
        );
    }

    #[test]
    fn each_message_is_measured_on_its_own_clock() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");

        app.tick = 50;
        send_chars(&mut app, 360);

        // A tool ends the message. The answer after it is a different reply and is
        // timed from its own first character, not from the one before the tool.
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });
        app.tick = 200;
        send_chars(&mut app, 360);
        app.tick = 240;

        let rate = app.stream_rate().expect("the answer is streaming");
        assert!(
            (rate - 25.0).abs() < 0.01,
            "the tool's 110 ticks must not be charged to writing: {rate}"
        );
    }

    #[test]
    fn a_reply_that_counted_itself_teaches_what_a_token_weighs() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);

        reports(&mut app, "Local model", 100);

        assert_eq!(
            app.chars_per_token, 3.0,
            "300 characters for 100 tokens is three to a token"
        );

        // And the estimate uses it, which is the only reason to learn it: a fresh
        // reply of the same 300 characters is 100 tokens, over 1.8s.
        app.streaming = None;
        app.tick = 100;
        send_chars(&mut app, 300);
        app.tick = 120;

        let rate = app.stream_rate().expect("a rate");
        assert!(
            (rate - 55.56).abs() < 0.1,
            "the learned ratio should be in use, not the guess: {rate}"
        );
    }

    #[test]
    fn a_turn_that_ran_a_tool_teaches_nothing() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });

        reports(&mut app, "Local model", 100);

        assert_eq!(
            app.chars_per_token, ASSUMED_CHARS_PER_TOKEN,
            "the arguments never streamed, so the comparison is not fair"
        );
    }

    #[test]
    fn a_consult_does_not_teach_the_answering_tiers_ratio() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);

        reports(&mut app, "DeepSeek", 100);

        assert_eq!(
            app.chars_per_token, ASSUMED_CHARS_PER_TOKEN,
            "the characters to hand belong to the driver, not the consultant"
        );
    }

    #[test]
    fn a_reply_too_short_to_measure_teaches_nothing() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);

        reports(&mut app, "Local model", CALIBRATE_MIN_TOKENS - 1);

        assert_eq!(app.chars_per_token, ASSUMED_CHARS_PER_TOKEN);
    }

    #[test]
    fn a_wild_ratio_is_kept_within_reason() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");

        // Four thousand characters for twenty tokens is 200 to a token, which would
        // report every rate afterwards at fifty times life size.
        send_chars(&mut app, 4_000);
        reports(&mut app, "Local model", CALIBRATE_MIN_TOKENS);
        assert_eq!(app.chars_per_token, CHARS_PER_TOKEN_MAX);

        // And the other way: a couple of tokens' worth of report against a long
        // stream would make every later rate read low.
        app.stream_chars = 300;
        reports(&mut app, "Local model", 200);
        assert_eq!(app.chars_per_token, CHARS_PER_TOKEN_MIN);
    }

    #[test]
    fn a_report_cannot_reuse_an_earlier_replys_characters() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);
        reports(&mut app, "Local model", 100);
        assert_eq!(app.chars_per_token, 3.0);

        // A second report for the same tier, with no new text behind it. If the
        // first had not consumed the characters it was paired with, this would
        // measure itself against them and read 6.0.
        reports(&mut app, "Local model", 50);

        assert_eq!(
            app.chars_per_token, 3.0,
            "a report is paired with its own stream, and only once"
        );
    }

    #[test]
    fn a_tool_in_one_turn_does_not_disqualify_the_next() {
        let (mut app, _commands) = attached_app();
        type_and_send(&mut app, "hello");
        send_chars(&mut app, 300);
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("stop".to_string()),
        });
        assert_eq!(app.chars_per_token, ASSUMED_CHARS_PER_TOKEN);

        // The next turn answers with prose and no tools, which is the fair sample
        // the last turn could not be.
        type_and_send(&mut app, "and now?");
        send_chars(&mut app, 300);
        reports(&mut app, "Local model", 100);

        assert_eq!(
            app.chars_per_token, 3.0,
            "one turn's tools must not silence every turn after it"
        );
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

        // The move arrives in two parts, as the agent sends it: announced, then
        // committed. The discarding belongs to the announcement, because the
        // verdict is what makes the partial answer dead.
        app.handle_agent_event(AgentEvent::Spilling {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated the same output 4 times".to_string(),
        });
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
        });

        assert!(!app.busy);
    }

    #[test]
    fn token_usage_is_reported_when_the_server_gives_it() {
        let mut app = new_app();
        spend_and_finish(
            &mut app,
            crate::provider::Usage {
                prompt_tokens: 120,
                completion_tokens: 45,
                ..Default::default()
            },
        );

        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("120 in, 45 out"))
        );
    }

    #[test]
    fn cache_hits_are_shown_alongside_the_token_counts() {
        let mut app = new_app();
        spend_and_finish(
            &mut app,
            crate::provider::Usage {
                prompt_tokens: 15360,
                completion_tokens: 2,
                cache_read_tokens: 7424,
                cache_write_tokens: 0,
            },
        );

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
        spend_and_finish(
            &mut app,
            crate::provider::Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                ..Default::default()
            },
        );

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
    fn switching_back_clears_the_failed_mark_and_moves_the_rail() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(
            tx,
            Canceller::default(),
            &["Local".to_string(), "Grok".to_string()],
            None,
        );
        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local".to_string(),
            to: "Grok".to_string(),
            reason: "you asked".to_string(),
        });
        assert_eq!(app.active_tier, 1);
        assert!(app.tier_failed[0]);

        app.handle_agent_event(AgentEvent::Switched {
            to: "Local".to_string(),
        });
        assert_eq!(app.active_tier, 0);
        assert!(
            !app.tier_failed[0],
            "the tier that is answering is not spent"
        );
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

    // The flash a spill draws is covered by the beat tests near the end of this
    // module: it is started by `Spilling` now rather than by `Escalated`, so the
    // assertions live with the event that starts it.

    #[test]
    fn the_usage_history_keeps_only_the_recent_turns() {
        let mut app = new_app();
        for turn in 0..(HISTORY as u64 + 30) {
            spend_and_finish(
                &mut app,
                crate::provider::Usage {
                    prompt_tokens: turn,
                    ..Default::default()
                },
            );
        }

        assert_eq!(app.usage_history.len(), HISTORY);
        assert_eq!(
            *app.usage_history.last().expect("the newest turn"),
            HISTORY as u64 + 29,
            "the oldest are dropped, not the newest"
        );
    }

    #[test]
    fn the_graph_gets_one_bar_per_turn_even_when_the_turn_spilled() {
        // Two attempts means two spends, but the graph is labelled per turn and
        // has to stay that way: a spill would otherwise look like two prompts.
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];

        app.handle_agent_event(AgentEvent::Spent {
            tier: "Local".to_string(),
            usage: crate::provider::Usage {
                prompt_tokens: 2_000,
                ..Default::default()
            },
        });
        app.handle_agent_event(AgentEvent::Spent {
            tier: "DeepSeek".to_string(),
            usage: crate::provider::Usage {
                prompt_tokens: 15_000,
                ..Default::default()
            },
        });
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
        });

        assert_eq!(
            app.usage_history.len(),
            1,
            "one prompt, one bar: {:?}",
            app.usage_history
        );
        assert_eq!(
            app.usage_history[0], 17_000,
            "and the bar is what the whole turn cost"
        );
        assert!(
            last_message(&app).contains("17,000 in"),
            "the transcript should agree with the graph: {}",
            last_message(&app)
        );
    }

    #[test]
    fn a_cancelled_turn_still_counts_what_it_cost() {
        // Requests were made and billed before the stop, so the turn closes with
        // what it spent rather than dropping it on the floor.
        let (mut app, _commands, _cancel) = attached_with_cancel();
        type_and_send(&mut app, "do something");

        app.handle_agent_event(AgentEvent::Spent {
            tier: "Local model".to_string(),
            usage: crate::provider::Usage {
                prompt_tokens: 1_234,
                completion_tokens: 56,
                ..Default::default()
            },
        });
        app.handle_agent_event(AgentEvent::Cancelled {
            tier: "Local model".to_string(),
        });

        assert_eq!(app.tokens_in, 1_234, "the spend is kept");
        assert_eq!(app.usage_history.len(), 1, "and graphed");
        assert!(
            last_message(&app).contains("1,234 in"),
            "the user should see what the stopped turn cost: {}",
            last_message(&app)
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

        spend_and_finish(&mut app, turn);
        spend_and_finish(&mut app, turn);

        assert_eq!(
            app.usage_history.len(),
            2,
            "one bar per turn, not per request"
        );
        assert_eq!(app.tokens_in, 30_720);
        assert_eq!(app.tokens_out, 4);
        assert_eq!(app.cache_read, 14_848);
        assert_eq!(app.cache_write, 0);
    }

    // ---- /why -------------------------------------------------------------

    use crate::stalls::Miss;

    fn a_stall_verdict() -> Verdict {
        Verdict {
            tier_id: "local".into(),
            tier_name: "Looping Local".into(),
            reason: crate::detect::StuckReason::RepeatedToolError {
                tool: "read_file".into(),
                class: crate::detect::ErrorClass::NotFound,
                times: 3,
            },
            counters: crate::stalls::Counters {
                steps_used: 5,
                steps_allowed: 12,
                repetition: crate::detect::repetition::RepetitionCounters {
                    consecutive: 1,
                    span_repeats: 1,
                    threshold: 4,
                },
                progress: crate::detect::progress::ProgressCounters {
                    same_run: 1,
                    failure_run: 3,
                    error_run: 3,
                    error_class: Some(crate::detect::ErrorClass::NotFound),
                    threshold: 4,
                },
                timing: crate::detect::Timing {
                    worst_gap_ms: 900,
                    worst_allowance_ms: 30_000,
                    first_token_ms: 120_000,
                    idle_ms: 30_000,
                    worst_phase: crate::detect::Phase::Idle,
                },
            },
        }
    }

    #[test]
    fn why_with_nothing_to_explain_says_so() {
        let mut app = new_app();
        type_and_send(&mut app, "/why");

        assert!(
            last_message(&app).contains("nothing has spilled"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn why_reports_the_last_stall_with_its_counters() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Stalled {
            verdict: Box::new(a_stall_verdict()),
        });
        type_and_send(&mut app, "/why");

        let report = last_message(&app);
        assert!(report.contains("Looping Local"), "{report}");
        assert!(
            report.contains("read_file failed 3 times"),
            "the verdict leads: {report}"
        );
        assert!(report.contains("5 of 12 used"), "{report}");
        assert!(
            report.contains("identical line(s)"),
            "the counters that did not fire are the point: {report}"
        );
    }

    #[test]
    fn why_says_where_the_record_of_every_spill_is() {
        // One stall is answered by the report; a pattern of them is answered by
        // the file, so the file has to be findable from here.
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Stalled {
            verdict: Box::new(a_stall_verdict()),
        });
        type_and_send(&mut app, "/why");

        let report = last_message(&app);
        assert!(
            report.contains("spills.jsonl") || report.contains("no state directory"),
            "{report}"
        );
    }

    #[test]
    fn why_reports_the_most_recent_stall_not_the_first() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::Stalled {
            verdict: Box::new(a_stall_verdict()),
        });
        app.handle_agent_event(AgentEvent::Stalled {
            verdict: Box::new(Verdict {
                tier_id: "frontier".into(),
                tier_name: "Frontier".into(),
                reason: crate::detect::StuckReason::StepLimit { steps: 12 },
                ..a_stall_verdict()
            }),
        });
        type_and_send(&mut app, "/why");

        let report = last_message(&app);
        assert!(report.contains("Frontier"), "{report}");
        assert!(!report.contains("Looping Local"), "{report}");
    }

    #[test]
    fn a_near_miss_is_shown_as_an_aside_in_the_transcript() {
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::AlmostStalled {
            tier: "Local".into(),
            miss: Miss::Repeats {
                seen: 3,
                allowed: 4,
            },
        });

        let line = last_message(&app);
        assert!(line.contains("nearly spilled"), "{line}");
        assert!(line.contains("Local"), "{line}");
        assert!(
            line.contains("repeated the same line 3 of 4 times"),
            "{line}"
        );
        assert!(
            line.contains("/why"),
            "it should say where the numbers are: {line}"
        );
        assert!(!line.contains("http://"), "notices use short names: {line}");
    }

    #[test]
    fn a_near_miss_is_worth_exactly_one_line() {
        // One line is the requirement, not merely a preference: this lands in
        // the middle of a transcript, and a wrapped warning is a wall.
        let mut app = new_app();
        app.handle_agent_event(AgentEvent::AlmostStalled {
            tier: "Local".into(),
            miss: Miss::SameCall {
                seen: 3,
                allowed: 4,
            },
        });

        let line = last_message(&app);
        assert_eq!(line.lines().count(), 1, "{line:?}");
        assert!(
            line.chars().count() <= 110,
            "{} chars: {line}",
            line.chars().count()
        );
    }

    // ---- slash commands ---------------------------------------------------

    fn last_message(app: &App) -> &str {
        app.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    /// Drive one request's spend and then the end of the turn, the way the agent
    /// does: the money arrives as its own event, before the turn ends.
    fn spend_and_finish(app: &mut App, usage: crate::provider::Usage) {
        let tier = app.active_tier_name().unwrap_or("a tier").to_string();
        app.handle_agent_event(AgentEvent::Spent { tier, usage });
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
        });
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
            ("/deescalate", Command::SetTier(None)),
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

    // ---- the stuck policy -------------------------------------------------

    #[test]
    fn the_rail_policy_follows_the_answering_tier_until_it_is_chosen() {
        // Tiers carry their own, so what the rail says has to follow whichever
        // one is answering rather than reporting a single global setting.
        let (mut app, _commands) = app_with_policies(&[OnStuck::Consult, OnStuck::Escalate]);
        assert_eq!(app.on_stuck(), OnStuck::Consult, "the first tier consults");
        assert!(!app.on_stuck_is_chosen());

        app.activate_tier("second");
        assert_eq!(
            app.on_stuck(),
            OnStuck::Escalate,
            "the second tier escalates, and the rail should say so"
        );
    }

    #[test]
    fn a_session_choice_outranks_every_tier() {
        let (mut app, _commands) = app_with_policies(&[OnStuck::Escalate, OnStuck::Escalate]);

        app.messages.clear();
        type_and_send(&mut app, "/on-stuck consult");
        assert_eq!(app.on_stuck(), OnStuck::Consult);
        assert!(
            app.on_stuck_is_chosen(),
            "the rail should show that this was chosen, not configured"
        );

        // And back, so a session override is not a one-way door.
        type_and_send(&mut app, "/on-stuck escalate");
        assert_eq!(app.on_stuck(), OnStuck::Escalate);
    }

    #[test]
    fn on_stuck_refuses_nonsense_without_changing_anything() {
        let (mut app, _commands) = app_with_policies(&[OnStuck::Escalate, OnStuck::Escalate]);
        let before = app.on_stuck();

        type_and_send(&mut app, "/on-stuck maybe");

        assert_eq!(app.on_stuck(), before);
        assert!(!app.on_stuck_is_chosen());
        assert!(
            last_message(&app).contains("escalate, consult or auto"),
            "{}",
            last_message(&app)
        );

        // The bare form explains itself rather than guessing.
        type_and_send(&mut app, "/on-stuck");
        assert!(
            last_message(&app).contains("usage"),
            "{}",
            last_message(&app)
        );
    }

    #[test]
    fn consult_is_forwarded_to_the_agent() {
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "/consult");

        assert_eq!(
            commands.try_recv().expect("the command should be sent"),
            Command::Consult
        );
    }

    #[test]
    fn on_stuck_is_forwarded_to_the_agent() {
        let (mut app, mut commands) = attached_app();
        type_and_send(&mut app, "/on-stuck consult");

        assert_eq!(
            commands.try_recv().expect("the command should be sent"),
            Command::SetOnStuck(Some(OnStuck::Consult))
        );
    }

    #[test]
    fn auto_hands_the_policy_back_to_each_tier() {
        // Without this, one `/on-stuck` would be a one-way door: every concrete
        // policy flattens the chain to a single answer, so the only way back to
        // an arrangement where two tiers differ would be a restart.
        let (mut app, mut commands) = app_with_policies(&[OnStuck::Consult, OnStuck::Escalate]);
        app.messages.clear();

        type_and_send(&mut app, "/on-stuck escalate");
        assert_eq!(
            app.on_stuck(),
            OnStuck::Escalate,
            "the override applies to the tier that consults too"
        );

        type_and_send(&mut app, "/on-stuck auto");

        // Both went to the agent, in order, and the second says "no policy"
        // rather than naming a third one.
        assert_eq!(
            commands.try_recv().expect("the first was forwarded"),
            Command::SetOnStuck(Some(OnStuck::Escalate))
        );
        assert_eq!(
            commands.try_recv().expect("and so was the second"),
            Command::SetOnStuck(None),
            "auto is the absence of a policy, not another one"
        );

        assert!(!app.on_stuck_is_chosen(), "the choice was handed back");
        assert_eq!(
            app.on_stuck(),
            OnStuck::Consult,
            "and the first tier's own policy is back in force"
        );

        app.activate_tier("second");
        assert_eq!(
            app.on_stuck(),
            OnStuck::Escalate,
            "as is the second's — which is what going back means"
        );
    }

    #[test]
    fn a_policy_is_not_recorded_when_no_agent_can_be_told() {
        // Otherwise the rail would report a policy nothing is enforcing, which
        // is the same mistake `/sticky` made before it was fixed.
        let mut app = new_app();
        let before = app.on_stuck();

        type_and_send(&mut app, "/on-stuck consult");

        assert_eq!(
            app.on_stuck(),
            before,
            "nothing was told, so nothing changed"
        );
        assert!(!app.on_stuck_is_chosen());
        assert!(
            last_message(&app).contains("nowhere to go"),
            "{}",
            last_message(&app)
        );
    }

    /// An attached app whose tiers carry the given stuck policies.
    fn app_with_policies(
        policies: &[OnStuck],
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let tiers: String = policies
            .iter()
            .enumerate()
            .map(|(index, policy)| {
                let kind = match policy {
                    OnStuck::Consult => "consult",
                    OnStuck::Escalate => "escalate",
                };
                format!(
                    "\n[[tier]]\nid = \"tier-{index}\"\nname = \"{}\"\nkind = \"openai\"\n                     base_url = \"http://localhost:1234/v1\"\non_stuck = \"{kind}\"\n",
                    match index {
                        0 => "first",
                        _ => "second",
                    }
                )
            })
            .collect();

        let config = Config::parse(
            std::path::Path::new("test.toml"),
            &format!("schema = 1\n{tiers}"),
        )
        .expect("the test config should be valid");

        let labels: Vec<String> = config
            .tiers
            .iter()
            .map(|tier| tier.display_name().to_string())
            .collect();
        let mut app = App::new(config);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.attach(tx, Canceller::default(), &labels, None);
        (app, rx)
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

    fn consulted_event() -> AgentEvent {
        AgentEvent::Consulted {
            driver: "Local".to_string(),
            consultant: "DeepSeek".to_string(),
            about: "repeated the same output 4 times".to_string(),
            nth: 1,
            of: 2,
        }
    }

    /// What a consult cost, reported the way the agent reports it: as a spend by
    /// the consultant, before the consult itself is announced.
    fn consulted_and_spent(app: &mut App, usage: crate::provider::Usage) {
        app.handle_agent_event(AgentEvent::Spent {
            tier: "DeepSeek".to_string(),
            usage,
        });
        app.handle_agent_event(consulted_event());
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

        app.handle_agent_event(consulted_event());

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

        app.handle_agent_event(consulted_event());

        assert!(app.busy, "the driver is still working on the same turn");
        assert!(!app.should_quit);
    }

    #[test]
    fn the_consultants_tokens_are_charged_to_the_consultant() {
        let (mut app, _commands, _cancel) = attached_with_cancel();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];
        type_and_send(&mut app, "go");
        let _ = app.messages.pop();

        consulted_and_spent(
            &mut app,
            crate::provider::Usage {
                prompt_tokens: 900,
                completion_tokens: 40,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        );

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

        // And it counts towards the turn, so the turn's closing line is the
        // whole cost and not just the driver's part of it.
        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
        });
        assert!(
            last_message(&app).contains("900 in"),
            "the turn total should include the consult: {}",
            last_message(&app)
        );
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
    fn spilling_narrates_the_move_before_the_tier_is_abandoned() {
        // The reason has to be on screen while the move is still a move. The
        // tier is named by the agent, so the index is resolved from the labels.
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];

        app.handle_agent_event(AgentEvent::Spilling {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated the same output 4 times".to_string(),
        });

        let said = last_message(&app);
        assert!(said.contains("repeated the same output 4 times"), "{said}");
        assert!(said.contains("spilling over to DeepSeek"), "{said}");
        assert_eq!(
            app.abandoning_tier(),
            Some(0),
            "the tier being abandoned should be the one drawn as going"
        );
        assert_eq!(
            app.active_tier, 0,
            "the move has not been committed yet — that is what makes it a beat"
        );
        assert!(
            !app.tier_failed[0],
            "and the tier is not marked spent until it actually is"
        );
    }

    #[test]
    fn the_beat_ends_on_its_own_after_a_few_frames() {
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];
        app.handle_agent_event(AgentEvent::Spilling {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "went quiet for 30s".to_string(),
        });

        app.tick += HANDOFF_TICKS;
        assert_eq!(app.abandoning_tier(), None, "the beat is over");
    }

    #[test]
    fn escalating_after_a_beat_records_the_move_without_a_second_line() {
        // `Spilling` then `Escalated` is one event in two parts: the transcript
        // must not say the same thing twice, and the tier ends up spent.
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];
        let before = app.messages.len();

        app.handle_agent_event(AgentEvent::Spilling {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated the same output 4 times".to_string(),
        });
        app.handle_agent_event(AgentEvent::Escalated {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "repeated the same output 4 times".to_string(),
        });

        assert_eq!(
            app.messages.len(),
            before + 1,
            "one line for one move: {:?}",
            app.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(app.tier_failed[0], "the tier is spent now");
        assert_eq!(app.active_tier, 1, "and the next one is answering");
    }

    #[test]
    fn a_finished_turn_does_not_leave_the_beat_running() {
        // The tick counter only advances while a turn is busy, so a beat left set
        // after the turn would flash forever instead of expiring.
        let (mut app, _commands) = attached_app();
        app.tier_labels = vec!["Local".to_string(), "DeepSeek".to_string()];
        app.handle_agent_event(AgentEvent::Spilling {
            from: "Local".to_string(),
            to: "DeepSeek".to_string(),
            reason: "went quiet for 30s".to_string(),
        });
        assert!(app.abandoning_tier().is_some());

        app.handle_agent_event(AgentEvent::Finished {
            stop_reason: Some("end_turn".to_string()),
        });

        assert_eq!(app.abandoning_tier(), None, "no frozen flash");
    }

    #[test]
    fn the_answer_keys_still_work_with_a_scrolled_preview() {
        let (mut app, mut answer) = long_preview_approval();
        app.handle_key(press(KeyCode::PageDown));

        app.handle_key(press(KeyCode::Char('y')));

        assert_eq!(answer.try_recv().expect("an answer"), Decision::Approve);
        assert!(app.approval.is_none());
    }

    // ---- resuming ---------------------------------------------------------

    fn saved_session() -> SessionFile {
        let mut saved = SessionFile::new("/tmp/example");
        saved.sticky = false;
        saved.on_stuck = Some(OnStuck::Consult);
        saved.mode = Mode::Plan;
        saved
            .cli_sessions
            .insert("grok".to_string(), "sess-1".to_string());
        saved.messages = vec![
            crate::session::ChatMessage::user("make the tests pass"),
            crate::session::ChatMessage::assistant("Looking at it.", Vec::new()),
            crate::session::ChatMessage::assistant(
                "",
                vec![crate::session::ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
            crate::session::ChatMessage::tool_result(
                "call_1",
                "error[E0308]: mismatched types\nmore",
            ),
        ];
        saved
    }

    #[test]
    fn resuming_brings_back_the_policy_the_mode_and_the_tier() {
        let mut app = new_app();
        app.attach(
            tokio::sync::mpsc::unbounded_channel().0,
            Canceller::default(),
            &["Local".to_string(), "Grok".to_string()],
            None,
        );

        app.restore(&saved_session(), 1);

        assert!(!app.sticky, "the session's choice came back");
        assert_eq!(app.on_stuck, Some(OnStuck::Consult));
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(app.active_tier, 1, "it is still on the tier it was using");
    }

    #[test]
    fn resuming_says_that_it_resumed() {
        // A transcript that appears from nowhere is confusing, so the one thing
        // a resume must not be is silent about itself.
        let mut app = new_app();
        app.restore(&saved_session(), 0);

        let last = app.messages.last().expect("a notice");
        assert_eq!(last.role, Role::System);
        assert!(last.text.contains("resumed this session"), "{}", last.text);
    }

    #[test]
    fn a_saved_tier_that_is_no_longer_configured_cannot_point_past_the_rail() {
        // The config may have shrunk since the session was written, and the rail
        // must not be handed an index it cannot draw.
        let mut app = new_app();
        app.attach(
            tokio::sync::mpsc::unbounded_channel().0,
            Canceller::default(),
            &["Local".to_string()],
            None,
        );

        app.restore(&saved_session(), 7);

        assert_eq!(app.active_tier, 0, "clamped to the only tier there is");
    }

    #[test]
    fn restoring_renders_the_conversation_including_what_the_tools_did() {
        let rendered = render_session(&saved_session().messages);
        let text: String = rendered
            .iter()
            .map(|message| format!("{:?} {}\n", message.role, message.text))
            .collect();

        assert!(text.contains("make the tests pass"), "{text}");
        assert!(text.contains("Looking at it."), "{text}");
        assert!(
            text.contains("read_file"),
            "a turn that only called tools still happened: {text}"
        );
        assert!(
            text.contains("error[E0308]"),
            "the raw result is the useful part: {text}"
        );
    }

    #[test]
    fn a_restored_conversation_does_not_carry_a_prompt_it_was_not_run_under() {
        let messages = vec![
            crate::session::ChatMessage::system("You are in PLAN MODE"),
            crate::session::ChatMessage::user("hello"),
        ];
        let rendered = render_session(&messages);

        assert_eq!(rendered.len(), 1, "{rendered:?}");
        assert_eq!(rendered[0].text, "hello");
    }

    #[test]
    fn the_resume_notice_counts_the_messages_and_coarsely_says_when() {
        let mut app = new_app();
        app.restore(&saved_session(), 0);

        let last = app.messages.last().expect("a notice");
        assert!(last.text.contains("4 messages"), "{}", last.text);
        assert!(
            last.text.contains("just now"),
            "written seconds ago: {}",
            last.text
        );
    }

    #[test]
    fn undo_reaches_the_agent_and_leaves_the_chain_alone() {
        // The pre-image lives with the agent, which is what ran the tool, so the
        // interface's whole job here is to pass the command along.
        let (mut app, mut commands) = attached_app();
        let before = (app.active_tier, app.sticky, app.tier_failed.clone());

        type_and_send(&mut app, "/undo");

        assert_eq!(
            commands.try_recv().expect("the command should be sent"),
            Command::Undo
        );
        assert_eq!(
            (app.active_tier, app.sticky, app.tier_failed.clone()),
            before,
            "undo is about the workspace, not the chain"
        );
    }

    #[test]
    fn undo_is_offered_in_the_command_menu() {
        // It is in the catalogue, so the menu and the help overlay pick it up
        // with nothing to keep in step.
        let (mut app, _commands) = attached_app();
        for ch in "/undo".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        assert!(
            app.menu_matches().iter().any(|spec| spec.name == "undo"),
            "{:?}",
            app.menu_matches()
        );
    }

    #[test]
    fn allow_reads_its_argument_into_the_change_it_asks_for() {
        // The interface's whole job here is to read the argument and pass it on:
        // the rules live with the agent, which is what runs a tool.
        let (mut app, mut commands) = attached_app();

        for (typed, expected) in [
            ("/allow", Command::Allow(AllowChange::List)),
            (
                "/allow git status",
                Command::Allow(AllowChange::Add("git status".to_string())),
            ),
            (
                "/allow save git status",
                Command::Allow(AllowChange::Save("git status".to_string())),
            ),
            ("/allow clear", Command::Allow(AllowChange::Clear)),
        ] {
            type_and_send(&mut app, typed);
            assert_eq!(
                commands.try_recv().expect("the command should be sent"),
                expected,
                "for {typed:?}"
            );
        }
    }

    #[test]
    fn a_save_with_nothing_to_save_says_so_rather_than_sending_a_rule() {
        // `/allow save` with no words is a mistake with an obvious fix, so it is
        // explained instead of being read as a rule for the program `save`.
        let (mut app, mut commands) = attached_app();
        let before = app.messages.len();

        type_and_send(&mut app, "/allow save");

        assert!(
            commands.try_recv().is_err(),
            "nothing should have been sent"
        );
        let said: Vec<String> = app.messages[before..]
            .iter()
            .map(|message| message.text.clone())
            .collect();
        assert!(
            said.iter().any(|line| line.contains("usage: /allow save")),
            "{said:?}"
        );
    }

    #[test]
    fn allow_is_offered_in_the_command_menu() {
        let (mut app, _commands) = attached_app();
        for ch in "/allow".chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        assert!(
            app.menu_matches().iter().any(|spec| spec.name == "allow"),
            "{:?}",
            app.menu_matches()
        );
    }
}
