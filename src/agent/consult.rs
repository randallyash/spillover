//! Asking a more capable tier a narrow question, instead of handing it the turn.
//!
//! Escalating delegates: the driver is abandoned and the next tier inherits the
//! whole problem, which means it pays for the entire conversation again — a cold
//! prefix on every remaining turn of a sticky session. A *consult* keeps the cheap
//! model driving and spends the frontier on one specific question.
//!
//! The hard part is not the plumbing, it is what goes in the question. We consult
//! because the driver is stuck, and it is often stuck precisely because it does
//! not understand the problem — so letting it write the question asks the weakest
//! link to do the hard part. This module therefore builds the question out of
//! **evidence spill already holds**: the user's own words, the reason the tier
//! was judged stuck, and the raw tool calls and results from the failed attempt.
//! The driver contributes no prose of its own. Its looped output is evidence too,
//! which is why repetition is quoted verbatim rather than described.
//!
//! Three limits follow from the plan's cost analysis, and each is enforced here
//! rather than left to the caller:
//!
//! - **The consultant gets no tools.** For an `openai` tier that is what makes
//!   "the answer is prose" true by construction: it cannot act, so it must
//!   answer, and the call is one round trip rather than a tool loop.
//! - **The answer is bounded.** It is injected into the driver's history and
//!   re-read on every later turn, so an essay would compound. It is clipped.
//! - **The question is bounded too**, for the same reason in the other
//!   direction: a consult is a fresh call, and its input is billed in full.

use crate::detect::StuckReason;
use crate::session::{ChatMessage, Role};

/// How much of the failed attempt's evidence to include.
///
/// A consult is a fresh, uncached call, so every character is billed — but too
/// little evidence and the consultant is guessing. These bound the two ends.
const MAX_EVIDENCE_CHARS: usize = 6_000;
const MAX_EVIDENCE_MESSAGES: usize = 12;

/// How long the injected answer may be.
///
/// A driving model re-reads its whole history on every subsequent turn, so this
/// is the figure that compounds. Generous enough for a real explanation, tight
/// enough that a rambling consultant cannot halve the context budget.
pub const MAX_ANSWER_CHARS: usize = 2_000;

/// The question put to the consultant, and a one-line version of it for the
/// transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consult {
    pub question: String,
}

/// A consult that has already happened in this turn.
///
/// Passed to the next one so the consultant is not asked the same question
/// again: advice that did not work is the most useful thing to know, and
/// repeating it is the most likely failure of a second consult.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Previous {
    pub consultant: String,
    pub answer: String,
}

/// Build the question from what spill already knows.
///
/// `goal` is the user's own message, not the driver's restatement of it.
/// `evidence` is the messages the failed attempt added, in order.
pub fn build(
    goal: &str,
    reason: &StuckReason,
    evidence: &[ChatMessage],
    previous: &[Previous],
) -> Consult {
    let mut question = String::new();

    question.push_str(
        "Another coding agent is stuck on a task and has asked you one question. You cannot act \
         on its behalf and have been given no tools, so answer in prose and in full sentences: \
         say what to do, concretely, based only on the evidence below. Do not say you will \
         investigate, and do not ask for more information unless the evidence is genuinely \
         insufficient — in that case name the single thing that must be checked.\n\n",
    );

    question.push_str(&format!("The task, in the user's own words:\n{goal}\n\n"));

    // Said before the evidence, because it changes what a useful answer is: if
    // the obvious advice has already been tried, repeating it wastes the call.
    if !previous.is_empty() {
        question.push_str("It has already been helped and the advice did not work:\n");
        for earlier in previous {
            question.push_str(&format!(
                "- {} said: {}\n",
                earlier.consultant,
                clip(earlier.answer.trim(), 400)
            ));
        }
        question.push_str(
            "Do not repeat that advice. Given it did not work, something about it must have been \
             wrong or incomplete — say what, and what to do instead.\n\n",
        );
    }

    question.push_str(&format!(
        "Why the agent was stopped, judged from outside: {}\n\n",
        reason.summary()
    ));

    // When the driver was repeating itself, the evidence is its own output — and
    // it cannot come from the session, because a turn that loops is abandoned
    // before any of it is recorded. The detector kept a sample, which is exactly
    // the thing the consultant needs to see.
    if let StuckReason::Repetition { sample, repeats } = reason {
        question.push_str(&format!(
            "It produced this same output {repeats} times, verbatim:\n{}\n\n",
            clip(sample.trim(), 800)
        ));
    }

    let rendered = render_evidence(evidence);
    if rendered.is_empty() {
        question.push_str(
            "There is no tool output: the agent never got as far as running anything, or its \
             replies were the problem itself. The reason above is the whole of the evidence.\n\n",
        );
    } else {
        question.push_str("What was happening, exactly as it happened:\n");
        question.push_str(&rendered);
        question.push('\n');
    }

    question.push_str(
        "Answer with the specific next step this agent should take, and why the evidence \
         points there. Be brief: your answer is passed back to a model with a small context \
         window, so a short, decisive answer is worth more than a thorough one.",
    );

    Consult { question }
}

