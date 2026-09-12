//! The ordered tiers, and moving down them when one stops working.

use std::sync::Arc;

use crate::config::{Limits, OnStuck};
use crate::provider::Provider;

pub struct Tier {
    /// The id this tier was configured under, which is what a saved session
    /// refers to it by.
    ///
    /// The label is for people and is derived from the provider (a display name
    /// plus an address), so it is neither unique nor stable; an index is stable
    /// only until somebody reorders their config. The configured id is the one
    /// durable name a tier has.
    pub id: String,
    /// Shown to the user, e.g. "Mock Local (http://127.0.0.1:8731/v1)".
    pub label: String,
    /// The model this tier runs. Each tier names its own, so spilling over can
    /// move between entirely different models.
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub limits: Limits,
    /// What happens when this tier is judged stuck.
    pub on_stuck: OnStuck,
    /// How many times this tier may consult within one turn.
    pub consults_per_turn: u32,
}

impl Tier {
    /// A tier that carries the configured default policy — consult, unless the
    /// configuration says otherwise.
    ///
    /// Most construction — the app, `doctor`, and tests — does not care about the
    /// stuck policy, so this keeps it out of the way of what they do care about.
    /// A test that does care sets the two fields afterwards, which reads better
    /// than threading them through every call site.
    ///
    /// The id defaults to the label, which is enough for a tier that is never
    /// written to a session file. Tiers built from configuration use `with_id`.
    pub fn new(
        label: impl Into<String>,
        model: impl Into<String>,
        provider: Arc<dyn Provider>,
        limits: Limits,
    ) -> Self {
        let label = label.into();
        Self {
            id: label.clone(),
            label,
            model: model.into(),
            provider,
            limits,
            on_stuck: OnStuck::default(),
            consults_per_turn: crate::config::DEFAULT_CONSULTS_PER_TURN,
        }
    }

    /// A tier that knows the id it was configured under, so a saved session can
    /// find it again after a restart.
    pub fn with_id(
        id: impl Into<String>,
        label: impl Into<String>,
        model: impl Into<String>,
        provider: Arc<dyn Provider>,
        limits: Limits,
    ) -> Self {
        Self {
            id: id.into(),
            ..Self::new(label, model, provider, limits)
        }
    }
}

/// The parts of a chain that outlive the process.
///
/// Tiers are named by configured id rather than by position so that reordering
/// `config.toml` between runs does not quietly move the user to a different
/// model. A name that no longer resolves is dropped on restore rather than
/// treated as an error: a tier being removed from the config is an ordinary
/// thing to have happened.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChainState {
    /// The tier that was answering.
    pub active: Option<String>,
    /// The tier the user pinned by hand, if any.
    pub pinned: Option<String>,
    /// Whether a failure keeps the lower tier for the rest of the session.
    pub sticky: bool,
    /// A stuck policy chosen for the session; `None` means each tier's own.
    pub on_stuck: Option<OnStuck>,
}

/// A tier label without its parenthetical detail: "Local (http://…)" becomes
/// "Local".
///
/// The address belongs in the session panel, not in a one-line rail, and the
/// short form is also what a user types to name a tier.
pub fn tier_name(label: &str) -> &str {
    match label.find(" (") {
        Some(cut) => &label[..cut],
        None => label,
    }
}

/// The tiers, in order, and which one is currently answering.
pub struct FallbackChain {
    tiers: Vec<Tier>,
    active: usize,
    sticky: bool,
    /// A tier the user chose by hand, which outranks the fallback policy.
    ///
    /// Without this, naming a tier would only last until the next turn on a
    /// non-sticky chain, and `/tier` would look broken.
    pinned: Option<usize>,
    /// A stuck policy chosen for this session, outranking each tier's own.
    ///
    /// Tiers carry their own from configuration, because the right answer
    /// depends on the model: consulting a hesitant local one is the point, while
    /// a frontier tier has nothing better to ask. That makes the choice worth
    /// trying without editing a file, which is what `/on-stuck` is for.
    on_stuck: Option<OnStuck>,
    /// Whether the next stall should consult whatever the policy says.
    ///
    /// One-shot, and separate from the policy above: this is "try it once, here"
    /// rather than "do this from now on". Consumed when a stall is handled, so a
    /// single request cannot quietly change how the rest of the session behaves.
    consult_next: bool,
}

