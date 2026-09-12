//! The golden loop: the failure path, driven end to end against real HTTP.
//!
//! Everything else in the suite stubs one layer or another. The provider tests
//! drive a real HTTP call but only one tier, with a hand-fed request. The agent
//! tests drive the whole loop but through a `ScriptedProvider` that answers with
//! ready-made `TurnSummary` values — so the detector never sees a streamed byte
//! and the chain never talks to anything.
//!
//! What is missing is the seam where the parts meet, and that seam is the
//! product: it is the only place where a real stream feeds a real detector,
//! whose verdict really does abandon a tier mid-flight, whose side effects
//! really have happened, and whose bill really is owed. These tests run that
//! with a real `OpenAiProvider` against a mock endpoint, a real `FallbackChain`,
//! the real detector, the real tool registry writing to the real filesystem, and
//! the real accounting. Only the model is fake.
//!
//! Three scenarios, and they are the three that matter:
//!
//! - [`the_golden_loop`] — a local tier loops, is detected, and the turn is
//!   handed to the frontier with only the original question.
//! - [`a_consult_keeps_the_driver_and_feeds_it_the_answer`] — the same stall,
//!   but the frontier is asked one question instead of taking the turn.
//! - [`a_consult_that_fails_escalates_rather_than_stranding_the_turn`] — the
//!   consultant is down, so the fallback everyone relies on actually happens.
//!
//! The hand-over scenarios ask for escalation explicitly. Consulting is the
//! default, so a chain left alone would ask the frontier a question first — and
//! these are here to prove the other path, which is the one a chain of one
//! always takes and the one that has to happen when a consult cannot.
//!
//! These are deliberately written against observable behaviour rather than
//! internals: the transcript a tier was sent, the file on disk, the events the
//! interface draws from. A change to how any of it is implemented should not
//! need these touched, which is what makes them worth having.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::agent::approval::testing::AlwaysApprove;
use crate::agent::tools::Registry;
use crate::agent::{AgentConfig, AgentEvent, Canceller, Command, DEFAULT_MAX_STEPS};
use crate::app::App;
use crate::config::{Config, Limits, OnStuck};
use crate::fallback::{FallbackChain, Tier};
use crate::provider::openai::OpenAiProvider;
use crate::provider::{Provider, Usage};

/// The line the local tier cannot stop repeating.
///
/// Four is not arbitrary: it is `Limits::default()`'s `max_repeat_run`, so this
/// fixture is exercising the shipped threshold rather than a test-only one.
const LOOPED_LINE: &str = "the same line";
const REPEATS: usize = 4;

/// What the local tier writes before it loses the plot.
const NOTE: &str = "written by the local model";

/// How the user's turn is phrased, so it can be looked for on the wire.
const GOAL: &str = "fix the parser";

// ---------------------------------------------------------------- transport

/// One SSE `data:` payload.
fn frame(value: serde_json::Value) -> String {
    value.to_string()
}

/// A content delta, as a server streams an answer.
fn text(content: &str) -> String {
    frame(serde_json::json!({
        "choices": [{ "delta": { "content": content }, "index": 0 }]
    }))
}

/// A frame that ends the turn.
fn stop(reason: &str) -> String {
    frame(serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": reason, "index": 0 }]
    }))
}

/// What the request cost. Sent mid-stream on purpose: a stream that is about to
/// be killed never reaches the frame that would have reported its total, and the
/// whole point of the cost assertion is that an abandoned attempt is still owed.
fn usage(prompt_tokens: u64, completion_tokens: u64) -> String {
    frame(serde_json::json!({
        "choices": [{ "delta": {}, "index": 0 }],
        "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens }
    }))
}

/// A tool call, in the shape a server streams one.
fn tool_call(name: &str, arguments: serde_json::Value) -> String {
    frame(serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            },
            "index": 0
        }]
    }))
}

/// A turn that asks to write a file, which is how the junk gets there.
fn asks_to_write(path: &str, content: &str) -> Vec<String> {
    vec![
        usage(200, 8),
        tool_call(
            "write_file",
            serde_json::json!({ "path": path, "content": content }),
        ),
        stop("tool_calls"),
    ]
}

