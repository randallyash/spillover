//! The provider boundary: everything a tier has to implement to answer a turn.

pub mod cli;
pub mod dialect;
pub mod openai;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

use crate::session::{ChatMessage, ToolCall, ToolSpec};

/// Progress emitted while a turn is still being generated.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A fragment of assistant text, to be appended to the transcript.
    Text(String),
    /// The tier showed real progress without anything printable — a tool call
    /// being built up, say. This is what keeps a long tool call from looking
    /// like a stalled tier.
    Activity,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// What a completed turn produced.
#[derive(Debug, Clone, Default)]
pub struct TurnSummary {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
}

/// Why a tier could not answer.
///
/// Deliberately transport-neutral: a tier may be an HTTP endpoint or a child
/// process, and the caller deciding whether to spill over should not have to
/// care which. Every message is written to be shown to a person.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("could not reach {target}: {detail}")]
    Unreachable { target: String, detail: String },

    #[error("{target} refused the request: {detail}")]
    Rejected { target: String, detail: String },

    #[error("lost the response from {target}: {detail}")]
    Broken { target: String, detail: String },
}

/// A short, human reason for a failed request, without repeating the target.
///
/// `reqwest` puts the whole URL in its own message, and every caller here
/// already names the target, so the raw text reads
/// "could not reach http://… : error sending request for url (http://…)".
pub fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "could not connect".to_string()
    } else if error.is_timeout() {
        "timed out".to_string()
    } else if error.is_body() || error.is_decode() {
        "the response could not be read".to_string()
    } else {
        // Nothing recognised: keep reqwest's own words, minus the URL it
        // repeats, so the text stays about the cause.
        let text = error.to_string();
        match text.find(" for url (") {
            Some(cut) => text[..cut].to_string(),
            None => text,
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable identity, used to tell the user which tier answered.
    fn describe(&self) -> String;

    /// Run one turn, forwarding text as it arrives.
    async fn stream(
        &self,
        request: ChatRequest,
        events: UnboundedSender<StreamEvent>,
    ) -> Result<TurnSummary, ProviderError>;
}

/// Read token counts from either the Chat Completions or the Messages naming.
///
/// Tiers report usage in whichever shape their backend uses, and a missing or
/// differently-named field is not worth failing a turn over.
pub fn read_usage(value: &serde_json::Value) -> Option<Usage> {
    let read = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| value.get(*name).and_then(serde_json::Value::as_u64))
    };

    let prompt = read(&["prompt_tokens", "input_tokens", "inputTokens"]);
    let completion = read(&["completion_tokens", "output_tokens", "outputTokens"]);

    match (prompt, completion) {
        (None, None) => None,
        (prompt, completion) => Some(Usage {
            prompt_tokens: prompt.unwrap_or(0),
            completion_tokens: completion.unwrap_or(0),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_chat_completions_usage() {
        let usage =
            read_usage(&json!({"prompt_tokens": 11, "completion_tokens": 7})).expect("usage");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[test]
    fn reads_messages_usage() {
        let usage = read_usage(&json!({"input_tokens": 812, "output_tokens": 45})).expect("usage");
        assert_eq!(usage.prompt_tokens, 812);
        assert_eq!(usage.completion_tokens, 45);
    }

    #[test]
    fn reads_camel_case_usage_as_well() {
        // Command Code reports usage this way.
        let usage = read_usage(&json!({"inputTokens": 15754, "outputTokens": 3})).expect("usage");
        assert_eq!(usage.prompt_tokens, 15754);
        assert_eq!(usage.completion_tokens, 3);
    }

    #[test]
    fn a_partial_usage_object_still_reads() {
        let usage = read_usage(&json!({"output_tokens": 5})).expect("usage");
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn an_unrecognised_usage_object_is_ignored_rather_than_faked() {
        assert!(read_usage(&json!({"something_else": 3})).is_none());
        assert!(read_usage(&json!({})).is_none());
    }

    #[test]
    fn error_messages_name_the_target_and_the_reason() {
        let error = ProviderError::Unreachable {
            target: "grok".to_string(),
            detail: "not found on PATH".to_string(),
        };
        assert_eq!(error.to_string(), "could not reach grok: not found on PATH");
    }

    #[tokio::test]
    async fn a_connection_failure_is_described_without_repeating_the_url() {
        // Nothing listens on port 9, so this fails at the connect step.
        let client = reqwest::Client::new();
        let error = client
            .get("http://127.0.0.1:9/v1/models")
            .send()
            .await
            .expect_err("a closed port must fail");

        let reason = transport_reason(&error);
        assert_eq!(reason, "could not connect");
        assert!(
            !reason.contains("http"),
            "the caller already names the target: {reason}"
        );
    }

    #[test]
    fn an_unrecognised_failure_keeps_its_words_but_drops_the_url() {
        // Constructing a real error of this kind needs a live connection, so the
        // trimming rule is checked directly here.
        let text = "error sending request for url (http://example.invalid/v1)";
        let trimmed = match text.find(" for url (") {
            Some(cut) => &text[..cut],
            None => text,
        };
        assert_eq!(trimmed, "error sending request");
    }
}
