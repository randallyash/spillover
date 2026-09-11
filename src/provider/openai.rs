//! Any OpenAI-compatible `/chat/completions` endpoint, streamed over SSE.
//!
//! This covers LM Studio, Ollama, llama.cpp, vLLM, OpenRouter and most hosted
//! APIs. The frame parser is deliberately separated from the HTTP call: it is
//! where the subtle behaviour lives (tool calls arrive split across frames, by
//! index, with the name and arguments in different chunks) and it is worth
//! testing directly.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::provider::{ChatRequest, Provider, ProviderError, StreamEvent, TurnSummary, Usage};
use crate::session::{ChatMessage, Role, ToolCall, ToolSpec};

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// How this tier is named when telling the user which one answered.
    display: String,
}

impl OpenAiProvider {
    /// Authentication is optional: local servers usually take none.
    pub fn new(
        display: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("spill/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            base_url: base_url.into(),
            api_key,
            display: display.into(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let messages: Vec<Value> = request.messages.iter().map(message_to_wire).collect();
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
        });

        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(tool_to_wire).collect());
            body["tool_choice"] = json!("auto");
        }

        body
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn describe(&self) -> String {
        format!("{} ({})", self.display, self.base_url)
    }

    async fn stream(
        &self,
        request: ChatRequest,
        events: UnboundedSender<StreamEvent>,
    ) -> Result<TurnSummary, ProviderError> {
        let url = self.endpoint();
        let mut builder = self.client.post(&url).json(&self.build_body(&request));
        if let Some(key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            builder = builder.bearer_auth(key);
        }

        let response = builder
            .send()
            .await
            .map_err(|source| ProviderError::Unreachable {
                target: url.clone(),
                detail: crate::provider::transport_reason(&source),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let mut body = summarise(&body);
            // A bare 404 from an OpenAI-shaped server is almost always a root
            // that is missing the `/v1` segment, which is not obvious from the
            // raw error.
            if status.as_u16() == 404 {
                body.push_str(
                    " — check base_url: OpenAI-compatible roots normally end in /v1, \
                     for example http://localhost:1234/v1",
                );
            }
            return Err(ProviderError::Rejected {
                target: url,
                detail: format!("HTTP {}: {body}", status.as_u16()),
            });
        }

        let mut accumulator = StreamAccumulator::default();
        let mut stream = response.bytes_stream().eventsource();

        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|source| ProviderError::Broken {
                target: url.clone(),
                detail: source.to_string(),
            })?;

            if frame.data.trim() == "[DONE]" {
                break;
            }
            for event in accumulator.apply(&frame.data) {
                // The receiver belongs to the UI; if it is gone the turn is
                // being abandoned, which is not an error here.
                let _ = events.send(event);
            }
        }

        Ok(accumulator.finish())
    }
}

/// Convert a neutral message into the OpenAI wire shape.
fn message_to_wire(message: &ChatMessage) -> Value {
    match message.role {
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
            "content": message.content,
        }),
        Role::Assistant if !message.tool_calls.is_empty() => json!({
            "role": "assistant",
            // Null rather than "" — some servers reject an empty string
            // alongside tool calls.
            "content": if message.content.is_empty() {
                Value::Null
            } else {
                Value::String(message.content.clone())
            },
            "tool_calls": message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments },
                    })
                })
                .collect::<Vec<_>>(),
        }),
        role => json!({ "role": role.as_str(), "content": message.content }),
    }
}

fn tool_to_wire(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        },
    })
}

/// Turn a frame's outcome into events, marking a real-but-silent frame as
/// activity so that "quiet" and "hung" stay distinguishable.
fn finished(mut events: Vec<StreamEvent>, progressed: bool) -> Vec<StreamEvent> {
    if events.is_empty() && progressed {
        events.push(StreamEvent::Activity);
    }
    events
}