/// A turn that asks to read a file, which is what a model stuck on a path it
/// cannot find keeps doing.
fn asks_to_read(path: &str) -> Vec<String> {
    vec![
        usage(200, 8),
        tool_call("read_file", serde_json::json!({ "path": path })),
        stop("tool_calls"),
    ]
}

/// The same line, `REPEATS` times. This is what the detector is for.
fn looping_answer() -> Vec<String> {
    let mut frames = vec![usage(1_500, 40)];
    for _ in 0..REPEATS {
        frames.push(text(&format!("{LOOPED_LINE}\n")));
    }
    frames.push(stop("stop"));
    frames
}

fn plain_answer(answer: &str) -> Vec<String> {
    vec![usage(900, 12), text(answer), stop("stop")]
}

/// A full SSE response body.
fn sse(frames: &[String]) -> Vec<u8> {
    let mut body = String::new();
    for f in frames {
        body.push_str("data: ");
        body.push_str(f);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

// ------------------------------------------------------------- the fake tier

/// Request bodies a tier was sent, in order.
///
/// The transcript on the wire is the thing worth asserting on: it is what the
/// model actually saw, which is the only definition of "the turn was handed
/// over" that means anything.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

impl Seen {
    fn record(&self, body: &str) {
        self.0.lock().expect("lock").push(body.to_string());
    }

    fn all(&self) -> Vec<String> {
        self.0.lock().expect("lock").clone()
    }

    fn count(&self) -> usize {
        self.0.lock().expect("lock").len()
    }

    /// The `messages` array of the Nth request this tier received.
    fn messages(&self, nth: usize) -> Vec<serde_json::Value> {
        let bodies = self.all();
        let body = bodies
            .get(nth)
            .unwrap_or_else(|| panic!("this tier was only asked {} time(s)", bodies.len()));
        let parsed: serde_json::Value =
            serde_json::from_str(body).unwrap_or_else(|error| panic!("not JSON ({error}): {body}"));
        parsed["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("no messages array in {body}"))
            .clone()
    }

    /// The whole Nth request, for asserting on things like the tool list.
    fn request(&self, nth: usize) -> serde_json::Value {
        let bodies = self.all();
        let body = bodies
            .get(nth)
            .unwrap_or_else(|| panic!("this tier was only asked {} time(s)", bodies.len()));
        serde_json::from_str(body).expect("a JSON request body")
    }

    /// Whether anything in the Nth request contains this text.
    fn nth_contains(&self, nth: usize, needle: &str) -> bool {
        self.all()
            .get(nth)
            .map(|body| body.contains(needle))
            .unwrap_or(false)
    }
}

/// A tier whose replies are scripted per call, recording what it was asked.
///
/// The script is indexed by how many times the tier has been called, so one tier
/// can loop first and answer afterwards — which is what a driver needs to do
/// after it has been helped.
struct FakeTier {
    calls: Arc<AtomicUsize>,
    seen: Seen,
    script: Vec<Vec<String>>,
}

impl FakeTier {
    fn new(script: Vec<Vec<String>>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Seen::default(),
            script,
        }
    }

    /// The responder for `with_body_from_request`.
    ///
    /// Returns the scripted frames for this call and records the request. The
    /// last entry in the script is reused if the tier is somehow called more
    /// times than it was scripted for, so a fixture cannot fail with an
    /// unhelpful 501 from the mock server instead of a real assertion.
    fn responder(&self) -> impl Fn(&mockito::Request) -> Vec<u8> + Send + Sync + 'static {
        let calls = Arc::clone(&self.calls);
        let seen = self.seen.clone();
        let script = self.script.clone();
        move |request: &mockito::Request| {
            let body = request
                .utf8_lossy_body()
                .map(|body| body.into_owned())
                .unwrap_or_default();
            seen.record(&body);

            let nth = calls.fetch_add(1, Ordering::SeqCst);
            let frames = script
                .get(nth)
                .or_else(|| script.last())
                .cloned()
                .unwrap_or_default();
            sse(&frames)
        }
    }

    fn seen(&self) -> Seen {
        self.seen.clone()
    }

    /// A tier as the chain wants it: named for people, keyed by its config id.
    fn tier(&self, id: &str, display: &str, url: &str) -> Tier {
        let provider = OpenAiProvider::new(display, format!("{url}/v1"), None);
        Tier::with_id(
            id,
            provider.describe(),
            "test-model",
            Arc::new(provider),
            Limits::default(),
        )
    }

    /// Mount the responder on a mock server, expecting exactly `hits` calls.
    ///
    /// The count is asserted rather than merely recorded, so the fixture is
    /// checked from the server's side too: a turn that quietly stopped calling a
    /// tier would fail here instead of passing on a stale recording.
    async fn mount(&self, server: &mut mockito::Server, hits: usize) -> mockito::Mock {
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body_from_request(self.responder())
            .expect(hits)
            .create_async()
            .await
    }
}

// -------------------------------------------------------------- the harness

async fn drain(rx: &mut UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the turn should not hang")
            .expect("the agent should emit an event");
        let terminal = matches!(
            event,
            AgentEvent::Finished { .. }
                | AgentEvent::Exhausted { .. }
                | AgentEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

/// Drive one prompt through a chain of these fake tiers.
async fn run_turn(workspace: &std::path::Path, chain: FallbackChain) -> Vec<AgentEvent> {
    let (tx, mut rx) = crate::agent::spawn(
        AgentConfig {
            workspace: workspace.to_path_buf(),
            max_steps: DEFAULT_MAX_STEPS,
            cancel: Canceller::default(),
            // Nothing about this fixture is about surviving a restart.
            store: None,
            log: None,
        },
        chain,
        Arc::new(Registry::with_default_tools()),
        Arc::new(AlwaysApprove::default()),
    );

    tx.send(Command::Prompt(GOAL.to_string()))
        .expect("the agent should be listening");
    drain(&mut rx).await
}

fn escalation(events: &[AgentEvent]) -> Option<(String, String, String)> {
    events.iter().find_map(|event| match event {
        AgentEvent::Escalated { from, to, reason } => {
            Some((from.clone(), to.clone(), reason.clone()))
        }
        _ => None,
    })
}

fn consulted(events: &[AgentEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Consulted {
                driver, consultant, ..
            } => Some((driver.clone(), consultant.clone())),
            _ => None,
        })
        .collect()
}

fn spent(events: &[AgentEvent]) -> Vec<(String, Usage)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Spent { tier, usage } => Some((tier.clone(), *usage)),
            _ => None,
        })
        .collect()
}

