//! `spill doctor`: what spill can and cannot reach right now, in one report.
//!
//! Written to be pasted into a bug report, and to answer "why is it not
//! working?" without anyone having to trace a request by hand. Secrets are
//! never printed — only the *name* of the variable a key would come from.

use std::time::Instant;

use serde_json::json;

use crate::config::{Config, Limits, OnStuck, TierClass, TierKind};
use crate::preset::{Library, cli_spec, openai_settings};
use crate::setup::probe;

#[derive(Debug, Clone)]
pub struct TierReport {
    pub id: String,
    pub name: String,
    /// `openai` or `cli`.
    pub kind: String,
    /// The URL or the binary this tier would use.
    pub target: String,
    pub reachable: bool,
    pub detail: String,
    pub milliseconds: u128,
    /// What this tier does when it gets stuck.
    ///
    /// Reported because consult is opt-in and its whole argument is that a
    /// choice was made: a diagnostic that cannot show which tiers consult cannot
    /// help anyone decide whether it is working.
    pub on_stuck: OnStuck,
    /// Which timeouts this tier is judged by, once its class and its own
    /// overrides are applied.
    ///
    /// Reported because the class is otherwise invisible: when a slow local model
    /// is spilled over, the first question is whether it was given the local
    /// grace period, and nothing else on screen answers it.
    pub limits: Limits,
    pub class: TierClass,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub tiers: Vec<TierReport>,
    pub notes: Vec<String>,
    pub version: String,
}

impl Report {
    /// Whether spill could answer anything at all with this configuration.
    pub fn usable(&self) -> bool {
        self.tiers.iter().any(|tier| tier.reachable)
    }

    pub fn exit_code(&self) -> i32 {
        if self.usable() { 0 } else { 1 }
    }

    /// The plain-text report.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("spill {} — doctor\n\n", self.version));

        if self.tiers.is_empty() {
            out.push_str("No tiers are configured. Run `spill setup` to choose some.\n");
            if !self.notes.is_empty() {
                out.push('\n');
                out.push_str(&self.render_notes());
            }
            return out.trim_end().to_string();
        }

        let width = self
            .tiers
            .iter()
            .map(|tier| tier.id.len())
            .max()
            .unwrap_or(4)
            .max(4);

        for tier in &self.tiers {
            let mark = if tier.reachable { "ok  " } else { "FAIL" };
            out.push_str(&format!(
                "{mark}  {:<width$}  {:<6}  {}\n",
                tier.id,
                tier.kind,
                tier.target,
                width = width
            ));
            out.push_str(&format!(
                "      {:<width$}  {}  ({} ms)\n",
                "",
                tier.detail,
                tier.milliseconds,
                width = width
            ));
            // Only when it is not the default. A line reading "consult" under
            // every tier of every ordinary configuration is noise; the whole
            // reason to print it is that somebody chose something else — which,
            // now that consulting is the default, means escalating.
            if tier.on_stuck != OnStuck::default() {
                out.push_str(&format!(
                    "      {:<width$}  on stuck: {}\n",
                    "",
                    tier.on_stuck,
                    width = width
                ));
            }
            // And only for a local tier, for the same reason: the hosted numbers
            // were what every tier got before classes existed, so printing them
            // everywhere would say nothing. A local tier is the case where the
            // answer is not what you would have guessed.
            if tier.class == TierClass::Local {
                out.push_str(&format!(
                    "      {:<width$}  limits: {} first token · {} idle (local)\n",
                    "",
                    seconds(tier.limits.first_token_timeout_ms),
                    seconds(tier.limits.idle_timeout_ms),
                    width = width
                ));
            }
        }