impl FallbackChain {
    /// `None` when there are no tiers at all, which is a configuration problem
    /// rather than an empty chain to run.
    pub fn new(tiers: Vec<Tier>, sticky: bool) -> Option<Self> {
        if tiers.is_empty() {
            return None;
        }
        Some(Self {
            tiers,
            active: 0,
            sticky,
            pinned: None,
            on_stuck: None,
            consult_next: false,
        })
    }

    pub fn active(&self) -> &Tier {
        &self.tiers[self.active]
    }

    /// The tier to ask when the active one is stuck and consults.
    ///
    /// The next tier in the chain, which is the more capable one by the chain's
    /// own ordering — so consult needs no separate setting for who to ask. The
    /// active tier itself, which is what `active()` returns, is the last resort:
    /// with nothing below, there is nobody to consult.
    pub fn consultant(&self) -> Option<&Tier> {
        self.tiers.get(self.active + 1)
    }

    /// Which tier is answering, as a position in the chain.
    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    /// The chain in order, for telling the user what will be tried.
    pub fn labels(&self) -> Vec<String> {
        self.tiers.iter().map(|tier| tier.label.clone()).collect()
    }

    /// The tiers themselves, for asking each one about the conversation it is
    /// holding.
    pub fn tiers(&self) -> &[Tier] {
        &self.tiers
    }

    /// Whether a tier was chosen by hand rather than reached by falling.
    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    pub fn sticky(&self) -> bool {
        self.sticky
    }

    /// The part of this chain that is worth carrying across a restart.
    pub fn state(&self) -> ChainState {
        ChainState {
            active: self.tiers.get(self.active).map(|tier| tier.id.clone()),
            pinned: self
                .pinned
                .and_then(|index| self.tiers.get(index))
                .map(|tier| tier.id.clone()),
            sticky: self.sticky,
            on_stuck: self.on_stuck,
        }
    }

    /// Put a chain back the way a saved session left it.
    ///
    /// Names that no longer resolve are dropped rather than treated as an error:
    /// a tier having been removed from the configuration is an ordinary thing to
    /// have happened, and refusing to start over it would be worse than
    /// forgetting which one was answering.
    pub fn restore_state(&mut self, state: &ChainState) {
        self.active = state
            .active
            .as_deref()
            .and_then(|id| self.index_of(id))
            .unwrap_or(0);
        self.pinned = state.pinned.as_deref().and_then(|id| self.index_of(id));
        self.sticky = state.sticky;
        self.on_stuck = state.on_stuck;
    }

    /// The position of a tier by its configured id.
    fn index_of(&self, id: &str) -> Option<usize> {
        self.tiers.iter().position(|tier| tier.id == id)
    }

    pub fn set_sticky(&mut self, sticky: bool) {
        self.sticky = sticky;
    }

    /// The stuck policy in force: this session's if one was chosen, otherwise
    /// the answering tier's own.
    pub fn on_stuck(&self) -> OnStuck {
        self.on_stuck.unwrap_or(self.active().on_stuck)
    }

    /// Choose the stuck policy for the rest of the session, or hand the choice
    /// back to each tier's own.
    ///
    /// `None` is not a third policy: it is the absence of one, which is the only
    /// way to get back to a chain where two tiers differ. Every concrete policy
    /// flattens the whole chain to one answer, so without this a session that
    /// tried one could never return to the arrangement its configuration
    /// described.
    pub fn set_on_stuck(&mut self, policy: Option<OnStuck>) {
        self.on_stuck = policy;
    }

    /// Each tier's own policy, for saying what "back to the configuration"
    /// actually means — which differs per tier, and is the point of going back.
    pub fn describe_own_policies(&self) -> String {
        self.tiers
            .iter()
            .map(|tier| format!("{} {}", tier_name(&tier.label), tier.on_stuck))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether the answering tier should consult rather than hand the turn over.
    pub fn consults_when_stuck(&self) -> bool {
        self.on_stuck() == OnStuck::Consult
    }

    /// Ask for the next stall to consult, whatever the policy says.
    pub fn consult_next_stall(&mut self) {
        self.consult_next = true;
    }

    /// Whether such a request is outstanding, and taken rather than read.
    ///
    /// Taking it is what makes it one-shot: a stall either uses it or spends it,
    /// and either way the request does not survive to change the next one.
    pub fn take_consult_request(&mut self) -> bool {
        std::mem::take(&mut self.consult_next)
    }

    /// Whether a consult is even possible, which needs a tier below to ask.
    pub fn can_consult(&self) -> bool {
        self.consultant().is_some()
    }

    /// Choose a tier by hand, and keep choosing it until told otherwise.
    pub fn pin(&mut self, index: usize) -> Option<&Tier> {
        if index >= self.tiers.len() {
            return None;
        }
        self.pinned = Some(index);
        self.active = index;
        Some(&self.tiers[index])
    }

    /// Hand control back to the fallback policy.
    pub fn unpin(&mut self) {
        self.pinned = None;
    }

    /// Resolve what a user typed into a position in the chain.
    ///
    /// Accepts the 1-based number the rail shows, or a tier's name, matched
    /// case-insensitively and by prefix so "deepseek" finds "DeepSeek V4 Flash".
    pub fn resolve(&self, query: &str) -> Option<usize> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }

