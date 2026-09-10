//! Application state and key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::agent::AgentEvent;
use crate::agent::approval::{ApprovalRequest, Decision};
use crate::agent::first_line;
use crate::config::{Config, Tier};

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
    commands: Option<UnboundedSender<String>>,
    /// Index of the assistant message currently being streamed into.
    streaming: Option<usize>,
    pub approval: Option<PendingApproval>,
    pub busy: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        let messages = vec![Message::system(welcome(&config))];
        Self {
            config,
            messages,
            input: String::new(),
            scroll_back: 0,
            should_quit: false,
            commands: None,
            streaming: None,
            approval: None,
            busy: false,
        }
    }

    /// Connect to a running agent, so prompts have somewhere to go.
    pub fn attach(
        &mut self,
        commands: UnboundedSender<String>,
        tiers_in_order: &str,
        warning: Option<String>,
    ) {
        self.commands = Some(commands);
        self.messages.push(Message::system(format!(
            "tiers, in order: {tiers_in_order}. File writes and shell commands will ask before \
             they run."
        )));
        if let Some(warning) = warning {
            self.messages.push(Message::system(warning));
        }
    }

    pub fn tiers(&self) -> &[Tier] {
        &self.config.tiers
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Terminals differ on whether they report repeats and releases; acting on
        // anything but a press makes one keytap move several lines.
        if key.kind != KeyEventKind::Press {
            return;
        }

        // While the agent is asking, the answer keys belong to the modal.
        if self.approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.answer_approval(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_approval(false);
                }
                _ => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.should_quit = true;
            return;
        }

        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) => self.input.push(ch),
            KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(1),
            KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            _ => {}
        }
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
        self.scroll_back = 0;
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(chunk) => {
                // Consecutive chunks belong to the same message; anything else
                // in between (a tool, a notice) starts a new one.
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
            }
            AgentEvent::ToolFinished { name, ok, summary } => {
                self.streaming = None;
                let mark = if ok { "✓" } else { "✗" };
                self.messages
                    .push(Message::system(format!("{mark} {name}  {summary}")));
            }
            AgentEvent::Denied { tool } => {
                self.streaming = None;
                self.messages
                    .push(Message::system(format!("✗ {tool} — you declined")));
            }
            AgentEvent::Notice(message) => {
                self.streaming = None;
                self.messages.push(Message::system(message));
            }
            AgentEvent::Finished { stop_reason, usage } => {
                self.streaming = None;
                self.busy = false;
                if stop_reason.as_deref() == Some("length") {
                    self.messages.push(Message::system(
                        "the model hit its output limit, so this answer is incomplete",
                    ));
                }
                if let Some(usage) = usage {
                    self.messages.push(Message::system(format!(
                        "tokens: {} in, {} out",
                        usage.prompt_tokens, usage.completion_tokens
                    )));
                }
            }
            AgentEvent::Escalated { from, to, reason } => {
                // What the failing tier streamed is not what the next tier will
                // continue from, so it must not sit in the transcript as though
                // it were an answer.
                self.discard_streaming_message();
                self.messages.push(Message::system(format!(
                    "✗ {from} {reason} — spilling over to {to}"
                )));
            }
            AgentEvent::Exhausted { reason } => {
                self.streaming = None;
                self.busy = false;
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

        if commands.send(text.clone()).is_err() {
            self.messages
                .push(Message::system("the agent is no longer running"));
            return;
        }

        self.input.clear();
        self.streaming = None;
        self.busy = true;
        self.messages.push(Message::user(text));
        self.scroll_back = 0;
    }
}

fn welcome(config: &Config) -> String {
    let mut text = String::from(
        "spill runs one prompt at a time through an ordered list of model tiers, and moves \
         down the list when the active tier stalls or repeats itself.",
    );

    text.push_str(&format!(
        "\n\nworkspace  {}\nfallback   {}",
        config.general.workspace,
        if config.general.sticky_fallback {
            "sticky — stays on the lower tier for the rest of the session"
        } else {
            "per turn — tries the first tier again on the next message"
        }
    ));

    if config.tiers.is_empty() {
        text.push_str(
            "\n\nNo tiers are configured yet. Copy config.example.toml to \
             ~/.config/spill/config.toml and point the first tier at a local server.",
        );
        return text;
    }

    text.push_str("\n\nTiers, in order:");
    for (index, tier) in config.tiers.iter().enumerate() {
        text.push_str(&format!(
            "\n  {}. {} [{}]",
            index + 1,
            tier.display_name(),
            tier.kind
        ));

        let target = match tier.model.as_deref().filter(|model| !model.is_empty()) {
            Some(model) => model.to_string(),
            None => tier
                .base_url
                .clone()
                .or_else(|| tier.preset.clone())
                .unwrap_or_else(|| "no target set".to_string()),
        };
        text.push_str(&format!("  ·  {target}"));

        if let Some(var) = tier.api_key_env.as_deref() {
            text.push_str(&format!("  ·  key from ${var}"));
        }
        if tier.approve_all {
            text.push_str("  ·  runs without confirmation");
        }
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
    fn welcome_summarizes_the_configured_tiers() {
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
            api_key_env = "GROK_TOKEN"
            approve_all = true
            "#,
        )
        .expect("config should parse");

        let text = welcome(&config);
        assert!(text.contains("/work"), "shows the workspace: {text}");
        assert!(
            text.contains("per turn"),
            "shows the fallback policy: {text}"
        );
        assert!(text.contains("Local model"), "names tier 1: {text}");
        assert!(text.contains("qwen3-coder"), "shows the model: {text}");
        assert!(
            text.contains("key from $GROK_TOKEN"),
            "names the key variable: {text}"
        );
        assert!(
            text.contains("runs without confirmation"),
            "warns about approve_all: {text}"
        );
    }

    #[test]
    fn welcome_says_so_when_nothing_is_configured() {
        let text = welcome(&Config::default());
        assert!(text.contains("No tiers are configured yet"), "got: {text}");
    }

    fn attached_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(tx, "Local model", None);
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
            "hello there"
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
            }),
        });

        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.text.contains("120 in, 45 out"))
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
    fn attaching_lists_the_chain_and_names_the_active_tier() {
        let (app, _commands) = attached_app();
        assert!(
            app.messages
                .iter()
                .any(|m| m.text.contains("tiers, in order: Local model"))
        );
    }

    #[test]
    fn attaching_reports_tiers_that_were_left_out() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = new_app();
        app.attach(
            tx,
            "Local model",
            Some("not in the chain yet: grok (cli)".to_string()),
        );

        assert!(
            app.messages
                .iter()
                .any(|m| m.text.contains("not in the chain yet: grok (cli)"))
        );
    }
}
