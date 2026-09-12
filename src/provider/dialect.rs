//! Turning an agent CLI's stdout into the events the app understands.
//!
//! Each parser is written against that CLI's documented output, with the exact
//! frame shapes checked rather than assumed. A line that is not recognised is
//! treated as progress and never as answer text, so a format change degrades to
//! "this produced nothing I could read" instead of pasting raw JSON at the user.

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{StreamEvent, TurnSummary, Usage, read_usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dialect {
    /// Plain text on stdout. This is what most agent CLIs print by default,
    /// which is why an unlisted CLI still works as a tier.
    #[default]
    Plain,
    /// Command Code, `--output-format json`.
    CommandCode,
    /// Grok Build, `--output-format streaming-json`.
    Grok,
}

pub trait Parser: Send {
    /// Consume one line of stdout, returning anything worth forwarding.
    fn line(&mut self, line: &str) -> Vec<StreamEvent>;

    /// The finished turn, or what the CLI said went wrong.
    fn finish(self: Box<Self>) -> Result<TurnSummary, String>;
}

pub fn parser_for(dialect: Dialect) -> Box<dyn Parser> {
    match dialect {
        Dialect::Plain => Box::new(PlainParser::default()),
        Dialect::CommandCode => Box::new(CommandCodeParser::default()),
        Dialect::Grok => Box::new(GrokParser::default()),
    }
}

#[derive(Default)]
pub struct PlainParser {
    text: String,
}

impl Parser for PlainParser {
    fn line(&mut self, line: &str) -> Vec<StreamEvent> {
        self.text.push_str(line);
        self.text.push('\n');
        vec![StreamEvent::Text(format!("{line}\n"))]
    }

    fn finish(self: Box<Self>) -> Result<TurnSummary, String> {
        Ok(TurnSummary {
            text: self.text.trim_end().to_string(),
            stop_reason: Some("end_turn".to_string()),
            ..TurnSummary::default()
        })
    }
}

/// Command Code prints `{"type":"event",…}` frames as it works and one
/// `{"type":"result",…}` line carrying `finalText` at the end.
///
/// It mints its own session id and names it in the `run_start` event and again
/// on the result line, so it is picked up here for the next turn to resume.
#[derive(Default)]
pub struct CommandCodeParser {
    text: String,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    session_id: Option<String>,
    error: Option<String>,
}

impl Parser for CommandCodeParser {
    fn line(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(value) = frame(line) else {
            return vec![StreamEvent::Activity];
        };

        match value.get("type").and_then(Value::as_str) {
            Some("result") => {
                // The result line carries the finished answer; an empty one
                // (a failed or truncated run) must not wipe what streamed.
                if let Some(text) = value.get("finalText").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.text = text.to_string();
                    }
                }
                self.stop_reason = value
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let reported = value
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .and_then(read_usage);
                if let Some(usage) = reported {
                    self.usage = Some(usage);
                }
                if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                }
                if value.get("subtype").and_then(Value::as_str) == Some("error") {
                    self.error = Some(
                        value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("the run failed")
                            .to_string(),
                    );
                }
                match reported {
                    Some(usage) => vec![StreamEvent::Usage(usage)],
                    None => vec![StreamEvent::Activity],
                }
            }
            Some("event") => {
                // Command Code streams the answer as `text_delta` events while
                // it works, so the run is visible as it happens rather than
                // appearing all at once at the end.
                let inner = value.get("event");
                if let Some(id) = inner
                    .and_then(|inner| inner.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    self.session_id = Some(id.to_string());
                }
                let mut events = Vec::new();

                // Text is taken first, and taken alongside anything else on the
                // frame rather than instead of it: returning early would let a
                // frame that carried both drop part of the answer, which is a
                // far worse failure than an uncounted token.
                let is_delta = inner
                    .and_then(|inner| inner.get("type"))
                    .and_then(Value::as_str)
                    == Some("text_delta");
                if is_delta {
                    if let Some(chunk) = inner
                        .and_then(|inner| inner.get("delta"))
                        .and_then(Value::as_str)
                    {
                        if !chunk.is_empty() {
                            self.text.push_str(chunk);
                            events.push(StreamEvent::Text(chunk.to_string()));
                        }
                    }
                }

                // `model_request_end` and `turn_end` carry usage, and they
                // arrive before the run ends. Reading them here rather than only
                // off the final result line is what lets a killed run report
                // what it had already spent.
                if let Some(usage) = inner.and_then(command_code_usage) {
                    self.usage = Some(usage);
                    events.push(StreamEvent::Usage(usage));
                }

                if events.is_empty() {
                    events.push(StreamEvent::Activity);
                }
                events
            }
            _ => vec![StreamEvent::Activity],
        }
    }

    fn finish(self: Box<Self>) -> Result<TurnSummary, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(TurnSummary {
            text: self.text,
            stop_reason: self.stop_reason,
            usage: self.usage,
            session_id: self.session_id,
            ..TurnSummary::default()
        })
    }
}

