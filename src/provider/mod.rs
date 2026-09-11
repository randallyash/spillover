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
    /// What the tier says this request has cost so far.
    ///
    /// Emitted as soon as a frame carries it, rather than only being read off
    /// the finished response, because a request that is *abandoned* was still
    /// billed. A tier that loops and is thrown away has already been charged for
    /// what it generated, and a stream killed mid-flight never reaches the frame
    /// that would have reported its total at the end — so whatever arrived
    /// before the kill is worth keeping.
    ///
    /// These restate the same figure rather than accumulating: a Command Code
    /// run reports one usage on `model_request_end`, again on `turn_end`, and
    /// again on its result line. A reader therefore takes the latest per
    /// request and sums across requests.
    Usage(Usage),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens the provider served from its prompt cache. Every provider
    /// with a cache bills these at a discount, so they are the difference
    /// between a cheap turn and an expensive one rather than a curiosity.
    ///
    /// Whether these are *part of* `prompt_tokens` or additional to it differs
    /// by provider — OpenAI counts them in, Anthropic does not — so the two are
    /// reported as they arrived and never added together here.
    pub cache_read_tokens: u64,
    /// Prompt tokens this request wrote into the cache, billed at a premium by
    /// the providers that charge for the write.
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Add another request's tokens to this total.
    ///
    /// A total here is a sum over *requests*, not over turns: a turn can make
    /// several requests — one per tool call — and every one of them is billed,
    /// including the requests behind an answer that was thrown away. Counting
    /// only the last one understates a turn in proportion to how much work it
    /// did, which is the worst possible direction for the error.
    ///
    /// Saturating, like every other tally in the program, so a provider
    /// reporting nonsense cannot panic a turn.
    pub fn absorb(&mut self, other: &Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

/// Fold one request's reported tokens into a running total.
///
/// Free rather than a method because a total that has not been reported yet is
/// absent, which is a fact about the total rather than about any one request.
pub fn accumulate(total: &mut Option<Usage>, more: Option<Usage>) {
    if let Some(more) = more {
        total.get_or_insert_with(Usage::default).absorb(&more);
    }
}

/// What a completed turn produced.
#[derive(Debug, Clone, Default)]
pub struct TurnSummary {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    /// The session the CLI ran under, when it names one in its own output.
    ///
    /// A CLI that mints its own session id (Command Code) reports it here so the
    /// next turn can continue that session instead of resending the whole
    /// conversation.
    pub session_id: Option<String>,
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

    /// Forget any continued session, because this tier's conversation was
    /// discarded and it must not resume one that holds that output.
    ///
    /// A stateless provider has nothing to forget, which is why this does
    /// nothing by default.
    fn forget_session(&self) {}
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

    // OpenAI puts its cache figure one level down, under the prompt details.
    let nested = |path: [&str; 2]| {
        value
            .get(path[0])
            .and_then(|inner| inner.get(path[1]))
            .and_then(serde_json::Value::as_u64)
    };

    let cache_read = read(&[
        "cacheReadTokens",
        "cache_read_tokens",
        "cache_read_input_tokens",
        "cached_tokens",
        "prompt_cache_hit_tokens",
    ])
    .or_else(|| nested(["prompt_tokens_details", "cached_tokens"]))
    .or_else(|| nested(["input_tokens_details", "cached_tokens"]));

    let cache_write = read(&[
        "cacheWriteTokens",
        "cache_write_tokens",
        "cache_creation_input_tokens",
        "prompt_cache_miss_tokens",
    ]);

    match (prompt, completion, cache_read, cache_write) {
        // Nothing recognised. A cache-only object is still recognised: it says
        // something true about the turn even without a token count.
        (None, None, None, None) => None,
        (prompt, completion, cache_read, cache_write) => Some(Usage {
            prompt_tokens: prompt.unwrap_or(0),
            completion_tokens: completion.unwrap_or(0),
            cache_read_tokens: cache_read.unwrap_or(0),
            cache_write_tokens: cache_write.unwrap_or(0),
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
    fn reads_command_codes_cache_counts() {
        // The exact shape of a real `cmd -p --output-format json` result frame.
        let usage = read_usage(&json!({
            "inputTokens": 15360,
            "outputTokens": 2,
            "cacheReadTokens": 7424,
            "cacheWriteTokens": 0,
        }))
        .expect("usage");
        assert_eq!(usage.prompt_tokens, 15360);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.cache_read_tokens, 7424);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn reads_anthropic_style_cache_counts() {
        let usage = read_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 50,
        }))
        .expect("usage");
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.cache_write_tokens, 50);
    }

    #[test]
    fn reads_openais_nested_cache_count() {
        let usage = read_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 768},
        }))
        .expect("usage");
        assert_eq!(usage.prompt_tokens, 1000);
        assert_eq!(usage.cache_read_tokens, 768);
    }

    #[test]
    fn a_cache_only_usage_object_is_not_discarded() {
        // No token counts, but it still says something true about the turn.
        let usage = read_usage(&json!({"cacheReadTokens": 4096})).expect("usage");
        assert_eq!(usage.cache_read_tokens, 4096);
        assert_eq!(usage.prompt_tokens, 0);
    }

    #[test]
    fn a_provider_with_no_cache_reports_zero_rather_than_nothing() {
        let usage =
            read_usage(&json!({"prompt_tokens": 10, "completion_tokens": 1})).expect("usage");
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
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