fn notices(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Notice(message) => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The spending line for a tier, whichever way it was labelled.
fn spend_for<'a>(spent: &'a [(String, Usage)], prefix: &str) -> Option<&'a Usage> {
    spent
        .iter()
        .find(|(tier, _)| tier.starts_with(prefix))
        .map(|(_, usage)| usage)
}

// ------------------------------------------------------------------ fixtures

/// The local tier: writes a file, then loops over the same line.
///
/// Two calls, and the order is the point. The first proves the tool path works
/// end to end — the file really is written, the result really is recorded, and
/// the model really is shown it — and the second is the failure the detector
/// exists to catch, arriving *after* a successful tool round trip rather than
/// instead of one.
fn local_tier() -> FakeTier {
    let writes_note = vec![
        tool_call(
            "write_file",
            serde_json::json!({ "path": "note.txt", "content": NOTE }),
        ),
        stop("tool_calls"),
    ];

    FakeTier::new(vec![writes_note, looping_answer()])
}

/// A tier configured to hand the turn over rather than ask a question first.
///
/// Named rather than inlined because escalating is no longer the default: a
/// scenario that wants the hand-over has to say so, and saying it here keeps the
/// intent visible at the point where the chain is built.
fn escalating(mut tier: Tier) -> Tier {
    tier.on_stuck = OnStuck::Escalate;
    tier
}

// -------------------------------------------------------------------- tests