/// The failed attempt, rendered as raw evidence rather than prose.
///
/// Tool calls are shown as the call and its result, which is the pair that
/// actually explains a stuck loop. Tool results are the raw output, because a
/// summarised error is exactly the paraphrase this module exists to avoid.
///
/// The budget is spent from the *newest* message backwards, and what survives is
/// a contiguous run of recent messages. The ordering matters twice over: the
/// newest activity is the state the driver is stuck in, so it is what the
/// consultant most needs, and a message that does not fit is dropped rather than
/// cut, so the consultant is never shown half an error and asked to trust it.
fn render_evidence(evidence: &[ChatMessage]) -> String {
    let window = &evidence[evidence.len().saturating_sub(MAX_EVIDENCE_MESSAGES)..];

    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;

    for message in window.iter().rev() {
        let rendered = match message.role {
            Role::User => format!("[user] {}", message.content),
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    format!("[agent] {}", message.content)
                } else {
                    let calls: Vec<String> = message
                        .tool_calls
                        .iter()
                        .map(|call| format!("{}({})", call.name, call.arguments))
                        .collect();
                    format!("[agent calls] {}", calls.join(", "))
                }
            }
            Role::Tool => format!("[result] {}", message.content),
            Role::System => continue,
        };

        let length = rendered.chars().count();
        if used + length > MAX_EVIDENCE_CHARS {
            // Stop rather than skip: carrying on would leave holes in the
            // middle, which reads as a continuous record when it is not one.
            break;
        }

        used += length;
        kept.push(rendered);
    }

    // Back into the order things happened, which is how it has to be read.
    kept.reverse();
    let elided = evidence.len().saturating_sub(kept.len());

    let mut out = kept.join("\n");
    if elided > 0 {
        out.push_str(&format!(
            "\n({elided} earlier message{} left out for length)",
            if elided == 1 { "" } else { "s" }
        ));
    }
    out.trim_end().to_string()
}

/// The message injected into the driver's history for a successful consult.
///
/// Phrased as something the driver is *told* by a stronger reader of the same
/// evidence, and explicitly not as a tool result or a user turn — a model that
/// mistook this for the user speaking would answer it rather than act on it.
pub fn injection(consultant: &str, answer: &str) -> String {
    format!(
        "A more capable model ({consultant}) was asked about this and said:\n\n{}\n\n\
         Act on that. It could not run anything itself, so it is advice, not a report of work \
         already done.",
        clip(answer.trim(), MAX_ANSWER_CHARS)
    )
}

