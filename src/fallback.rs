//! The ordered tiers, and moving down them when one stops working.

use std::sync::Arc;

use crate::config::Limits;
use crate::provider::Provider;

pub struct Tier {
    /// Shown to the user, e.g. "Mock Local (http://127.0.0.1:8731/v1)".
    pub label: String,
    /// The model this tier runs. Each tier names its own, so spilling over can
    /// move between entirely different models.
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub limits: Limits,
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
        })
    }

    pub fn active(&self) -> &Tier {
        &self.tiers[self.active]
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

    /// Whether a tier was chosen by hand rather than reached by falling.
    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    pub fn sticky(&self) -> bool {
        self.sticky
    }

    pub fn set_sticky(&mut self, sticky: bool) {
        self.sticky = sticky;
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
        Tier {
            label: format!("{id} (stub)"),
            model: format!("{id}-model"),
            provider: Arc::new(Stub("stub")),
            limits: Limits::default(),
        }
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
            .map(|index| Tier {
                label: format!("tier {index}"),
                model: "m".to_string(),
                provider: Arc::new(Counting(Arc::clone(&calls))),
                limits: Limits::default(),
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
}