/// The golden loop, end to end.
///
/// A local endpoint repeats one line until the detector fires; the turn is
/// handed to the frontier, which sees the original question and nothing of the
/// loop; the tool the local tier ran has already changed the disk; and the
/// abandoned attempt is still on the bill.
#[tokio::test]
async fn the_golden_loop() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let local = local_tier();
    let frontier = FakeTier::new(vec![plain_answer("Fixed it.")]);

    let mut local_server = mockito::Server::new_async().await;
    let mut frontier_server = mockito::Server::new_async().await;
    // The local tier is asked twice — the tool call, then the loop — and the
    // frontier once, for the turn it takes over.
    let local_mock = local.mount(&mut local_server, 2).await;
    let frontier_mock = frontier.mount(&mut frontier_server, 1).await;

    let chain = FallbackChain::new(
        vec![
            escalating(local.tier("local", "Local", &local_server.url())),
            frontier.tier("frontier", "Frontier", &frontier_server.url()),
        ],
        true,
    )
    .expect("a chain of two");

    let events = run_turn(workspace.path(), chain).await;

    // 1 and 2: the loop was seen, named, and acted on.
    let (from, to, reason) = escalation(&events).expect("the loop must be escalated");
    assert!(
        from.starts_with("Local"),
        "the tier that looped should be named: {from}"
    );
    assert!(
        to.starts_with("Frontier"),
        "the turn should go to the tier below: {to}"
    );
    assert_eq!(
        reason,
        format!("repeated the same output {REPEATS} times"),
        "the reason should be the detector's own finding, not a paraphrase"
    );

    // 3: the next tier gets the original user turn, not the loop.
    let frontier_seen = frontier.seen();
    assert_eq!(
        frontier_seen.count(),
        1,
        "the frontier should be asked exactly once"
    );
    let handed_over = frontier_seen.messages(0);
    assert_eq!(
        handed_over.len(),
        2,
        "instructions and the user's turn, and nothing else: {handed_over:#?}"
    );
    assert_eq!(handed_over[1]["role"], "user");
    assert_eq!(handed_over[1]["content"], GOAL);
    assert!(
        !frontier_seen.nth_contains(0, LOOPED_LINE),
        "the loop must not reach the next tier"
    );

    // 4: the side effect really happened, and its result really was in the
    // history the local model was shown.
    let written = std::fs::read_to_string(workspace.path().join("note.txt"))
        .expect("the local tier's tool call must have written the file");
    assert_eq!(written, NOTE, "the side effect is not undone by escalating");

    let local_seen = local.seen();
    assert!(
        local_seen.count() >= 2,
        "the local tier should have been asked twice, not {} time(s)",
        local_seen.count()
    );
    let seen_by_local = local_seen.messages(1);
    let tool_result = seen_by_local
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the tool result must be in the history the model was shown");
    assert!(
        tool_result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("note.txt"),
        "the result should be the real one: {tool_result:#?}"
    );
    assert!(
        !frontier_seen.nth_contains(0, NOTE),
        "the abandoned attempt, side effect and all, must not be replayed to the next tier"
    );

    // 5: the abandoned attempt is on the bill.
    let spends = spent(&events);
    let local_spend = spend_for(&spends, "Local").expect("the looped attempt was still billed");
    assert_eq!(
        local_spend.prompt_tokens, 1_500,
        "what the tier reported before it was cut off is owed: {spends:?}"
    );
    let frontier_spend =
        spend_for(&spends, "Frontier").expect("the tier that answered was billed too");
    assert_eq!(frontier_spend.prompt_tokens, 900);

    // And it reaches `/cost`, which is the surface this is all for.
    let mut app = App::new(Config::default());
    for event in events {
        app.handle_agent_event(event);
    }
    let cost = app.describe_cost();
    assert!(
        cost.contains("Local: 1,500 in"),
        "the abandoned attempt should be itemised in /cost:\n{cost}"
    );
    assert!(
        cost.contains("Frontier: 900 in"),
        "and so should the one that answered:\n{cost}"
    );

    // The servers agree with what was asserted above: the looped tier was asked
    // twice and the frontier exactly once, which is the shape of the whole
    // scenario rather than an incidental detail of it.
    local_mock.assert_async().await;
    frontier_mock.assert_async().await;
}