/// Usage from a Command Code event frame, wherever it sits.
///
/// `model_request_end` and `turn_end` carry it at the top level; `run_end` nests
/// it under `result`. All three arrive *before* the run is over, which is the
/// reason to look for them at all: a run that is killed or cancelled mid-flight
/// never reaches its final `result` line, and its bills are still owed.
fn command_code_usage(inner: &Value) -> Option<Usage> {
    let direct = inner.get("usage").filter(|usage| !usage.is_null());
    let nested = inner
        .get("result")
        .and_then(|result| result.get("usage"))
        .filter(|usage| !usage.is_null());
    read_usage(direct.or(nested)?)
}

/// Grok Build's `streaming-json`: one `type`-tagged object per line, with text
/// in `data`, and `end` last.
#[derive(Default)]
pub struct GrokParser {
    text: String,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    error: Option<String>,
}

impl Parser for GrokParser {
    fn line(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(value) = frame(line) else {
            return vec![StreamEvent::Activity];
        };

        match value.get("type").and_then(Value::as_str) {
            Some("text") => match value.get("data").and_then(Value::as_str) {
                Some(chunk) if !chunk.is_empty() => {
                    self.text.push_str(chunk);
                    vec![StreamEvent::Text(chunk.to_string())]
                }
                _ => vec![StreamEvent::Activity],
            },
            Some("usage") => {
                self.stop_reason = value
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(usage) = value
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .and_then(read_usage)
                {
                    self.usage = Some(usage);
                    return vec![StreamEvent::Usage(usage)];
                }
                vec![StreamEvent::Activity]
            }
            Some("end") => {
                if let Some(reason) = value.get("stopReason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(usage) = value
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .and_then(read_usage)
                {
                    self.usage = Some(usage);
                    return vec![StreamEvent::Usage(usage)];
                }
                vec![StreamEvent::Activity]
            }
            Some("error") => {
                self.error = Some(
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the run failed")
                        .to_string(),
                );
                vec![StreamEvent::Activity]
            }
            // thought, tool_call, tool_call_update, plan, and anything added
            // later: real progress, nothing to show.
            _ => vec![StreamEvent::Activity],
        }
    }

    fn finish(self: Box<Self>) -> Result<TurnSummary, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(TurnSummary {
            text: self.text,
            stop_reason: self.stop_reason,
            usage: self.usage,
            ..TurnSummary::default()
        })
    }
}