        let working = self.tiers.iter().filter(|tier| tier.reachable).count();
        out.push_str(&format!(
            "\n{working} of {} tier(s) usable. ",
            self.tiers.len()
        ));
        if self.usable() {
            out.push_str("spill will start on the first one that answers.\n");
        } else {
            out.push_str("spill has nowhere to send a prompt until one of these works.\n");
        }

        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str(&self.render_notes());
        }

        out.trim_end().to_string()
    }

    fn render_notes(&self) -> String {
        let mut out = String::from("worth knowing:\n");
        for note in &self.notes {
            out.push_str("  - ");
            out.push_str(note);
            out.push('\n');
        }
        out
    }

    /// The same report for a script to read.
    pub fn to_json(&self) -> String {
        let payload = json!({
            "version": self.version,
            "usable": self.usable(),
            "tiers": self.tiers.iter().map(|tier| json!({
                "id": tier.id,
                "name": tier.name,
                "kind": tier.kind,
                "target": tier.target,
                "reachable": tier.reachable,
                "detail": tier.detail,
                "milliseconds": tier.milliseconds,
                "onStuck": tier.on_stuck.to_string(),
                // Carried for every tier, not only the local ones: prose should
                // show what is notable, machine output should be complete.
                "class": tier.class.label(),
                "firstTokenTimeoutMs": tier.limits.first_token_timeout_ms,
                "idleTimeoutMs": tier.limits.idle_timeout_ms,
            })).collect::<Vec<_>>(),
            "notes": self.notes,
        });
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    /// The check's message often opens with the target, which the report
    /// already shows in its own column.
    fn detail_of(detail: &str, target: &str) -> String {
        match detail.strip_prefix(target) {
            Some(rest) => rest
                .trim_start_matches([':', ' ', '—', '-'])
                .trim_start()
                .to_string(),
            None => detail.to_string(),
        }
    }

    fn target_of(library: &Library, tier: &crate::config::Tier) -> String {
        match tier.kind {
            TierKind::OpenAi => openai_settings(library, tier)
                .map(|settings| settings.base_url)
                .unwrap_or_else(|error| format!("(unresolved: {error})")),
            TierKind::Cli => cli_spec(library, tier)
                .map(|spec| spec.bin)
                .unwrap_or_else(|error| format!("(unresolved: {error})")),
        }
    }

    fn name_of(tier: &crate::config::Tier) -> String {
        tier.display_name().to_string()
    }
}

/// A millisecond budget as the seconds a person would say, so the report reads
/// "120s" rather than "120000".
fn seconds(milliseconds: u64) -> String {
    if milliseconds % 1_000 == 0 {
        format!("{}s", milliseconds / 1_000)
    } else {
        format!("{}ms", milliseconds)
    }
}

