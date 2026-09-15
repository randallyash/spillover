//! The provider-neutral conversation history.
//!
//! These types are the wire-independent representation of a conversation. Each
//! provider maps them onto whatever shape its own API wants, so a fallback tier
//! can take over mid-conversation without translation loss.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A tool the model asked to call, with its arguments still as raw JSON text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A declared tool, in the shape providers expect when offering them.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    // Omitted when empty so a plain exchange is stored as two short lines
    // rather than carrying two structural nulls through the whole transcript.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool result, correlated back to the call that produced it.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<ChatMessage>,
}

impl Session {
    pub fn with_system_prompt(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![ChatMessage::system(prompt)],
        }
    }

    /// Rebuild a session from a previous run: a fresh system prompt, then the
    /// conversation that was saved under the old one.
    pub fn restore(prompt: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        let mut session = Self::with_system_prompt(prompt);
        // Anything that claims to be a system message is dropped: there is
        // exactly one, it is the one just built, and a second would be an
        // instruction from nowhere.
        session
            .messages
            .extend(messages.into_iter().filter(|m| m.role != Role::System));
        session
    }

    pub fn push(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// The conversation, without the system prompt.
    pub fn conversation(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .filter(|message| message.role != Role::System)
            .cloned()
            .collect()
    }

    /// Drop everything after `len` messages.
    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
    }

    /// Start the conversation over, keeping the system prompt.
    pub fn reset(&mut self) {
        self.messages.truncate(1);
    }

    /// Swap the system prompt for a new one, in place.
    pub fn replace_system_prompt(&mut self, prompt: impl Into<String>) {
        let prompt = ChatMessage::system(prompt);
        match self.messages.first_mut() {
            Some(first) if first.role == Role::System => *first = prompt,
            _ => self.messages.insert(0, prompt),
        }
    }

    /// Replace the older half of the conversation with a short ledger of what
    /// happened in it.
    pub fn compact(&mut self, keep_turns: usize) -> Compaction {
        let before_messages = self.messages.len();
        let chars_before: usize = self.messages.iter().map(|m| m.content.len()).sum();

        // Where each user turn begins. Cutting at one of these is what keeps a
        // tool call and its result on the same side of the line, which
        // providers require.
        let starts: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::User)
            .map(|(index, _)| index)
            .collect();

        // Nothing to gain: every turn is inside the window already.
        if starts.len() <= keep_turns + 1 {
            return Compaction {
                before_messages,
                after_messages: before_messages,
                dropped_turns: 0,
                ledger: String::new(),
                chars_before,
                chars_after: chars_before,
            };
        }

        let cut = starts[starts.len() - keep_turns];
        let dropped = &self.messages[1..cut];
        let ledger = ledger_for(dropped);

        let mut rebuilt: Vec<ChatMessage> = Vec::with_capacity(self.messages.len());
        rebuilt.push(self.messages[0].clone());
        if !ledger.is_empty() {
            rebuilt.push(ChatMessage::system(ledger.clone()));
        }
        rebuilt.extend(self.messages[cut..].iter().cloned());

        let after_chars: usize = rebuilt.iter().map(|m| m.content.len()).sum();

        // Refuse to make things worse. A run of very short turns can add up to
        // less than the ledger that would replace them, and a "compaction" that
        // grows the thing it is compacting would be a bug that costs money.
        if after_chars >= chars_before {
            return Compaction {
                before_messages,
                after_messages: before_messages,
                dropped_turns: 0,
                ledger: String::new(),
                chars_before,
                chars_after: chars_before,
            };
        }

        self.messages = rebuilt;

        Compaction {
            before_messages,
            after_messages: self.messages.len(),
            dropped_turns: cut.saturating_sub(1),
            ledger,
            // Reported so the saving can be stated in the transcript.
            chars_before,
            chars_after: after_chars,
        }
    }
}

/// What a compaction did, for telling the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub before_messages: usize,
    pub after_messages: usize,
    /// How many user turns were folded into the ledger.
    pub dropped_turns: usize,
    /// The ledger itself, empty when nothing was dropped.
    pub ledger: String,
    pub chars_before: usize,
    pub chars_after: usize,
}

impl Compaction {
    /// Whether this call changed anything.
    pub fn happened(&self) -> bool {
        self.dropped_turns > 0
    }