/// A stall where the frontier is asked instead of handed the turn.
///
/// The consultant gets one narrow question and one message; the driver keeps the
/// turn and finishes it with the answer in hand.
#[tokio::test]
async fn a_consult_keeps_the_driver_and_feeds_it_the_answer() {
    let workspace = tempfile::tempdir().expect("tempdir");

    // Loop first; answer once it has been helped.
    let driver = FakeTier::new(vec![
        looping_answer(),
        plain_answer("Tried the HashMap instead."),
    ]);
    let consultant = FakeTier::new(vec![plain_answer("Change the type to u32.")]);

    let mut driver_server = mockito::Server::new_async().await;
    let mut consultant_server = mockito::Server::new_async().await;
    let driver_mock = driver.mount(&mut driver_server, 2).await;
    let consultant_mock = consultant.mount(&mut consultant_server, 1).await;

    let mut local = driver.tier("local", "Local", &driver_server.url());
    local.on_stuck = crate::config::OnStuck::Consult;
    local.consults_per_turn = 1;
    let far = consultant.tier("frontier", "Frontier", &consultant_server.url());

    let chain = FallbackChain::new(vec![local, far], true).expect("a chain of two");
    let events = run_turn(workspace.path(), chain).await;

    let asked = consulted(&events);
    assert_eq!(
        asked.len(),
        1,
        "the driver should have asked the tier below once: {events:?}"
    );
    assert!(
        asked[0].0.starts_with("Local"),
        "the driver is named: {}",
        asked[0].0
    );
    assert!(
        asked[0].1.starts_with("Frontier"),
        "so is the consultant: {}",
        asked[0].1
    );
    assert!(
        escalation(&events).is_none(),
        "a consult must not hand the turn over: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::Finished { .. })),
        "the driver should have finished the turn itself: {:?}",
        events.last()
    );

    // The consultant got one question, and no tools to go with it. That is what
    // makes "the answer is prose" true for an endpoint rather than hoped for.
    let consultant_seen = consultant.seen();
    assert_eq!(consultant_seen.count(), 1);
    let question = consultant_seen.messages(0);
    assert_eq!(
        question.len(),
        1,
        "one question, not a conversation: {question:#?}"
    );
    assert_eq!(question[0]["role"], "user");
    assert!(
        consultant_seen.request(0).get("tools").is_none(),
        "a consultant that could call a tool would act instead of answering"
    );

    // The question is built from what spill already knew, not from the stuck
    // model's account of the problem.
    let asked = question[0]["content"].as_str().unwrap_or_default();
    assert!(asked.contains(GOAL), "the user's own words: {asked}");
    assert!(
        asked.contains(&format!("repeated the same output {REPEATS} times")),
        "the detector's finding, verbatim: {asked}"
    );
    assert!(
        asked.contains(LOOPED_LINE),
        "the repeated output itself is the evidence: {asked}"
    );

    // And the answer reached the driver, which is the whole point of consulting.
    assert_eq!(
        driver.seen().count(),
        2,
        "the driver runs the turn again with the advice"
    );
    assert!(
        driver.seen().nth_contains(1, "Change the type to u32."),
        "the consultant's answer should be in the driver's history"
    );

    driver_mock.assert_async().await;
    consultant_mock.assert_async().await;
}