/// Check every configured tier, one after another, and collect what happened.
pub async fn diagnose(library: &Library, config: &Config) -> Report {
    let mut tiers = Vec::new();

    for tier in &config.tiers {
        let started = Instant::now();
        let readiness = probe::check(library, tier).await;
        let elapsed = started.elapsed();

        let target = Report::target_of(library, tier);
        let detail = Report::detail_of(&readiness.detail, &target);
        // Asked of the same function the agent builds with, so a report can never
        // describe timeouts that differ from the ones in force.
        let class = crate::tiers::class_of(library, tier);

        tiers.push(TierReport {
            id: tier.id.clone(),
            name: Report::name_of(tier),
            kind: tier.kind.to_string(),
            target,
            reachable: readiness.ok,
            detail,
            milliseconds: elapsed.as_millis(),
            on_stuck: tier.on_stuck,
            limits: tier.limits.resolve(class),
            class,
        });
    }

    Report {
        tiers,
        notes: crate::tiers::notes(library, config),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn config(text: &str) -> Config {
        Config::parse(Path::new("test.toml"), text).expect("valid")
    }

    fn report(tiers: Vec<TierReport>) -> Report {
        Report {
            tiers,
            notes: Vec::new(),
            version: "0.0.0".to_string(),
        }
    }

    fn tier(id: &str, reachable: bool) -> TierReport {
        TierReport {
            id: id.to_string(),
            name: id.to_string(),
            kind: "openai".to_string(),
            target: format!("http://example.invalid/{id}"),
            reachable,
            detail: if reachable {
                "would use a-model".to_string()
            } else {
                "could not reach it".to_string()
            },
            milliseconds: 12,
            on_stuck: OnStuck::default(),
            limits: Limits::default(),
            class: TierClass::Hosted,
        }
    }

    /// The same, consulting when stuck.
    fn escalating_tier(id: &str) -> TierReport {
        TierReport {
            on_stuck: OnStuck::Escalate,
            ..tier(id, true)
        }
    }

    #[test]
    fn a_detail_repeating_the_target_is_trimmed() {
        assert_eq!(
            Report::detail_of(
                "http://localhost:1234/v1 would use qwen3",
                "http://localhost:1234/v1"
            ),
            "would use qwen3"
        );
        assert_eq!(
            Report::detail_of("grok is installed", "grok"),
            "is installed"
        );
    }

    #[test]
    fn a_detail_about_something_else_is_left_alone() {
        assert_eq!(
            Report::detail_of("could not reach http://x/v1/models", "http://x/v1"),
            "could not reach http://x/v1/models"
        );
    }

    #[test]
    fn a_report_is_usable_if_any_tier_is() {
        assert!(report(vec![tier("a", false), tier("b", true)]).usable());
        assert!(!report(vec![tier("a", false)]).usable());
        assert!(!report(Vec::new()).usable());
    }

    #[test]
    fn the_exit_code_follows_usability() {
        assert_eq!(report(vec![tier("a", true)]).exit_code(), 0);
        assert_eq!(report(vec![tier("a", false)]).exit_code(), 1);
        assert_eq!(report(Vec::new()).exit_code(), 1);
    }

    #[test]
    fn the_text_report_shows_each_tier_and_what_became_of_it() {
        let text = report(vec![tier("local", true), tier("grok", false)]).render();

        assert!(text.contains("local"), "{text}");
        assert!(text.contains("ok"), "{text}");
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("grok"), "{text}");
        assert!(text.contains("could not reach it"), "{text}");
        assert!(text.contains("1 of 2 tier(s) usable"), "{text}");
    }

    #[test]
    fn a_config_where_nothing_works_says_so_plainly() {
        let text = report(vec![tier("local", false)]).render();
        assert!(
            text.contains("nowhere to send a prompt"),
            "the consequence should be stated: {text}"
        );
    }

    #[test]
    fn a_working_config_says_spill_will_start() {
        let text = report(vec![tier("local", true)]).render();
        assert!(text.contains("will start"), "{text}");
    }

    #[test]
    fn an_empty_config_points_at_setup() {
        let text = report(Vec::new()).render();
        assert!(text.contains("spill setup"), "{text}");
    }

    #[test]
    fn notes_are_shown_in_the_text_report() {
        let mut report = report(vec![tier("local", true)]);
        report.notes.push("a key is not set".to_string());

        let text = report.render();
        assert!(text.contains("worth knowing"), "{text}");
        assert!(text.contains("a key is not set"), "{text}");
    }

    #[test]
    fn the_json_report_is_parseable_and_carries_the_same_facts() {
        let mut report = report(vec![tier("local", true), tier("grok", false)]);
        report.notes.push("something to know".to_string());

        let parsed: serde_json::Value =
            serde_json::from_str(&report.to_json()).expect("the report must be valid JSON");

        assert_eq!(parsed["usable"], true);
        assert_eq!(parsed["version"], "0.0.0");
        assert_eq!(parsed["tiers"].as_array().expect("tiers").len(), 2);
        assert_eq!(parsed["tiers"][0]["id"], "local");
        assert_eq!(parsed["tiers"][0]["reachable"], true);
        assert_eq!(parsed["tiers"][1]["reachable"], false);
        assert_eq!(parsed["tiers"][0]["milliseconds"], 12);
        assert_eq!(parsed["notes"][0], "something to know");
    }

    #[test]
    fn json_from_an_empty_report_is_still_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(&report(Vec::new()).to_json()).expect("valid JSON");
        assert_eq!(parsed["usable"], false);
        assert_eq!(parsed["tiers"].as_array().expect("tiers").len(), 0);
    }

    #[tokio::test]
    async fn a_dead_endpoint_is_diagnosed_without_erroring_out() {
        let library = Library::embedded();
        let report = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "dead"
                name = "Not running"
                kind = "openai"
                base_url = "http://127.0.0.1:9/v1"
                model = "m"
                "#,
            ),
        )
        .await;

        assert_eq!(report.tiers.len(), 1);
        assert!(!report.tiers[0].reachable);
        assert_eq!(report.tiers[0].id, "dead");
        assert_eq!(report.tiers[0].kind, "openai");
        assert_eq!(report.tiers[0].target, "http://127.0.0.1:9/v1");
        assert!(!report.usable());
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test]
    async fn a_live_endpoint_is_diagnosed_as_working() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"qwen3-coder"}]}"#)
            .create_async()
            .await;

        let library = Library::embedded();
        let report = diagnose(
            &library,
            &config(&format!(
                r#"
                [[tier]]
                id = "local"
                kind = "openai"
                base_url = "{}/v1"
                model = ""
                "#,
                server.url()
            )),
        )
        .await;

        assert!(report.usable());
        assert_eq!(report.exit_code(), 0);
        assert!(
            report.tiers[0].detail.contains("qwen3-coder"),
            "{:?}",
            report.tiers[0]
        );
    }

    #[tokio::test]
    async fn a_missing_cli_is_diagnosed_without_running_it() {
        let library = Library::embedded();
        let report = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "ghost"
                kind = "cli"
                bin = "definitely-not-a-real-cli-xyz"
                args = ["-p", "{prompt}"]
                "#,
            ),
        )
        .await;

        assert!(!report.usable());
        assert_eq!(report.tiers[0].target, "definitely-not-a-real-cli-xyz");
        // Both the tier line and the notes should mention it.
        let text = report.render();
        assert!(text.contains("not on PATH"), "{text}");
    }

    #[tokio::test]
    async fn every_configured_tier_appears_even_when_one_fails() {
        let library = Library::embedded();
        let (bin, flag) = crate::provider::cli::portable_shell();
        let report = diagnose(
            &library,
            &config(&format!(
                r#"
                [[tier]]
                id = "dead"
                kind = "openai"
                base_url = "http://127.0.0.1:9/v1"
                model = "m"

                [[tier]]
                id = "shell"
                kind = "cli"
                bin = "{bin}"
                args = ["{flag}", "{{prompt}}"]
                "#
            )),
        )
        .await;

        assert_eq!(report.tiers.len(), 2, "a failure must not hide the rest");
        assert_eq!(report.tiers[0].id, "dead");
        assert_eq!(report.tiers[1].id, "shell");
        assert!(report.tiers[1].reachable);
    }

    #[test]
    fn a_tier_that_escalates_when_stuck_says_so() {
        // A tier that hands the turn over is the one that departs from the
        // default, and the departure is what is worth printing: it means the
        // whole turn goes to the tier below and the session stays there.
        let text = report(vec![tier("local", true), escalating_tier("helper")]).render();

        assert!(text.contains("on stuck: escalate"), "{text}");
        // And named against the tier it belongs to, not floating at the end.
        let helper = text
            .lines()
            .position(|line| line.contains("helper"))
            .expect("the helper tier");
        // The header, the detail, then the policy: three lines belong to a tier
        // whose policy is worth printing.
        assert!(
            text.lines()
                .skip(helper)
                .take(3)
                .any(|line| line.contains("on stuck: escalate")),
            "the policy should read as that tier's: {text}"
        );
    }

    #[test]
    fn the_default_policy_is_not_printed_at_all() {
        // A line reading "consult" under every tier of every ordinary
        // configuration is noise; the reason to print it is that someone chose
        // something other than the default.
        let text = report(vec![tier("local", true), tier("grok", false)]).render();
        assert!(!text.contains("on stuck"), "{text}");
    }

    #[test]
    fn the_json_always_carries_the_policy_even_when_it_is_the_default() {
        // A machine reading the report should not have to infer a field from its
        // absence, which is why this differs from the text.
        let report = report(vec![tier("local", true), escalating_tier("helper")]);
        let parsed: serde_json::Value =
            serde_json::from_str(&report.to_json()).expect("valid JSON");

        assert_eq!(parsed["tiers"][0]["onStuck"], "consult", "the default");
        assert_eq!(parsed["tiers"][1]["onStuck"], "escalate");
    }

    #[tokio::test]
    async fn a_configured_policy_is_reported_for_a_real_config() {
        let library = Library::embedded();
        let report = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "local"
                kind = "openai"
                base_url = "http://127.0.0.1:9/v1"
                model = "m"
                on_stuck = "escalate"
                "#,
            ),
        )
        .await;

        assert_eq!(report.tiers[0].on_stuck, OnStuck::Escalate);
        assert!(
            report.render().contains("on stuck: escalate"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn secrets_are_never_printed_only_variable_names() {
        let mut report = report(vec![tier("hosted", false)]);
        report.notes.push(
            "tier \"hosted\" reads its key from $OPENROUTER_API_KEY, which is not set".into(),
        );

        let text = report.render();
        let json = report.to_json();

        assert!(text.contains("$OPENROUTER_API_KEY"), "{text}");
        assert!(json.contains("$OPENROUTER_API_KEY"), "{json}");
        // A key would look like this; nothing resembling one may appear.
        assert!(!text.contains("sk-"), "{text}");
        assert!(!json.contains("sk-"), "{json}");
    }
}
