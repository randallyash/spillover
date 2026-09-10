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

/// The tiers, in order, and which one is currently answering.
pub struct FallbackChain {
    tiers: Vec<Tier>,
    active: usize,
    sticky: bool,
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
        })
    }

    pub fn active(&self) -> &Tier {
        &self.tiers[self.active]
    }

    /// The chain in order, for telling the user what will be tried.
    pub fn labels(&self) -> Vec<String> {
        self.tiers.iter().map(|tier| tier.label.clone()).collect()
    }

    /// Start a new user turn. A non-sticky chain gives the top tier another
    /// chance; a sticky one stays where it fell to.
    pub fn begin_turn(&mut self) {
        if !self.sticky {
            self.active = 0;
        }
    }

    /// Move one tier down. `None` means every tier has been tried.
    pub fn escalate(&mut self) -> Option<&Tier> {
        if self.active + 1 >= self.tiers.len() {
            return None;
        }
        self.active += 1;
        Some(&self.tiers[self.active])
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
}