/// A consult that cannot complete escalates.
///
/// This is the promise the whole fallback rests on: consulting may be the better
/// move, but it must never be the move that strands a turn. The consultant here
/// is down (a 500), so the turn is handed over — and the tier that failed as a
/// consultant then answers as the active tier.
#[tokio::test]
async fn a_consult_that_fails_escalates_rather_than_stranding_the_turn() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let driver = FakeTier::new(vec![looping_answer(), plain_answer("never reached")]);
    // The consultant answers the escalated turn, not the consult.
    let consultant = FakeTier::new(vec![plain_answer("Answered after all.")]);

    let mut driver_server = mockito::Server::new_async().await;
    let mut consultant_server = mockito::Server::new_async().await;
    // The driver loops once and is never retried: the turn leaves it.
    let driver_mock = driver.mount(&mut driver_server, 1).await;

    // Two mocks on one server, in creation order: mockito serves the first
    // non-exhausted match, so the first call is refused and the second succeeds.
    let seen = Seen::default();
    let refused = consultant_server
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("consultant is down")
        .expect(1)
        .create_async()
        .await;
    let capture = seen.clone();
    let answered = consultant_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body_from_request(move |request| {
            if let Ok(body) = request.utf8_lossy_body() {
                capture.record(&body);
            }
            sse(&plain_answer("Answered after all."))
        })
        .expect(1)
        .create_async()
        .await;

    let mut local = driver.tier("local", "Local", &driver_server.url());
    local.on_stuck = crate::config::OnStuck::Consult;
    local.consults_per_turn = 1;
    let far = consultant.tier("frontier", "Frontier", &consultant_server.url());

    let chain = FallbackChain::new(vec![local, far], true).expect("a chain of two");
    let events = run_turn(workspace.path(), chain).await;

    // The failure was reported rather than swallowed, so a consult that did
    // nothing does not look like one that did.
    assert!(
        notices(&events).contains("could not be consulted"),
        "the consult failure should be said out loud: {events:?}"
    );
    assert!(
        consulted(&events).is_empty(),
        "a consult that failed is not a consult: {events:?}"
    );

    // And the turn was handed over, which is the guarantee.
    let (from, to, _) = escalation(&events).expect("a failed consult must escalate");
    assert!(from.starts_with("Local"), "{from}");
    assert!(to.starts_with("Frontier"), "{to}");

    // The consultant was asked twice, but only the second call produced a body
    // to read: the first was the refused consult, whose 500 has no messages.
    assert_eq!(
        seen.count(),
        1,
        "only the answered call should have been captured"
    );
    let escalated_question = seen.messages(0);
    assert_eq!(
        escalated_question[1]["content"], GOAL,
        "the escalated tier gets the original turn: {escalated_question:#?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::Finished { .. })),
        "the turn should have finished on the tier that took it over: {:?}",
        events.last()
    );

    // The server-side counts say the shape was what it should be: the driver was
    // asked once and left, the consultant was refused once and then answered.
    driver_mock.assert_async().await;
    refused.assert_async().await;
    answered.assert_async().await;
}

/// A model stuck against the same wall, spilled on the tighter error budget.
///
/// This is the half a unit test cannot prove: the classifier has to recognise the
/// failure from the message a *real* tool produced out of a *real* `std::io::Error`
/// for a path that really is not there. It is the *same* path each time, so the
/// model is walking into one obstacle rather than discovering several, and the
/// class rule spills at three where the tier's own allowance is four.
#[tokio::test]
async fn a_model_stuck_on_a_missing_file_is_spilled() {
    let workspace = tempfile::tempdir().expect("tempdir");
    // The same absent path three times. A fourth turn exists so the script has
    // somewhere to go if the detector wrongly lets this through.
    let driver = FakeTier::new(vec![
        asks_to_read("missing.rs"),
        asks_to_read("missing.rs"),
        asks_to_read("missing.rs"),
        plain_answer("never reached"),
    ]);
    let frontier = FakeTier::new(vec![plain_answer("Read a different file.")]);

    let mut driver_server = mockito::Server::new_async().await;
    let mut frontier_server = mockito::Server::new_async().await;
    let driver_mock = driver.mount(&mut driver_server, 3).await;
    let frontier_mock = frontier.mount(&mut frontier_server, 1).await;

    let chain = FallbackChain::new(
        vec![
            escalating(driver.tier("local", "Local", &driver_server.url())),
            frontier.tier("frontier", "Frontier", &frontier_server.url()),
        ],
        true,
    )
    .expect("a chain of two");

    let events = run_turn(workspace.path(), chain).await;

    let (from, to, reason) = escalation(&events).expect("the same missing file three times");
    assert!(from.starts_with("Local"), "{from}");
    assert!(to.starts_with("Frontier"), "{to}");
    assert_eq!(
        reason, "read_file failed 3 times with the same error (no such file)",
        "the verdict should name the class, which is what made it a stall at three"
    );

    // The turn is handed over with only the original question: the failed
    // attempt's evidence does not travel.
    let handed_over = frontier.seen().messages(0);
    assert_eq!(handed_over.len(), 2, "{handed_over:#?}");
    assert_eq!(handed_over[1]["content"], GOAL);

    // And the driver really did hit a real missing file three times.
    let third = driver.seen().messages(2);
    let result = third
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the third result must be in the history");
    // The wording around it is the platform's own — Windows says "The system
    // cannot find the file specified" where Unix says "No such file or
    // directory" — so the portable thing to pin is the error number, which is
    // the part that means the same thing on both.
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("(os error 2)"),
        "the real IO error is what the classifier read: {result:#?}"
    );

    driver_mock.assert_async().await;
    frontier_mock.assert_async().await;
}

