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
///
/// Arguments are kept as a string rather than parsed so a malformed or empty
/// argument blob is reported to the model as a tool error instead of being
/// silently dropped here.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
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

    pub fn push(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Drop everything after `len` messages.
    ///
    /// Used to discard a failed attempt before retrying the same turn on
    /// another tier: the next model must not inherit a half-finished answer
    /// from a model that was looping.
    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
    }
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
}