        if let Ok(number) = query.parse::<usize>() {
            // The rail numbers tiers from one, so that is what typing does.
            if (1..=self.tiers.len()).contains(&number) {
                return Some(number - 1);
            }
            return None;
        }

        let wanted = query.to_lowercase();
        let named = |candidate: &str| tier_name(candidate).to_lowercase();

        // An exact name wins over a prefix, so "local" cannot be stolen by
        // "Local Over There" when both exist.
        self.tiers
            .iter()
            .position(|tier| named(&tier.label) == wanted)
            .or_else(|| {
                self.tiers
                    .iter()
                    .position(|tier| named(&tier.label).starts_with(&wanted))
            })
    }

    /// Start a new user turn.
    ///
    /// A tier the user asked for by name is used until they say otherwise; then
    /// a sticky chain stays where it fell to, and a per-turn one gives the top
    /// tier another chance.
    pub fn begin_turn(&mut self) {
        match self.pinned {
            Some(index) => self.active = index,
            None if self.sticky => {}
            None => self.active = 0,
        }
    }

    /// Move one tier down. `None` means every tier has been tried.
    ///
    /// This is the automatic path, so it releases a hand-picked tier: it has
    /// just failed, and continuing to insist on it would defeat the fallback.
    pub fn escalate(&mut self) -> Option<&Tier> {
        if self.active + 1 >= self.tiers.len() {
            return None;
        }
        self.pinned = None;
        self.active += 1;
        Some(&self.tiers[self.active])
    }

    /// Forget every tier's continued session.
    ///
    /// Used when the conversation itself is replaced — cleared or compacted —
    /// because a CLI tier resuming a session would otherwise continue a
    /// conversation that no longer exists in that form.
    pub fn forget_sessions(&self) {
        for tier in &self.tiers {
            tier.provider.forget_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatRequest, ProviderError, StreamEvent, TurnSummary};
    use crate::session::ChatMessage;
    use async_trait::async_trait;
    use tokio::sync::mpsc::UnboundedSender;

    struct Stub(&'static str);

    #[async_trait]
    impl Provider for Stub {
        fn describe(&self) -> String {
            self.0.to_string()
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _events: UnboundedSender<StreamEvent>,
        ) -> Result<TurnSummary, ProviderError> {
            Ok(TurnSummary::default())
        }
    }

    fn tier(id: &str) -> Tier {
        Tier::new(
            format!("{id} (stub)"),
            format!("{id}-model"),
            Arc::new(Stub("stub")),
            Limits::default(),
        )
    }

    fn chain(ids: &[&str], sticky: bool) -> FallbackChain {
        FallbackChain::new(ids.iter().map(|id| tier(id)).collect(), sticky)
            .expect("a chain needs at least one tier")
    }

    #[test]
    fn a_chain_needs_at_least_one_tier() {
        assert!(FallbackChain::new(Vec::new(), true).is_none());
    }

    #[test]
    fn it_starts_at_the_top() {
        let chain = chain(&["local", "deepseek", "grok"], true);
        assert_eq!(chain.active().label, "local (stub)");
        assert_eq!(
            chain.labels(),
            vec!["local (stub)", "deepseek (stub)", "grok (stub)"]
        );
    }

    #[test]
    fn escalating_walks_down_the_list() {
        let mut chain = chain(&["local", "deepseek", "grok"], true);

        let next = chain.escalate().expect("there is a tier below");
        assert_eq!(next.label, "deepseek (stub)");
        assert_eq!(chain.active().label, "deepseek (stub)");

        let next = chain.escalate().expect("there is a tier below");
        assert_eq!(next.label, "grok (stub)");
        assert!(
            chain.escalate().is_none(),
            "the last tier has nothing below"
        );
    }

    #[test]
    fn escalating_past_the_last_tier_reports_exhaustion_and_stays_put() {
        let mut chain = chain(&["local", "deepseek"], true);
        chain.escalate().expect("one step down");

        assert!(chain.escalate().is_none(), "there is nothing below");
        assert_eq!(
            chain.active().label,
            "deepseek (stub)",
            "it must not move past the end"
        );
    }

    #[test]
    fn a_sticky_chain_does_not_climb_back_up_between_turns() {
        let mut chain = chain(&["local", "deepseek"], true);
        chain.escalate().expect("one step down");

        chain.begin_turn();

        assert_eq!(
            chain.active().label,
            "deepseek (stub)",
            "a sticky chain should stay put"
        );
    }

    #[test]
    fn a_non_sticky_chain_retries_the_top_tier_next_turn() {
        let mut chain = chain(&["local", "deepseek"], false);
        chain.escalate().expect("one step down");

        chain.begin_turn();

        assert_eq!(
            chain.active().label,
            "local (stub)",
            "a per-turn chain should retry tier 1"
        );
    }

    #[test]
    fn labels_come_back_in_order() {
        let chain = chain(&["local", "grok"], true);
        assert_eq!(chain.labels(), vec!["local (stub)", "grok (stub)"]);
    }

    #[test]
    fn the_active_tier_carries_its_own_limits_and_model() {
        let mut first = tier("local");
        first.limits.max_repeat_run = 9;
        let chain = FallbackChain::new(vec![first, tier("grok")], true).expect("a chain");
        assert_eq!(chain.active().limits.max_repeat_run, 9);
        assert_eq!(chain.active().model, "local-model");
    }

    #[test]
    fn a_single_tier_chain_exhausts_immediately() {
        let mut chain = chain(&["only"], true);
        assert!(chain.escalate().is_none());
    }

    #[test]
    fn the_stub_provider_is_wired_up() {
        let chain = chain(&["local"], true);
        assert_eq!(chain.active().provider.describe(), "stub");
        // Proves the chain's provider is usable, not just its metadata.
        assert_eq!(ChatMessage::user("x").content, "x");
    }

    // ---- choosing a tier by hand -----------------------------------------

    #[test]
    fn a_pinned_tier_survives_a_new_turn_on_a_non_sticky_chain() {
        // The whole reason pinning exists: without it, naming a tier would only
        // last until the next message and look broken.
        let mut chain = chain(&["local", "deepseek", "grok"], false);
        chain.pin(2).expect("index 2 exists");

        chain.begin_turn();

        assert_eq!(chain.active().label, "grok (stub)");
        assert!(chain.is_pinned());
    }

    #[test]
    fn unpinning_hands_control_back_to_the_policy() {
        let mut chain = chain(&["local", "deepseek"], false);
        chain.pin(1).expect("index 1 exists");
        chain.unpin();

        chain.begin_turn();

        assert!(!chain.is_pinned());
        assert_eq!(chain.active().label, "local (stub)");
    }

    #[test]
    fn a_fallback_releases_a_hand_picked_tier() {
        // The pinned tier has just failed, so insisting on it would defeat the
        // whole point of falling back.
        let mut chain = chain(&["local", "deepseek", "grok"], true);
        chain.pin(0).expect("index 0 exists");

        chain.escalate().expect("there is a tier below");

        assert!(!chain.is_pinned());
        assert_eq!(chain.active().label, "deepseek (stub)");
    }

    #[test]
    fn pinning_past_the_end_is_refused_rather_than_clamping() {
        let mut chain = chain(&["local", "grok"], true);
        assert!(chain.pin(5).is_none());
        assert_eq!(chain.active().label, "local (stub)", "it must not move");
        assert!(!chain.is_pinned());
    }

    #[test]
    fn a_tier_can_be_named_by_the_number_the_rail_shows() {
        let chain = chain(&["local", "deepseek", "grok"], true);
        // One-based, to match "1. Local → 2. DeepSeek" in the rail.
        assert_eq!(chain.resolve("1"), Some(0));
        assert_eq!(chain.resolve("3"), Some(2));
        // Out of range is not silently the last tier.
        assert_eq!(chain.resolve("0"), None);
        assert_eq!(chain.resolve("4"), None);
    }

    #[test]
    fn a_tier_can_be_named_by_its_name() {
        let chain = chain(
            &[
                "Local (http://10.0.0.1:1234/v1)",
                "DeepSeek V4 Flash",
                "Grok",
            ],
            true,
        );
        assert_eq!(chain.resolve("local"), Some(0));
        assert_eq!(chain.resolve("DeepSeek V4 Flash"), Some(1));
        assert_eq!(
            chain.resolve("DeepSeek"),
            Some(1),
            "a prefix should find it"
        );
        assert_eq!(chain.resolve("grok"), Some(2));
        assert_eq!(chain.resolve("nothing like this"), None);
    }

    #[test]
    fn an_exact_name_beats_a_prefix() {
        let chain = chain(&["Local", "Local Over There"], true);
        assert_eq!(
            chain.resolve("Local"),
            Some(0),
            "the exact match must win over the longer name that starts the same"
        );
    }

    #[test]
    fn a_tier_name_stops_at_its_address() {
        assert_eq!(tier_name("Local (http://localhost:1234/v1)"), "Local");
        assert_eq!(tier_name("Grok Build"), "Grok Build");
        assert_eq!(tier_name(""), "");
    }

    #[test]
    fn the_sticky_policy_can_be_changed_at_runtime() {
        let mut chain = chain(&["local", "grok"], false);
        assert!(!chain.sticky());

        chain.set_sticky(true);
        chain.escalate().expect("one step down");
        chain.begin_turn();

        assert!(
            chain.sticky(),
            "the change should be in effect for the rest of the session"
        );
        assert_eq!(chain.active().label, "grok (stub)");
    }

    #[test]
    fn the_chain_reports_its_own_shape() {
        let chain = chain(&["local", "deepseek", "grok"], true);
        assert_eq!(chain.len(), 3);
        assert_eq!(chain.active_index(), 0);
    }

    // ---- the stuck policy ------------------------------------------------

    #[test]
    fn the_stuck_policy_comes_from_the_answering_tier_until_it_is_chosen() {
        // Tiers carry their own, because the right answer depends on the model.
        let mut first = Tier::new(
            "local".to_string(),
            "m".to_string(),
            Arc::new(Stub("stub")),
            Limits::default(),
        );
        first.on_stuck = OnStuck::Consult;
        let mut second = Tier::new(
            "grok".to_string(),
            "m".to_string(),
            Arc::new(Stub("stub")),
            Limits::default(),
        );
        // Named rather than left to the default, because differing from the tier
        // above is the whole point: consult on the tier that is doing the work,
        // hand the turn over on the one that has to finish it.
        second.on_stuck = OnStuck::Escalate;
        let mut chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        assert!(chain.consults_when_stuck(), "the local tier consults");

        // And it follows the tier, not the chain: the frontier tier escalates.
        chain.escalate().expect("one step down");
        assert!(!chain.consults_when_stuck(), "grok escalates");
    }

    #[test]
    fn a_chosen_policy_outranks_every_tier_for_the_session() {
        let mut first = Tier::new(
            "local".to_string(),
            "m".to_string(),
            Arc::new(Stub("stub")),
            Limits::default(),
        );
        first.on_stuck = OnStuck::Escalate;
        let second = Tier::new(
            "grok".to_string(),
            "m".to_string(),
            Arc::new(Stub("stub")),
            Limits::default(),
        );
        let mut chain = FallbackChain::new(vec![first, second], true).expect("a chain");

        assert!(!chain.consults_when_stuck());
        chain.set_on_stuck(Some(OnStuck::Consult));

        assert!(
            chain.consults_when_stuck(),
            "the choice wins over the config"
        );

        // On the tier below too, so one choice covers the chain.
        chain.escalate().expect("one step down");
        assert!(chain.consults_when_stuck());
    }

    #[test]
    fn a_one_shot_consult_request_is_taken_rather_than_read() {
        // Taking it is what keeps it one-shot: a request that could be read
        // twice would quietly become the policy for the rest of the session.
        //
        // The tier is set to escalate on purpose. Consulting is the default now,
        // so a chain left alone could not tell "the one-shot changed the policy"
        // from "the policy was already consult" — and telling those two apart is
        // this test's entire job.
        let mut tiers = vec![tier("local"), tier("grok")];
        tiers[0].on_stuck = OnStuck::Escalate;
        let mut chain = FallbackChain::new(tiers, true).expect("a chain");

        assert!(!chain.take_consult_request(), "nothing was asked for");

        chain.consult_next_stall();
        assert!(chain.take_consult_request(), "the first stall takes it");
        assert!(
            !chain.take_consult_request(),
            "and the next stall does not get it"
        );
        assert!(
            !chain.consults_when_stuck(),
            "asking once must not change the policy"
        );
    }

    #[test]
    fn a_consult_needs_somewhere_to_ask() {
        let chain = chain(&["local", "grok"], true);
        assert!(chain.can_consult(), "there is a tier below");

        let only = chain_of_one();
        assert!(
            !only.can_consult(),
            "the last tier has nobody to ask, so it escalates instead"
        );
    }

    /// A chain of exactly one tier, which is the last tier in any chain.
    fn chain_of_one() -> FallbackChain {
        FallbackChain::new(
            vec![Tier::new(
                "only".to_string(),
                "m".to_string(),
                Arc::new(Stub("stub")),
                Limits::default(),
            )],
            true,
        )
        .expect("a chain")
    }

    #[test]
    fn forgetting_sessions_reaches_every_tier() {
        // A recording provider that counts how often it was told to forget.
        struct Counting(Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait]
        impl Provider for Counting {
            fn describe(&self) -> String {
                "counting".to_string()
            }

            async fn stream(
                &self,
                _request: ChatRequest,
                _events: UnboundedSender<StreamEvent>,
            ) -> Result<TurnSummary, ProviderError> {
                Ok(TurnSummary::default())
            }

            fn forget_session(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tiers = (0..3)
            .map(|index| {
                Tier::new(
                    format!("tier {index}"),
                    "m".to_string(),
                    Arc::new(Counting(Arc::clone(&calls))),
                    Limits::default(),
                )
            })
            .collect();

        let chain = FallbackChain::new(tiers, true).expect("a chain");
        chain.forget_sessions();

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "every tier must be told, or one would resume a discarded conversation"
        );
    }

    /// A tier whose configured id is short and distinct from its label, which
    /// is the case a saved session has to name it by.
    fn named_tier(id: &str) -> Tier {
        Tier::with_id(
            id,
            format!("{id} (stub)"),
            format!("{id}-model"),
            Arc::new(Stub("stub")),
            Limits::default(),
        )
    }

    #[test]
    fn a_chain_is_saved_and_restored_by_name_rather_than_position() {
        // The whole point of naming tiers by id: reordering the config between
        // runs must not quietly move the user onto a different model.
        let mut chain = FallbackChain::new(
            vec![
                named_tier("local"),
                named_tier("deepseek"),
                named_tier("grok"),
            ],
            true,
        )
        .expect("a chain");

        chain.set_on_stuck(Some(OnStuck::Consult));
        chain.pin(2).expect("pin the third tier");
        let state = chain.state();

        assert_eq!(state.active.as_deref(), Some("grok"));
        assert_eq!(state.pinned.as_deref(), Some("grok"));
        assert!(state.sticky);
        assert_eq!(state.on_stuck, Some(OnStuck::Consult));

        // The same tiers, in a different order.
        let mut reordered = FallbackChain::new(
            vec![
                named_tier("grok"),
                named_tier("local"),
                named_tier("deepseek"),
            ],
            false,
        )
        .expect("a chain");
        reordered.restore_state(&state);

        assert_eq!(
            reordered.active().id,
            "grok",
            "still the tier that was answering"
        );
        assert!(
            reordered.is_pinned(),
            "a hand-picked tier is still hand-picked"
        );
        assert_eq!(reordered.on_stuck(), OnStuck::Consult);
        assert!(reordered.sticky(), "the session's sticky choice came back");
    }

    #[test]
    fn a_tier_that_no_longer_exists_is_forgotten_rather_than_fatal() {
        // Removing a tier from the config is an ordinary thing to have happened;
        // refusing to start over it would be worse than forgetting which one it
        // was.
        let mut chain = FallbackChain::new(vec![named_tier("local")], true).expect("a chain");
        let state = ChainState {
            active: Some("retired".to_string()),
            pinned: Some("also-retired".to_string()),
            sticky: false,
            on_stuck: Some(OnStuck::Escalate),
        };

        chain.restore_state(&state);

        assert_eq!(
            chain.active().id,
            "local",
            "it falls back to the first tier"
        );
        assert!(
            !chain.is_pinned(),
            "a pin on a tier that is gone is dropped"
        );
        assert!(!chain.sticky());
    }

    #[test]
    fn a_chain_with_nothing_saved_restores_to_its_first_tier() {
        let mut chain = FallbackChain::new(vec![named_tier("local"), named_tier("grok")], true)
            .expect("a chain");

        chain.pin(1).expect("pin");
        chain.restore_state(&ChainState::default());

        assert_eq!(chain.active_index(), 0);
        assert!(!chain.is_pinned());
        assert!(
            !chain.sticky(),
            "the empty state says what the file said, and the file has no opinion"
        );
    }
}