/// Probing for files that are not there is work, not a stall.
///
/// The counterpart to the fixture above, and the reason it is a separate test:
/// three *different* absent paths are three facts about the filesystem, which is
/// what looking for a `.env` and a `Makefile` looks like. Nothing should spill,
/// and the model should be free to answer.
#[tokio::test]
async fn looking_for_files_that_are_not_there_is_not_a_stall() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let driver = FakeTier::new(vec![
        asks_to_read(".env"),
        asks_to_read("Makefile"),
        asks_to_read("pyproject.toml"),
        plain_answer("None of those exist; here is the plan."),
    ]);
    let frontier = FakeTier::new(vec![plain_answer("never reached")]);

    let mut driver_server = mockito::Server::new_async().await;
    let mut frontier_server = mockito::Server::new_async().await;
    // Four calls: three probes that each miss, then the answer.
    let driver_mock = driver.mount(&mut driver_server, 4).await;
    // And the next tier is never reached, so it is never asked.
    let frontier_mock = frontier.mount(&mut frontier_server, 0).await;

    let chain = FallbackChain::new(
        vec![
            driver.tier("local", "Local", &driver_server.url()),
            frontier.tier("frontier", "Frontier", &frontier_server.url()),
        ],
        true,
    )
    .expect("a chain of two");

    let events = run_turn(workspace.path(), chain).await;

    assert!(
        escalation(&events).is_none(),
        "probing for files must not spill: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::Finished { .. })),
        "the driver should have answered: {:?}",
        events.last()
    );

    // The frontier was never contacted, and the model got to answer.
    assert_eq!(frontier.seen().count(), 0);
    assert_eq!(driver.seen().count(), 4);
    // The answer arrives as an event, not in a request: the fourth request is the
    // conversation *up to* the answer, which is why the request body is not where
    // to look for it.
    let answered: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        answered.contains("None of those exist"),
        "the driver's own answer is what came back: {answered:?}"
    );

    driver_mock.assert_async().await;
    frontier_mock.assert_async().await;
}

/// The class reaches the tier the agent actually runs.
///
/// A unit test can prove the classifier and the resolver agree; only building a
/// real tier proves the two are wired together. `mockito` listens on `127.0.0.1`,
/// so this is the local case by construction.
#[tokio::test]
async fn a_local_endpoint_is_judged_by_the_local_timeouts() {
    let server = mockito::Server::new_async().await;
    let workspace = tempfile::tempdir().expect("tempdir");

    let config = Config::parse(
        std::path::Path::new("test.toml"),
        &format!(
            r#"
            [[tier]]
            id = "on-this-machine"
            kind = "openai"
            base_url = "{}/v1"
            model = "test-model"

            [[tier]]
            id = "out-there"
            kind = "openai"
            base_url = "https://openrouter.ai/api/v1"
            model = "test-model"
            "#,
            server.url()
        ),
    )
    .expect("a valid config");

    let tiers = crate::tiers::build(
        &crate::preset::Library::embedded(),
        &config,
        workspace.path(),
    )
    .await
    .expect("both tiers should build without a network call");

    let local = &tiers[0];
    assert_eq!(
        local.limits.first_token_timeout_ms, 120_000,
        "a model on the LAN gets time to load its weights"
    );
    assert_eq!(
        local.limits.idle_timeout_ms, 30_000,
        "and less patience once it is streaming"
    );

    let hosted = &tiers[1];
    assert_eq!(
        hosted.limits.first_token_timeout_ms, 30_000,
        "a hosted endpoint that is silent for half a minute is broken"
    );
    assert_eq!(hosted.limits.idle_timeout_ms, 60_000);
}