/// Shorten a response body so an error message stays readable.
fn summarise(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no response body".to_string()
    } else if trimmed.chars().count() > 300 {
        let head: String = trimmed.chars().take(300).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles a turn out of streaming frames.
///
/// Tool calls are collected rather than emitted as they arrive: they are only
/// safe to act on once the arguments are complete, and they arrive split across
/// many frames keyed by `index`, sometimes with the id or name in only the
/// first one.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    text: String,
    tool_calls: Vec<PartialToolCall>,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    /// Feed one SSE `data:` payload. Returns text fragments worth showing now.
    pub fn apply(&mut self, payload: &str) -> Vec<StreamEvent> {
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Vec::new();
        }

        // Keep-alives, comments and provider-specific oddities are not fatal:
        // dropping an unreadable frame beats aborting a working turn.
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };

        let mut events = Vec::new();
        // Whether this frame showed the model making progress, even when it
        // carried nothing printable. A tool-call turn emits no text at all, so
        // without this a slow tool call would look like a stalled tier.
        let mut progressed = false;

        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            if let Some(read) = crate::provider::read_usage(usage) {
                self.usage = Some(read);
                progressed = true;
            }
        }

        let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
            return finished(events, progressed);
        };
        progressed = true;

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_string());
        }

        let Some(delta) = choice.get("delta") else {
            return finished(events, progressed);
        };

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                self.text.push_str(content);
                events.push(StreamEvent::Text(content.to_string()));
            }
        }

        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                // Indices can skip or arrive out of order; grow to fit.
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(PartialToolCall::default());
                }
                let slot = &mut self.tool_calls[index];

                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if !name.is_empty() {
                            slot.name = name.to_string();
                        }
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(arguments);
                    }
                }
            }
        }

        finished(events, progressed)
    }

    pub fn finish(self) -> TurnSummary {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .enumerate()
            .map(|(index, call)| ToolCall {
                // Some servers omit ids; the protocol requires one, so synthesise
                // a stable placeholder rather than sending an empty string back.
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                arguments: if call.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                },
            })
            .collect();

        TurnSummary {
            text: self.text,
            tool_calls,
            stop_reason: self.stop_reason,
            usage: self.usage,
            // A chat-completions endpoint is stateless: the caller resends the
            // whole conversation every turn, so there is no session to continue.
            session_id: None,
        }
    }
}

/// Ask an OpenAI-compatible endpoint which models it serves, and take the first.
///
/// This is what makes an empty `model` in the config work for a local server:
/// whatever the user has loaded is the right answer, and they should not have to
/// type its id.
pub async fn first_model(base_url: &str, api_key: Option<&str>) -> Result<String, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    list_models(base_url, api_key)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{url} listed no models; set `model` explicitly in your config"))
}