    /// A one-line account of the saving, for the transcript.
    pub fn summary(&self) -> String {
        if !self.happened() {
            return "nothing to compact — this session is already short".to_string();
        }
        format!(
            "compacted {} earlier turn{} (~{} → ~{} characters of history)",
            self.dropped_turns,
            if self.dropped_turns == 1 { "" } else { "s" },
            self.chars_before,
            self.chars_after
        )
    }
}

/// A short record of what happened in the messages being dropped.
fn ledger_for(dropped: &[ChatMessage]) -> String {
    const MAX_LINE: usize = 100;
    const MAX_ENTRIES: usize = 40;

    let mut entries: Vec<String> = Vec::new();

    for message in dropped {
        match message.role {
            Role::User => {
                entries.push(format!(
                    "- asked: {}",
                    clip(&first_line(&message.content), MAX_LINE)
                ));
            }
            Role::Assistant => {
                if message.content.trim().is_empty() {
                    continue;
                }
                entries.push(format!(
                    "- answered: {}",
                    clip(&first_line(&message.content), MAX_LINE)
                ));
            }
            Role::Tool => {
                entries.push(format!(
                    "- tool result: {}",
                    clip(&first_line(&message.content), MAX_LINE)
                ));
            }
            Role::System => {}
        }

        if entries.len() >= MAX_ENTRIES {
            entries.push("- …earlier turns omitted".to_string());
            break;
        }
    }

    if entries.is_empty() {
        return String::new();
    }

    let mut out = String::from("Earlier in this session, before the transcript was compacted:\n");
    out.push_str(&entries.join("\n"));
    out.push_str(
        "\n\nAny tool listed above has already run, and its effect is real. Do not repeat it.",
    );
    out
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Cut to a character budget, counting characters rather than bytes so a
/// multi-byte glyph is never split.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_is_empty() {
        let session = Session::default();
        assert!(session.messages().is_empty());
        assert_eq!(session.messages().len(), 0);
    }

    #[test]
    fn a_system_prompt_becomes_the_first_message() {
        let session = Session::with_system_prompt("be brief");
        assert_eq!(session.messages().len(), 1);
        assert_eq!(session.messages()[0].role, Role::System);
        assert_eq!(session.messages()[0].content, "be brief");
    }

    #[test]
    fn messages_accumulate_in_order() {
        let mut session = Session::default();
        session.push(ChatMessage::user("first"));
        session.push(ChatMessage::assistant("second", Vec::new()));
        let roles: Vec<Role> = session.messages().iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
    }

    #[test]
    fn an_assistant_message_can_carry_tool_calls() {
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        };
        let message = ChatMessage::assistant("", vec![call.clone()]);
        assert_eq!(message.tool_calls, vec![call]);
        assert!(message.content.is_empty());
    }

    #[test]
    fn a_tool_result_carries_the_matching_call_id() {
        let message = ChatMessage::tool_result("call_1", "contents");
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn role_names_match_the_wire_names() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::Tool.as_str(), "tool");
    }

    #[test]
    fn truncate_drops_everything_after_a_checkpoint() {
        let mut session = Session::with_system_prompt("be brief");
        session.push(ChatMessage::user("ask"));
        let checkpoint = session.messages().len();

        // A tier's failed attempt.
        session.push(ChatMessage::assistant("half an ans", Vec::new()));
        session.push(ChatMessage::tool_result("call_1", "a result"));

        session.truncate(checkpoint);

        assert_eq!(session.messages().len(), checkpoint);
        assert_eq!(
            session.messages().last().map(|m| m.content.as_str()),
            Some("ask"),
            "the user's turn should survive the rollback"
        );
    }

    #[test]
    fn truncating_past_the_end_is_harmless() {
        let mut session = Session::with_system_prompt("be brief");
        session.truncate(99);
        assert_eq!(session.messages().len(), 1);
    }

    #[test]
    fn resetting_leaves_only_the_system_prompt() {
        let mut session = Session::with_system_prompt("be brief");
        session.push(ChatMessage::user("ask"));
        session.push(ChatMessage::assistant("answer", Vec::new()));

        session.reset();

        assert_eq!(session.messages().len(), 1);
        assert_eq!(session.messages()[0].role, Role::System);
    }

    // ---- compaction -------------------------------------------------------

    /// A session of `turns` exchanges with realistically-sized messages.
    fn conversation(turns: usize) -> Session {
        let mut session = Session::with_system_prompt("be brief");
        for turn in 0..turns {
            session.push(ChatMessage::user(format!(
                "question {turn}: {}",
                "could you look into this ".repeat(8)
            )));
            session.push(ChatMessage::assistant(
                "",
                vec![ToolCall {
                    id: format!("call_{turn}"),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ));
            session.push(ChatMessage::tool_result(
                format!("call_{turn}"),
                format!(
                    "contents of file {turn}:\n{}",
                    "a line of the file\n".repeat(20)
                ),
            ));
            session.push(ChatMessage::assistant(
                format!("answer {turn}: {}", "here is the explanation ".repeat(10)),
                Vec::new(),
            ));
        }
        session
    }

    #[test]
    fn a_short_session_is_left_alone() {
        let mut session = conversation(2);
        let before = session.messages().len();

        let report = session.compact(4);

        assert!(!report.happened(), "{report:?}");
        assert_eq!(session.messages().len(), before);
        assert!(
            report.summary().contains("nothing to compact"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn compaction_keeps_the_system_prompt_and_the_recent_turns() {
        let mut session = conversation(6);
        let report = session.compact(2);

        assert!(report.happened(), "{report:?}");
        assert!(
            session.messages().len() < report.before_messages,
            "it should have shrunk: {report:?}"
        );
        assert_eq!(
            session.messages()[0].role,
            Role::System,
            "the system prompt must stay first"
        );
        assert_eq!(session.messages()[0].content, "be brief");

        // The last two turns are intact, verbatim.
        let text: String = session
            .messages()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("question 5"), "{text}");
        assert!(text.contains("answer 5"), "{text}");
        assert!(text.contains("question 4"), "{text}");
    }

    #[test]
    fn compaction_records_the_tools_that_ran_because_their_effects_are_real() {
        // The promise the README makes: a discarded conversation does not undo
        // a file that was written.
        let mut session = Session::with_system_prompt("be brief");
        for turn in 0..6 {
            session.push(ChatMessage::user(format!(
                "question {turn}: {}",
                "please do the thing ".repeat(8)
            )));
            session.push(ChatMessage::tool_result(
                format!("call_{turn}"),
                format!("wrote file {turn}.txt:\n{}", "bytes written\n".repeat(20)),
            ));
            session.push(ChatMessage::assistant(
                format!("done {turn}: {}", "that is finished now ".repeat(8)),
                Vec::new(),
            ));
        }

        let report = session.compact(2);
        let ledger = &report.ledger;

        assert!(ledger.contains("wrote file 0.txt"), "{ledger}");
        assert!(
            ledger.contains("already run"),
            "the ledger must say the effects stand: {ledger}"
        );
    }

    #[test]
    fn compaction_declines_when_it_would_grow_the_conversation() {
        // A run of very short turns can be smaller than the ledger replacing
        // them. Shrinking the history is the entire point, so a compaction that
        // made it bigger would be a bug that costs money.
        let mut session = Session::with_system_prompt("be brief");
        for turn in 0..12 {
            session.push(ChatMessage::user(format!("q{turn}")));
            session.push(ChatMessage::assistant(format!("a{turn}"), Vec::new()));
        }
        let before = session.messages().len();
        let before_chars: usize = session.messages().iter().map(|m| m.content.len()).sum();

        let report = session.compact(1);

        assert!(!report.happened(), "it should have declined: {report:?}");
        assert_eq!(session.messages().len(), before, "nothing may be touched");
        let after_chars: usize = session.messages().iter().map(|m| m.content.len()).sum();
        assert_eq!(after_chars, before_chars);
        assert!(
            report.summary().contains("nothing to compact"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn compaction_never_splits_a_tool_call_from_its_result() {
        // Providers reject a tool result whose call is missing, so the cut has
        // to land on a turn boundary.
        let mut session = conversation(8);
        session.compact(3);

        let mut open: Vec<String> = Vec::new();
        for message in session.messages() {
            for call in &message.tool_calls {
                open.push(call.id.clone());
            }
            if message.role == Role::Tool {
                let id = message.tool_call_id.clone().unwrap_or_default();
                // Either the call is still present, or both were dropped.
                for orphan in open.iter().filter(|open_id| **open_id == id) {
                    assert_eq!(*orphan, id);
                }
            }
        }

        let calls: Vec<&str> = session
            .messages()
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
            .collect();
        let results: Vec<&str> = session
            .messages()
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            calls, results,
            "every surviving tool result must still have its call"
        );
    }

    #[test]
    fn compaction_reports_what_it_saved() {
        let mut session = conversation(10);
        let report = session.compact(2);

        assert!(report.dropped_turns > 0, "{report:?}");
        assert!(
            report.chars_after < report.chars_before,
            "the report should show a real saving: {report:?}"
        );
        let summary = report.summary();
        assert!(summary.contains("compacted"), "{summary}");
        assert!(summary.contains("turn"), "{summary}");
    }

    #[test]
    fn compacting_twice_is_stable() {
        let mut session = conversation(10);
        session.compact(2);
        let after_first = session.messages().len();

        let second = session.compact(2);

        assert!(
            !second.happened(),
            "a compacted session is already inside the window: {second:?}"
        );
        assert_eq!(session.messages().len(), after_first);
    }

    #[test]
    fn the_ledger_is_bounded() {
        let mut session = Session::with_system_prompt("be brief");
        for turn in 0..500 {
            session.push(ChatMessage::user(format!("question {turn}")));
            session.push(ChatMessage::assistant(format!("answer {turn}"), Vec::new()));
        }

        let report = session.compact(1);

        assert!(
            report.ledger.len() < 8_000,
            "the ledger itself must not become the problem: {} bytes",
            report.ledger.len()
        );
        assert!(report.ledger.contains("omitted"), "it should say so");
    }

    #[test]
    fn compaction_with_nothing_to_drop_before_the_window_is_a_no_op() {
        let mut session = Session::with_system_prompt("be brief");
        session.push(ChatMessage::user("only turn"));
        session.push(ChatMessage::assistant("only answer", Vec::new()));

        let report = session.compact(0);

        // One user turn, and keeping zero would still have to keep the turn in
        // progress, so nothing is dropped and nothing is lost.
        assert_eq!(report.after_messages, session.messages().len());
        let text: String = session
            .messages()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert!(text.contains("only answer"), "{text}");
    }

    #[test]
    fn a_clipped_ledger_line_keeps_whole_characters() {
        let line = "日本語".repeat(60);
        let clipped = clip(&line, 5);
        assert_eq!(clipped.chars().count(), 6, "five plus the ellipsis");
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn a_conversation_survives_being_written_to_a_file() {
        // These types are the on-disk format now, so a rename here silently
        // changes the file every saved session is read from.
        let messages = vec![
            ChatMessage::user("read it"),
            ChatMessage::assistant(
                "reading",
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.rs"}"#.to_string(),
                }],
            ),
            ChatMessage::tool_result("call_1", "fn main() {}"),
        ];

        let text = serde_json::to_string(&messages).expect("serialize");
        let back: Vec<ChatMessage> = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, messages);
    }

    #[test]
    fn an_empty_tool_list_is_left_out_of_the_file_entirely() {
        // Most messages carry no tools, and two structural nulls on each of them
        // would be most of the file.
        let text = serde_json::to_string(&ChatMessage::user("hello")).expect("serialize");
        assert_eq!(text, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn restoring_rebuilds_the_prompt_and_keeps_the_conversation() {
        let messages = vec![
            ChatMessage::user("first"),
            ChatMessage::assistant("answered", Vec::new()),
        ];
        let session = Session::restore("be brief", messages.clone());

        assert_eq!(session.messages().len(), 3);
        assert_eq!(session.messages()[0].role, Role::System);
        assert_eq!(session.messages()[0].content, "be brief");
        assert_eq!(&session.messages()[1..], messages.as_slice());
    }

    #[test]
    fn restoring_drops_any_system_message_from_the_file() {
        // A saved conversation is read back without the prompt that produced it,
        // so anything claiming to be one is a stray instruction from nowhere.
        let messages = vec![
            ChatMessage::system("You are in PLAN MODE"),
            ChatMessage::user("first"),
        ];
        let session = Session::restore("be brief", messages);

        assert_eq!(session.messages().len(), 2);
        assert_eq!(session.messages()[0].content, "be brief");
        assert!(
            !session
                .messages()
                .iter()
                .any(|m| m.content.contains("PLAN MODE")),
            "the stale prompt must not survive"
        );
    }

    #[test]
    fn the_conversation_is_what_gets_saved_and_excludes_the_prompt() {
        let mut session = Session::with_system_prompt("be brief");
        session.push(ChatMessage::user("first"));

        let conversation = session.conversation();
        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation[0].content, "first");
        assert!(
            !conversation.iter().any(|m| m.role == Role::System),
            "the prompt is rebuilt on load rather than stored"
        );
    }
}