/// A tier that sets one timeout keeps its class's other defaults.
///
/// The case the `Option`-per-field config exists for, proved end to end rather
/// than only in the resolver.
#[tokio::test]
async fn a_local_tier_that_sets_only_one_timeout_keeps_the_rest() {
    let server = mockito::Server::new_async().await;
    let workspace = tempfile::tempdir().expect("tempdir");

    let config = Config::parse(
        std::path::Path::new("test.toml"),
        &format!(
            r#"
            [[tier]]
            id = "on-this-machine"
            kind = "openai"
            base_url = "{}/v1"
            model = "test-model"

            [tier.limits]
            idle_timeout_ms = 5000
            "#,
            server.url()
        ),
    )
    .expect("a valid config");

    let tiers = crate::tiers::build(
        &crate::preset::Library::embedded(),
        &config,
        workspace.path(),
    )
    .await
    .expect("the tier should build");

    assert_eq!(tiers[0].limits.idle_timeout_ms, 5_000, "what was written");
    assert_eq!(
        tiers[0].limits.first_token_timeout_ms, 120_000,
        "and what was not still follows the class, not a global 30s"
    );
}

/// The whole claim, end to end: the real tool wrote it, the real undo put it back.
///
/// A unit test can prove the tool returns the previous bytes and that the undo
/// restores them. Only this proves the two are wired together — that the bytes
/// travelled out of a real tool call made by a real streamed turn, through the
/// real loop, into the state `/undo` reads.
#[tokio::test]
async fn a_write_made_through_the_loop_can_be_put_back() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let note = workspace.path().join("note.txt");
    std::fs::write(&note, "the original contents\n").expect("write");

    let tier = FakeTier::new(vec![
        asks_to_write("note.txt", "junk from the model"),
        plain_answer("Wrote it."),
    ]);
    let mut server = mockito::Server::new_async().await;
    // Two calls: the tool call, then the answer after its result went back.
    let mock = tier.mount(&mut server, 2).await;

    let chain = FallbackChain::new(vec![tier.tier("local", "Local", &server.url())], true)
        .expect("a chain of one");

    let (tx, mut rx) = crate::agent::spawn(
        AgentConfig {
            workspace: workspace.path().to_path_buf(),
            max_steps: DEFAULT_MAX_STEPS,
            cancel: Canceller::default(),
            store: None,
            log: None,
        },
        chain,
        Arc::new(Registry::with_default_tools()),
        Arc::new(AlwaysApprove::default()),
    );

    tx.send(Command::Prompt(GOAL.to_string()))
        .expect("send prompt");
    let _ = drain(&mut rx).await;

    assert_eq!(
        std::fs::read_to_string(&note).expect("read"),
        "junk from the model",
        "the real write_file should have run"
    );

    tx.send(Command::Undo).expect("send undo");
    // `/undo` answers with a single notice and no terminal event, so one message
    // is exactly what to wait for.
    let said = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the undo should answer")
        .expect("an event");

    let AgentEvent::Notice(said) = said else {
        panic!("expected a notice about the undo, got {said:?}");
    };
    assert!(said.contains("restored"), "{said}");
    assert!(said.contains("write_file"), "{said}");
    assert_eq!(
        std::fs::read_to_string(&note).expect("read"),
        "the original contents\n",
        "the junk should be gone and what was there before should be back"
    );

    mock.assert_async().await;
}