/// Every model an OpenAI-compatible endpoint advertises.
///
/// Used by discovery and by the setup wizard's reachability check, which is why
/// it returns the whole list rather than just the first id.
pub async fn list_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(concat!("spill/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default();

    let mut request = client.get(&url);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }

    let response = request.send().await.map_err(|error| {
        format!(
            "could not reach {url}: {}",
            crate::provider::transport_reason(&error)
        )
    })?;

    let status = response.status();
    if !status.is_success() {
        // A key problem is the usual cause, so say so rather than just the code.
        let hint = match status.as_u16() {
            401 | 403 => " — check that the key is set and valid",
            404 => " — check that base_url ends in /v1",
            _ => "",
        };
        return Err(format!("{url} returned HTTP {}{hint}", status.as_u16()));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("{url} did not return usable JSON: {error}"))?;

    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ChatMessage;

    fn accumulate(payloads: &[&str]) -> StreamAccumulator {
        let mut accumulator = StreamAccumulator::default();
        for payload in payloads {
            accumulator.apply(payload);
        }
        accumulator
    }

    #[test]
    fn accumulates_text_across_frames() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"content":"Hel"},"index":0}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"index":0}]}"#,
        ]);
        assert_eq!(accumulator.finish().text, "Hello");
    }

    #[test]
    fn emits_events_for_arriving_text() {
        let mut accumulator = StreamAccumulator::default();
        let events = accumulator.apply(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Text(text) if text == "hi"));
    }

    #[test]
    fn assembles_a_tool_call_split_across_frames() {
        // The realistic shape: id and name first, arguments a fragment at a time.
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}}]}"#,
        ]);
        let summary = accumulator.finish();
        assert_eq!(summary.tool_calls.len(), 1);
        assert_eq!(summary.tool_calls[0].id, "call_1");
        assert_eq!(summary.tool_calls[0].name, "read_file");
        assert_eq!(summary.tool_calls[0].arguments, r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn keeps_parallel_tool_calls_apart_by_index() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        ]);
        let summary = accumulator.finish();
        let names: Vec<&str> = summary.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "list_dir"]);
    }

    #[test]
    fn tolerates_out_of_order_indices() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"list_dir"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file"}}]}}]}"#,
        ]);
        let summary = accumulator.finish();
        let names: Vec<&str> = summary.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "list_dir"]);
    }

    #[test]
    fn records_finish_reason() {
        let accumulator =
            accumulate(&[r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#]);
        assert_eq!(
            accumulator.finish().stop_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn records_usage_when_the_server_reports_it() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"content":"x"}}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
        ]);
        let usage = accumulator
            .finish()
            .usage
            .expect("usage should be recorded");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[test]
    fn ignores_done_empty_and_unparseable_frames() {
        let accumulator = accumulate(&[
            "[DONE]",
            "",
            "   ",
            "not json at all",
            r#"{"choices":[]}"#,
            r#"{"keep_alive":true}"#,
        ]);
        let summary = accumulator.finish();
        assert!(summary.text.is_empty());
        assert!(summary.tool_calls.is_empty());
    }

    #[test]
    fn synthesises_an_id_when_the_server_omits_one() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        ]);
        let summary = accumulator.finish();
        assert_eq!(summary.tool_calls[0].id, "call_0");
    }

    #[test]
    fn defaults_empty_arguments_to_an_object() {
        let accumulator = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"list_dir"}}]}}]}"#,
        ]);
        assert_eq!(accumulator.finish().tool_calls[0].arguments, "{}");
    }

    #[test]
    fn drops_a_tool_call_that_never_got_a_name() {
        let accumulator =
            accumulate(&[r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c"}]}}]}"#]);
        assert!(accumulator.finish().tool_calls.is_empty());
    }

    #[test]
    fn builds_openai_shaped_messages() {
        let user = message_to_wire(&ChatMessage::user("hello"));
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"], "hello");

        let tool = message_to_wire(&ChatMessage::tool_result("call_1", "contents"));
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");

        let assistant = message_to_wire(&ChatMessage::assistant(
            "",
            vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"a"}"#.to_string(),
            }],
        ));
        assert_eq!(assistant["role"], "assistant");
        assert!(
            assistant["content"].is_null(),
            "empty content must serialise as null next to tool_calls"
        );
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn declares_tools_and_asks_the_model_to_choose() {
        let provider = OpenAiProvider::new("Local", "http://localhost:1234/v1", None);
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![ToolSpec {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                parameters: json!({"type": "object"}),
            }],
        };
        let body = provider.build_body(&request);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn omits_tools_when_none_are_declared() {
        let provider = OpenAiProvider::new("Local", "http://localhost:1234/v1", None);
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
        };
        assert!(provider.build_body(&request).get("tools").is_none());
    }

    #[test]
    fn strips_a_trailing_slash_from_the_endpoint() {
        let provider = OpenAiProvider::new("Local", "http://localhost:1234/v1/", None);
        assert_eq!(
            provider.endpoint(),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    fn sse(payloads: &[&str]) -> String {
        let mut body = String::new();
        for payload in payloads {
            body.push_str("data: ");
            body.push_str(payload);
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    #[tokio::test]
    async fn streams_a_turn_from_a_real_http_server() {
        let mut server = mockito::Server::new_async().await;
        let body = sse(&[
            r#"{"choices":[{"delta":{"content":"Hel"},"index":0}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"index":0}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#,
        ]);
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;

        let provider = OpenAiProvider::new("Local", format!("{}/v1", server.url()), None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let summary = provider
            .stream(request_with_prompt("hi"), tx)
            .await
            .expect("stream should succeed");

        mock.assert_async().await;
        assert_eq!(summary.text, "Hello");
        assert_eq!(summary.stop_reason.as_deref(), Some("stop"));

        let mut streamed = String::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::Text(text) = event {
                streamed.push_str(&text);
            }
        }
        assert_eq!(streamed, "Hello");
    }

    #[tokio::test]
    async fn sends_the_bearer_token_when_a_key_is_configured() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer sk-secret")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse(&[r#"{"choices":[{"delta":{"content":"ok"}}]}"#]))
            .create_async()
            .await;

        let provider = OpenAiProvider::new(
            "Hosted",
            format!("{}/v1", server.url()),
            Some("sk-secret".to_string()),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        provider
            .stream(request_with_prompt("hi"), tx)
            .await
            .expect("stream should succeed with the right header");

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn maps_a_server_error_to_a_status_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(500)
            .with_body("model not loaded")
            .create_async()
            .await;

        let provider = OpenAiProvider::new("Local", format!("{}/v1", server.url()), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let error = provider
            .stream(request_with_prompt("hi"), tx)
            .await
            .expect_err("a 500 must fail the turn");

        match error {
            ProviderError::Rejected { detail, .. } => {
                assert!(detail.contains("HTTP 500"), "got: {detail}");
                assert!(detail.contains("model not loaded"), "got: {detail}");
            }
            other => panic!("expected a rejected request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_404_hints_at_a_missing_v1_segment() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/chat/completions")
            .with_status(404)
            .create_async()
            .await;

        // Note: deliberately built without the /v1 segment.
        let provider = OpenAiProvider::new("Local", server.url(), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let error = provider
            .stream(request_with_prompt("hi"), tx)
            .await
            .expect_err("a 404 must fail the turn");

        assert!(error.to_string().contains("/v1"), "got: {error}");
    }

    #[tokio::test]
    async fn reports_an_unreachable_server_as_a_transport_error() {
        // Port 1 is reserved and nothing listens there.
        let provider = OpenAiProvider::new("Dead", "http://127.0.0.1:1/v1", None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let error = provider
            .stream(request_with_prompt("hi"), tx)
            .await
            .expect_err("a dead endpoint must fail the turn");
        assert!(
            matches!(error, ProviderError::Unreachable { .. }),
            "got: {error:?}"
        );
    }

    #[tokio::test]
    async fn discovers_the_first_advertised_model() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"qwen3-coder-30b"},{"id":"other"}]}"#)
            .create_async()
            .await;

        let model = first_model(&format!("{}/v1", server.url()), None)
            .await
            .expect("discovery should succeed");
        assert_eq!(model, "qwen3-coder-30b");
    }

    #[tokio::test]
    async fn discovery_sends_the_key_when_there_is_one() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .match_header("authorization", "Bearer sk-secret")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"hosted-model"}]}"#)
            .create_async()
            .await;

        first_model(&format!("{}/v1", server.url()), Some("sk-secret"))
            .await
            .expect("discovery should succeed");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn discovery_explains_itself_when_no_models_are_advertised() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .create_async()
            .await;

        let error = first_model(&format!("{}/v1", server.url()), None)
            .await
            .expect_err("an empty model list must fail");
        assert!(error.contains("set `model`"), "got: {error}");
    }

    #[tokio::test]
    async fn discovery_reports_an_unreachable_endpoint() {
        let error = first_model("http://127.0.0.1:1/v1", None)
            .await
            .expect_err("a dead endpoint must fail");
        assert!(error.contains("could not reach"), "got: {error}");
    }

    fn request_with_prompt(text: &str) -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage::user(text)],
            tools: Vec::new(),
        }
    }
}