/// Cut to a character budget, marking that it happened.
pub fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    // Leave a cell for the marker, so the clip is visible as one.
    let head: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;

    fn tool_call(name: &str, arguments: &str) -> ChatMessage {
        ChatMessage::assistant(
            "",
            vec![ToolCall {
                id: "call_1".to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
            }],
        )
    }

    fn build_simple(evidence: &[ChatMessage]) -> Consult {
        build(
            "make the tests pass",
            &StuckReason::RepeatedToolCall {
                tool: "run_shell".to_string(),
                times: 3,
            },
            evidence,
            &[],
        )
    }

    #[test]
    fn the_question_carries_the_users_own_words_verbatim() {
        // The plan's central point: the goal comes from the user, never from the
        // driver's restatement of it.
        let consult = build_simple(&[]);
        assert!(
            consult.question.contains("make the tests pass"),
            "{}",
            consult.question
        );
    }

    #[test]
    fn the_question_carries_the_raw_reason() {
        let consult = build_simple(&[]);
        assert!(
            consult
                .question
                .contains("called run_shell with identical arguments 3 times"),
            "{}",
            consult.question
        );
    }

    #[test]
    fn a_failed_tool_call_is_shown_with_its_arguments_and_its_raw_result() {
        // The pair that explains a loop. A summarised error would be the
        // paraphrase this module exists to avoid.
        let evidence = vec![
            tool_call("run_shell", r#"{"command":"cargo test"}"#),
            ChatMessage::tool_result("call_1", "error[E0308]: mismatched types\n  --> src/a.rs:4"),
        ];

        let consult = build_simple(&evidence);
        let question = &consult.question;

        assert!(question.contains("run_shell"), "{question}");
        assert!(question.contains("cargo test"), "{question}");
        assert!(
            question.contains("error[E0308]: mismatched types"),
            "the raw error must survive: {question}"
        );
        assert!(question.contains("src/a.rs:4"), "{question}");
    }

    #[test]
    fn a_repeated_answer_is_quoted_rather_than_described() {
        // Repetition is the one case where the driver's own output *is* the
        // evidence, so it goes in verbatim.
        let looping = "the same line\nthe same line\nthe same line";
        let evidence = vec![ChatMessage::assistant(looping, Vec::new())];

        let consult = build(
            "do the thing",
            &StuckReason::Repetition {
                sample: "the same line".to_string(),
                repeats: 4,
            },
            &evidence,
            &[],
        );

        assert!(consult.question.contains("the same line"), "{consult:?}");
        assert!(
            consult
                .question
                .contains("repeated the same output 4 times")
        );
    }

    #[test]
    fn a_consult_with_no_evidence_says_so_rather_than_leaving_a_blank() {
        // Telling the consultant there is nothing to read is more useful than an
        // empty section it has to interpret.
        let consult = build_simple(&[]);
        assert!(
            consult.question.contains("no tool output"),
            "{}",
            consult.question
        );
    }

    #[test]
    fn the_question_tells_the_consultant_it_cannot_act() {
        // This is what makes "the answer is prose" true rather than hoped for,
        // alongside being given no tools.
        let consult = build_simple(&[]);
        assert!(
            consult.question.contains("no tools"),
            "{}",
            consult.question
        );
        assert!(
            consult.question.contains("answer in prose"),
            "{}",
            consult.question
        );
    }

    #[test]
    fn the_question_asks_for_brevity_and_says_why() {
        let consult = build_simple(&[]);
        assert!(
            consult.question.contains("context window"),
            "the reason a short answer matters should be stated: {}",
            consult.question
        );
    }

    #[test]
    fn system_messages_are_not_passed_to_the_consultant() {
        // The driver's system prompt is its own instructions; sending it would
        // confuse the consultant about its role.
        let evidence = vec![
            ChatMessage::system("You are spill, working in /home/x"),
            tool_call("read_file", r#"{"path":"a.rs"}"#),
        ];
        let consult = build_simple(&evidence);
        assert!(
            !consult.question.contains("You are spill"),
            "{}",
            consult.question
        );
    }

    #[test]
    fn a_huge_attempt_is_bounded_rather_than_sent_whole() {
        // A consult is a fresh call, so its input is billed in full; an unbounded
        // question could cost more than the escalation it avoided.
        let huge = "x".repeat(50_000);
        let mut evidence = Vec::new();
        for _ in 0..40 {
            evidence.push(ChatMessage::tool_result("call_1", huge.clone()));
        }

        let consult = build_simple(&evidence);
        assert!(
            consult.question.chars().count() < MAX_EVIDENCE_CHARS + 2_000,
            "the question grew to {} characters",
            consult.question.chars().count()
        );
        assert!(
            consult.question.contains("left out for length"),
            "it should say it was trimmed: {}",
            &consult.question[..200.min(consult.question.len())]
        );
    }

    #[test]
    fn the_newest_evidence_is_what_survives_a_trim() {
        // The driver is stuck in its most recent state, so that is the part worth
        // spending the budget on.
        let mut evidence = Vec::new();
        for n in 0..40 {
            evidence.push(ChatMessage::tool_result(
                format!("call_{n}"),
                format!("result number {n} {}", "x".repeat(500)),
            ));
        }

        let consult = build_simple(&evidence);
        assert!(
            consult.question.contains("result number 39"),
            "the newest result should be present"
        );
        assert!(
            !consult.question.contains("result number 0"),
            "the oldest should have been dropped"
        );
    }

    #[test]
    fn a_truncated_evidence_block_does_not_show_half_an_error() {
        // Deeper than the char budget, so it should be dropped entirely rather
        // than shown cut off — a consultant reading half an error would draw the
        // wrong conclusion.
        let evidence = vec![
            ChatMessage::tool_result("call_0", "a".repeat(MAX_EVIDENCE_CHARS + 100)),
            ChatMessage::tool_result("call_1", "the short final line"),
        ];

        let consult = build_simple(&evidence);
        // The short one survives, since it fits in what is left.
        assert!(consult.question.contains("the short final line"));
        assert!(
            !consult.question.contains("aaaa"),
            "the over-long one should be dropped, not cut"
        );
    }

    #[test]
    fn the_injected_answer_names_the_consultant_and_says_it_did_not_act() {
        let injected = injection("DeepSeek", "Change the type to u32.");
        assert!(injected.contains("DeepSeek"), "{injected}");
        assert!(injected.contains("Change the type to u32."), "{injected}");
        assert!(
            injected.contains("advice, not a report of work"),
            "the driver must not think the work is done: {injected}"
        );
    }

    #[test]
    fn a_long_answer_is_clipped_before_it_enters_the_history() {
        // This is the figure that compounds: the driver re-reads it every turn.
        let answer = "y".repeat(10_000);
        let injected = injection("DeepSeek", &answer);
        assert!(
            injected.chars().count() < MAX_ANSWER_CHARS + 300,
            "the injection grew to {} characters",
            injected.chars().count()
        );
        assert!(injected.contains('…'), "the clip should be visible");
    }

    #[test]
    fn a_short_answer_is_injected_whole() {
        let injected = injection("DeepSeek", "  Use a HashMap.  ");
        assert!(injected.contains("Use a HashMap."), "{injected}");
        assert!(
            !injected.contains('…'),
            "nothing to clip, so nothing should look clipped"
        );
    }

    #[test]
    fn clipping_keeps_whole_characters() {
        let clipped = clip("日本語のテキスト", 4);
        assert_eq!(clipped.chars().count(), 4);
        assert!(clipped.ends_with('…'));
        // Not four bytes: four characters.
        assert!(clipped.starts_with('日'), "{clipped:?}");
    }

    #[test]
    fn the_question_does_not_contain_a_placeholder_for_the_driver() {
        // A leftover `{driver}` in the sent text would be a bug the consultant
        // reads as literal instructions.
        let consult = build_simple(&[]);
        assert!(!consult.question.contains("{driver}"), "{consult:?}");
    }
}