/// A trimmed, parsed line, or `None` when it is blank or not a JSON object.
fn frame(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut Box<dyn Parser>, lines: &[&str]) -> Vec<StreamEvent> {
        lines.iter().flat_map(|line| parser.line(line)).collect()
    }

    fn text_of(events: &[StreamEvent]) -> String {
        let mut out = String::new();
        for event in events {
            if let StreamEvent::Text(text) = event {
                out.push_str(text);
            }
        }
        out
    }

    // ---- plain ------------------------------------------------------------

    #[test]
    fn plain_text_becomes_the_answer() {
        let mut parser = parser_for(Dialect::Plain);
        let events = feed(&mut parser, &["first line", "second line"]);
        assert_eq!(text_of(&events), "first line\nsecond line\n");
        assert_eq!(
            parser.finish().expect("no error").text,
            "first line\nsecond line"
        );
    }

    #[test]
    fn blank_plain_lines_are_preserved_as_spacing() {
        let mut parser = parser_for(Dialect::Plain);
        feed(&mut parser, &["a", "", "b"]);
        assert_eq!(parser.finish().expect("no error").text, "a\n\nb");
    }

    // ---- command code -----------------------------------------------------
    //
    // These frames are copied from a real `cmd -p ... --output-format json` run,
    // not from the documentation: the shape that mattered (that the answer
    // streams as `text_delta`, and that usage is camelCase) is not in the docs.

    #[test]
    fn command_code_bookkeeping_events_are_progress_not_answer_text() {
        let mut parser = parser_for(Dialect::CommandCode);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"event","event":{"type":"run_start","sessionId":"2795e208"}}"#,
                r#"{"type":"event","event":{"type":"turn_start","turnNumber":1}}"#,
                r#"{"type":"event","event":{"type":"model_request_start","model":"deepseek/deepseek-v4-flash"}}"#,
                r#"{"type":"event","event":{"type":"model_trace","traceId":"d1631e34"}}"#,
                r#"{"type":"event","event":{"type":"message_update","content":[{"type":"text","text":"pong"}]}}"#,
                r#"{"type":"event","event":{"type":"message_end","content":[{"type":"text","text":"pong"}]}}"#,
            ],
        );
        assert_eq!(text_of(&events), "", "only text_delta carries the answer");
        assert!(events.iter().all(|e| matches!(e, StreamEvent::Activity)));
    }

    #[test]
    fn command_code_streams_the_answer_as_text_delta_events() {
        let mut parser = parser_for(Dialect::CommandCode);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"event","event":{"type":"text_delta","delta":"p"}}"#,
                r#"{"type":"event","event":{"type":"text_delta","delta":"ong"}}"#,
            ],
        );
        assert_eq!(text_of(&events), "pong");
        assert_eq!(parser.finish().expect("no error").text, "pong");
    }

    #[test]
    fn command_code_reports_usage_from_the_frames_that_arrive_before_the_end() {
        // The fixture's real frames. `model_request_end` and `turn_end` both
        // carry usage and both arrive before the run is over — which matters
        // because a run that is killed or cancelled never reaches its final
        // result line, and its bills are owed regardless.
        let mut parser = CommandCodeParser::default();

        let early = parser.line(
            r#"{"type":"event","event":{"type":"model_request_end","model":"m","usage":{"inputTokens":15329,"outputTokens":18,"cacheReadTokens":5632,"cacheWriteTokens":0},"stopReason":"stop"}}"#,
        );
        let usage = early
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("model_request_end carries usage, so it should be reported");
        assert_eq!(usage.prompt_tokens, 15329);
        assert_eq!(usage.cache_read_tokens, 5632);

        // And the turn_end frame, which nests nothing but still reports.
        let mid = parser.line(
            r#"{"type":"event","event":{"type":"turn_end","turnNumber":1,"hadToolCalls":false,"usage":{"inputTokens":15329,"outputTokens":18}}}"#,
        );
        assert!(
            mid.iter().any(|e| matches!(e, StreamEvent::Usage(_))),
            "{mid:?}"
        );

        // run_end nests its usage under `result`, so it needs finding there.
        let late = parser.line(
            r#"{"type":"event","event":{"type":"run_end","result":{"finalText":"hi","usage":{"inputTokens":15329,"outputTokens":18}}}}"#,
        );
        assert!(
            late.iter().any(|e| matches!(e, StreamEvent::Usage(_))),
            "{late:?}"
        );
    }

    #[test]
    fn a_frame_carrying_text_and_usage_yields_both() {
        // Neither may be dropped for the sake of the other. Losing a text delta
        // would corrupt the answer; losing the usage would understate the cost.
        let mut parser = CommandCodeParser::default();
        let events = parser.line(
            r#"{"type":"event","event":{"type":"text_delta","delta":"half an answer","usage":{"inputTokens":42,"outputTokens":7}}}"#,
        );

        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Text(t) if t == "half an answer")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Usage(u) if u.prompt_tokens == 42)),
            "{events:?}"
        );
    }

    #[test]
    fn command_code_still_reports_usage_from_the_result_line() {
        let mut parser = CommandCodeParser::default();
        let events = parser.line(
            r#"{"type":"result","subtype":"success","stopReason":"end_turn","usage":{"inputTokens":15754,"outputTokens":3},"finalText":"pong"}"#,
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Usage(u) if u.prompt_tokens == 15754)),
            "{events:?}"
        );
    }

    #[test]
    fn grok_reports_usage_as_soon_as_its_frame_arrives() {
        let mut parser = GrokParser::default();

        let events = parser.line(
            r#"{"type":"usage","usage":{"input_tokens":12,"output_tokens":3},"stopReason":"end_turn"}"#,
        );
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("the usage frame should be reported");
        assert_eq!(usage.prompt_tokens, 12);
    }

    #[test]
    fn command_code_takes_the_final_answer_and_usage_from_the_result_line() {
        let mut parser = parser_for(Dialect::CommandCode);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"event","event":{"type":"text_delta","delta":"pon"}}"#,
                r#"{"type":"event","event":{"type":"text_delta","delta":"g"}}"#,
                r#"{"type":"result","subtype":"success","sessionId":"78867895","stopReason":"end_turn","usage":{"inputTokens":15754,"outputTokens":3,"cacheReadTokens":5376,"cacheWriteTokens":0},"durationMs":2143,"finalText":"pong"}"#,
            ],
        );
        assert_eq!(text_of(&events), "pong");

        let summary = parser.finish().expect("no error");
        // The result line replaces what streamed rather than doubling it.
        assert_eq!(summary.text, "pong");
        assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));
        let usage = summary.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 15754);
        assert_eq!(usage.completion_tokens, 3);
    }

    #[test]
    fn command_code_reports_a_failed_run() {
        let mut parser = parser_for(Dialect::CommandCode);
        feed(
            &mut parser,
            &[r#"{"type":"result","subtype":"error","error":"not signed in","finalText":""}"#],
        );
        assert_eq!(parser.finish().expect_err("should fail"), "not signed in");
    }

    /// The whole of a real `cmd -p "…" --output-format json` run, recorded
    /// verbatim: 30 frames, including the bookkeeping and the `run_end` state
    /// dump that the parser has to pass over without leaking into the answer.
    const RECORDED_RUN: &str = include_str!("fixtures/cmd-stream.jsonl");

    #[test]
    fn a_recorded_command_code_run_is_read_end_to_end() {
        let mut parser = parser_for(Dialect::CommandCode);
        let mut events = Vec::new();
        for line in RECORDED_RUN.lines() {
            events.extend(parser.line(line));
        }

        // The thinking deltas are the bulk of the recording, and none of them
        // may reach the transcript as answer text.
        assert_eq!(
            text_of(&events),
            "hello",
            "only the text_delta carries the answer"
        );

        let summary = parser.finish().expect("the recorded run succeeded");
        assert_eq!(summary.text, "hello");
        assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));

        let usage = summary.usage.expect("the result line carries usage");
        assert_eq!(usage.prompt_tokens, 15329);
        assert_eq!(usage.completion_tokens, 18);
        assert_eq!(usage.cache_read_tokens, 5632);
        assert_eq!(usage.cache_write_tokens, 0);

        assert_eq!(
            summary.session_id.as_deref(),
            Some("5d978fce-0cb0-4069-9635-c3cec5bbe020"),
            "the id to resume this conversation with"
        );
    }

    #[test]
    fn command_code_keeps_streamed_text_when_the_result_is_empty() {
        let mut parser = parser_for(Dialect::CommandCode);
        feed(
            &mut parser,
            &[
                r#"{"type":"event","event":{"type":"text_delta","delta":"half a thought"}}"#,
                // A failed or truncated run reports an empty finalText, which
                // must not erase what the user already saw.
                r#"{"type":"result","subtype":"max_turns","stopReason":"max_turns","finalText":""}"#,
            ],
        );
        let summary = parser.finish().expect("no error");
        assert_eq!(summary.text, "half a thought");
        assert_eq!(summary.stop_reason.as_deref(), Some("max_turns"));
    }

    #[test]
    fn command_code_max_turns_is_carried_through_as_a_stop_reason() {
        let mut parser = parser_for(Dialect::CommandCode);
        feed(
            &mut parser,
            &[
                r#"{"type":"result","subtype":"max_turns","stopReason":"max_turns","finalText":"partial"}"#,
            ],
        );
        let summary = parser.finish().expect("no error");
        assert_eq!(summary.stop_reason.as_deref(), Some("max_turns"));
        assert_eq!(summary.text, "partial");
    }

    #[test]
    fn command_code_never_puts_raw_json_in_the_answer() {
        let mut parser = parser_for(Dialect::CommandCode);
        let events = feed(&mut parser, &["this is not json", "{not json either"]);
        assert_eq!(text_of(&events), "");
        assert!(parser.finish().expect("no error").text.is_empty());
    }

    // ---- grok -------------------------------------------------------------

    #[test]
    fn grok_text_frames_stream_into_the_answer() {
        let mut parser = parser_for(Dialect::Grok);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"text","data":"Here's "}"#,
                r#"{"type":"text","data":"a summary"}"#,
            ],
        );
        assert_eq!(text_of(&events), "Here's a summary");
        assert_eq!(parser.finish().expect("no error").text, "Here's a summary");
    }

    #[test]
    fn grok_thoughts_and_tool_calls_are_progress_only() {
        let mut parser = parser_for(Dialect::Grok);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"thought","data":"Analyzing the directory structure..."}"#,
                r#"{"type":"tool_call","toolCallId":"call_1","title":"Read","kind":"read","status":"in_progress","toolName":"read_file","rawInput":{"path":"src/main.rs"},"content":[],"locations":[]}"#,
                r#"{"type":"tool_call_update","toolCallId":"call_1","status":"completed","content":[],"rawOutput":{"lines":42},"locations":[]}"#,
            ],
        );
        assert!(events.iter().all(|e| matches!(e, StreamEvent::Activity)));

        let summary = parser.finish().expect("no error");
        assert!(summary.text.is_empty());
        // The claim the abandoned-text guarantee rests on: a tool-call frame is
        // progress, never a call, so nothing here can become a side effect.
        assert!(
            summary.tool_calls.is_empty(),
            "a tool-call frame is progress, not a call: {:?}",
            summary.tool_calls
        );
    }

    #[test]
    fn grok_end_carries_the_stop_reason_and_usage() {
        let mut parser = parser_for(Dialect::Grok);
        feed(
            &mut parser,
            &[
                r#"{"type":"text","data":"done"}"#,
                r#"{"type":"usage","messageId":"resp_1","stopReason":"end_turn","usage":{"input_tokens":812,"output_tokens":45,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":0}}"#,
                r#"{"type":"end","stopReason":"end_turn","sessionId":"abc123","requestId":"xyz789","num_turns":7}"#,
            ],
        );
        let summary = parser.finish().expect("no error");
        assert_eq!(summary.text, "done");
        assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));
        let usage = summary.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 812);
        assert_eq!(usage.completion_tokens, 45);
    }

    #[test]
    fn grok_reports_an_error_frame() {
        let mut parser = parser_for(Dialect::Grok);
        feed(
            &mut parser,
            &[r#"{"type":"error","message":"Couldn't start session: no credentials"}"#],
        );
        assert_eq!(
            parser.finish().expect_err("should fail"),
            "Couldn't start session: no credentials"
        );
    }

    #[test]
    fn grok_unknown_event_types_are_tolerated() {
        let mut parser = parser_for(Dialect::Grok);
        let events = feed(
            &mut parser,
            &[
                r#"{"type":"plan","entries":[]}"#,
                r#"{"type":"available_commands","tools":[],"commands":[]}"#,
                r#"{"type":"max_turns_reached"}"#,
                r#"{"type":"something_added_later","whatever":1}"#,
            ],
        );
        assert!(events.iter().all(|e| matches!(e, StreamEvent::Activity)));
        assert!(parser.finish().expect("no error").text.is_empty());
    }

    #[test]
    fn grok_an_end_after_a_usage_line_keeps_the_tokens() {
        let mut parser = parser_for(Dialect::Grok);
        feed(
            &mut parser,
            &[
                r#"{"type":"usage","stopReason":"tool_use","usage":{"input_tokens":10,"output_tokens":2}}"#,
                // The end line here carries no usage of its own.
                r#"{"type":"end","stopReason":"end_turn","sessionId":"s"}"#,
            ],
        );
        let summary = parser.finish().expect("no error");
        assert_eq!(summary.usage.expect("usage").prompt_tokens, 10);
        assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn an_empty_text_frame_is_activity_not_empty_answer_text() {
        let mut parser = parser_for(Dialect::Grok);
        let events = feed(&mut parser, &[r#"{"type":"text","data":""}"#]);
        assert!(matches!(events.as_slice(), [StreamEvent::Activity]));
    }

    #[test]
    fn a_json_array_line_is_not_mistaken_for_a_frame() {
        let mut parser = parser_for(Dialect::Grok);
        let events = feed(&mut parser, &["[1,2,3]"]);
        assert!(matches!(events.as_slice(), [StreamEvent::Activity]));
    }

    #[test]
    fn dialects_default_to_plain() {
        assert_eq!(Dialect::default(), Dialect::Plain);
    }
}
