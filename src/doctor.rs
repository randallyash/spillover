//! `spill doctor`: what spill can and cannot reach right now, in one report.
//!
//! Written to be pasted into a bug report, and to answer "why is it not
//! working?" without anyone having to trace a request by hand. Secrets are
//! never printed — only the *name* of the variable a key would come from.

use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;

use crate::allow::AllowRules;
use crate::config::{Config, Limits, OnStuck, Origin, TierClass, TierKind};
use crate::preset::{Library, cli_spec, openai_settings};
use crate::provider::cli::{CliSpec, which};
use crate::setup::probe;
use crate::text::pack;

/// What a `cli` tier resolved to, which is the half of it that is not visible
/// anywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliDetail {
    /// Where the binary actually is.
    pub bin_path: Option<PathBuf>,
    /// Whether this tier continues a session between turns, and with which
    /// flags — empty means the whole transcript is sent every turn instead.
    pub resume_args: Vec<String>,
    /// Flags that open the session, when spill is the one choosing its id.
    /// Empty means the CLI mints its own and reports it.
    pub session_args: Vec<String>,
    /// Whether the CLI can be consulted at all, which needs a read-only mode
    /// spill can launch it with. A CLI without one hands the turn over instead.
    pub read_only_args: Vec<String>,
}

impl CliDetail {
    /// Whether turn-to-turn session continuity is on for this tier.
    pub fn continues_sessions(&self) -> bool {
        !self.resume_args.is_empty()
    }

    /// Whether the CLI names its own session, so spill has no id to pass.
    pub fn captures_session(&self) -> bool {
        self.continues_sessions() && self.session_args.is_empty()
    }
}

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
    pub on_stuck: OnStuck,
    /// Which timeouts this tier is judged by, once its class and its own
    /// overrides are applied.
    pub limits: Limits,
    pub class: TierClass,
    /// Present for a `cli` tier whose spec resolved.
    pub cli: Option<CliDetail>,
}

/// The model credentials removed from a delegated CLI's environment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Credentials {
    /// Every name spill removes before starting a delegated CLI.
    pub removed: Vec<&'static str>,
    /// Those set in this environment right now: the ones the removal is doing
    /// something about, and the ones whose absence from a CLI would explain it
    /// failing to find a key.
    pub set: Vec<&'static str>,
}

/// What this machine offers the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Columns and rows, when there is a terminal to ask. `None` when there is
    /// not, which is the ordinary case for a report on its way to a bug tracker.
    pub terminal: Option<(u16, u16)>,
    /// The colour depth the environment advertises: 8, 256, or truecolor.
    pub colours: u16,
    /// Whether `NO_COLOR` asks for none, per its specification.
    pub no_color: bool,
}

impl Machine {
    /// Ask the machine, which is only ever done once, by `diagnose`.
    fn detect() -> Self {
        Self {
            terminal: crossterm::terminal::size().ok(),
            colours: crossterm::style::available_color_count(),
            no_color: std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        }
    }

    /// The terminal as a person would describe it.
    fn terminal_line(&self) -> String {
        match self.terminal {
            Some((columns, rows)) => format!("{columns}x{rows}"),
            None => "not a terminal (the output is piped)".to_string(),
        }
    }

    /// The colour depth, and whether it will be used.
    fn colour_line(&self) -> String {
        let depth = if self.colours > 256 {
            "truecolor"
        } else if self.colours == 256 {
            "256 colours"
        } else {
            "8 colours"
        };

        if self.no_color {
            format!("{depth}, but NO_COLOR is set so none of them are used")
        } else {
            depth.to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub tiers: Vec<TierReport>,
    pub notes: Vec<String>,
    pub version: String,
    /// Which file was read, and how it was found.
    pub origin: Origin,
    pub workspace: String,
    pub sticky_fallback: bool,
    pub max_steps: usize,
    pub shell_timeout_secs: u64,
    /// Present only when there is a `cli` tier to start: with none, nothing is
    /// removed from anything.
    pub credentials: Option<Credentials>,
    pub machine: Machine,
    /// Which shell commands run without being asked about.
    pub allow: AllowRules,
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
        out.push_str(&self.render_config());
        out.push('\n');

        if self.tiers.is_empty() {
            out.push_str("No tiers are configured. Run `spill setup` to choose some.\n");
            out.push('\n');
            out.push_str(&self.render_machine());
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
            if let Some(cli) = &tier.cli {
                for line in cli_lines(cli) {
                    out.push_str(&format!("      {:<width$}  {line}\n", "", width = width));
                }
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

        if let Some(credentials) = &self.credentials {
            out.push('\n');
            out.push_str(&render_credentials(credentials));
        }

        // Only when it is not what a configuration saying nothing would give,
        // on the same reasoning as the stuck policy above: twenty-three lines
        // reciting the defaults would drown the tier list they sit under.
        if !self.allow.is_default() {
            out.push('\n');
            out.push_str(&self.render_allow());
        }

        out.push('\n');
        out.push_str(&self.render_machine());

        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str(&self.render_notes());
        }

        out.trim_end().to_string()
    }

    /// Which file is in force, and how it was chosen.
    fn render_config(&self) -> String {
        let mut out = String::new();
        let (path, how) = match &self.origin {
            Origin::Given(path) => (Some(path), "named by --config"),
            Origin::Found(path) => (Some(path), "found at the default path"),
            Origin::Missing(path) => (
                Some(path),
                "not there yet, so the built-in defaults are in force",
            ),
            Origin::Text => (None, "not read from a file"),
        };

        let head = match path {
            Some(path) => path.display().to_string(),
            None => "(no file)".to_string(),
        };
        out.push_str(&format!(
            "{:<width$} {head}\n",
            "config",
            width = LABEL_WIDTH
        ));
        out.push_str(&format!(
            "{:width$} {how} · workspace {} · sticky fallback {} · {} steps · shell {}s\n",
            "",
            self.workspace,
            if self.sticky_fallback { "on" } else { "off" },
            self.max_steps,
            self.shell_timeout_secs,
            width = LABEL_WIDTH
        ));
        out
    }

    /// What runs without being asked about.
    fn render_allow(&self) -> String {
        let rules = self.allow.texts();
        let mut out = String::from("runs without asking:\n");
        out.push_str(&pack(rules.iter().map(String::as_str), "  ", REPORT_WIDTH));
        out.trim_end().to_string()
    }

    /// What this machine gives the interface.
    fn render_machine(&self) -> String {
        format!(
            "this machine\n  terminal  {}\n  colours   {}\n  needs     nothing installed for \
             spill itself: no Rust toolchain and no runtime\n",
            self.machine.terminal_line(),
            self.machine.colour_line()
        )
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
                "cli": tier.cli.as_ref().map(|cli| json!({
                    "binPath": cli.bin_path.as_ref().map(|path| path.display().to_string()),
                    "sessions": cli.continues_sessions(),
                    "sessionArgs": cli.session_args,
                    "resumeArgs": cli.resume_args,
                    "readOnlyArgs": cli.read_only_args,
                    "consultable": !cli.read_only_args.is_empty(),
                })),
            })).collect::<Vec<_>>(),
            "notes": self.notes,
            // Everything below is carried for a machine even where the prose
            // leaves it out: the report is meant to be pasted into a bug report,
            // and a reader should not have to ask for the rest of it.
            "config": {
                "path": self.origin.path().map(|path| path.display().to_string()),
                "source": self.origin.token(),
                "workspace": self.workspace,
                "stickyFallback": self.sticky_fallback,
                "maxSteps": self.max_steps,
                "shellTimeoutSecs": self.shell_timeout_secs,
            },
            "credentials": self.credentials.as_ref().map(|credentials| json!({
                "removed": credentials.removed,
                "set": credentials.set,
            })),
            "machine": {
                "terminal": self.machine.terminal.map(|(columns, rows)| json!({
                    "columns": columns,
                    "rows": rows,
                })),
                "colours": self.machine.colours,
                "noColor": self.machine.no_color,
            },
            "allowShell": {
                "rules": self.allow.texts(),
                "default": self.allow.is_default(),
            },
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

/// How wide the label column is in the blocks that have one.
const LABEL_WIDTH: usize = 9;

/// How wide the report wraps, in columns.
const REPORT_WIDTH: usize = 78;

/// What a `cli` tier resolved to, as the lines that belong under it.
fn cli_lines(cli: &CliDetail) -> Vec<String> {
    let mut lines = Vec::new();

    // Only when it was found. A missing one is already the tier's own failure
    // line, and repeating it here would say the same thing twice.
    if let Some(path) = &cli.bin_path {
        lines.push(format!("binary: {}", path.display()));
    }

    if cli.continues_sessions() {
        // Both sets of flags, because they answer different questions: what
        // opens a session, and what comes back to it. A CLI that names its own
        // id has only the second, and saying so is the point.
        let mut parts = Vec::new();
        if cli.captures_session() {
            parts.push("the CLI names the session".to_string());
        } else {
            parts.push(format!("opens with {}", cli.session_args.join(" ")));
        }
        parts.push(format!("resumes with {}", cli.resume_args.join(" ")));
        lines.push(format!("sessions: on · {}", parts.join(" · ")));
    } else {
        lines.push(
            "sessions: off · the whole transcript is sent every turn, which costs tokens but \
             survives a CLI that cannot remember"
                .to_string(),
        );
    }

    if cli.read_only_args.is_empty() {
        lines.push(
            "consult: no read-only flag for this CLI, so a stall hands the turn over instead of \
             asking it"
                .to_string(),
        );
    } else {
        lines.push(format!(
            "consult: available read-only · {}",
            cli.read_only_args.join(" ")
        ));
    }

    lines
}

/// The removed credentials: the whole list, then the ones that are set.
fn render_credentials(credentials: &Credentials) -> String {
    let mut out = String::from(
        "delegated CLIs sign in for themselves, so spill removes these from their\nenvironment \
         before they start — a key exported for another tier cannot change\nwhose account is \
         billed:\n",
    );
    out.push_str(&pack(
        credentials.removed.iter().copied(),
        "  ",
        REPORT_WIDTH,
    ));

    if !credentials.set.is_empty() {
        out.push_str(&format!(
            "  * set here right now, so this is the removal doing something: {}\n",
            credentials.set.join(" ")
        ));
    }

    out
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
        let cli = cli_detail(library, tier);

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
            cli,
        });
    }

    // Only when there is a CLI tier. With none, nothing is removed from
    // anything, and a list of variables that will not be touched is noise in
    // exactly the report that most needs to stay readable.
    let credentials = config
        .tiers
        .iter()
        .any(|tier| tier.kind == TierKind::Cli)
        .then(|| Credentials {
            removed: crate::spawn::MODEL_CREDENTIALS.to_vec(),
            set: crate::spawn::inherited_credentials(),
        });

    Report {
        tiers,
        notes: crate::tiers::notes(library, config),
        version: env!("CARGO_PKG_VERSION").to_string(),
        origin: config.origin.clone(),
        workspace: config.general.workspace.clone(),
        sticky_fallback: config.general.sticky_fallback,
        max_steps: config.general.max_steps,
        shell_timeout_secs: config.general.shell_timeout_secs,
        credentials,
        machine: Machine::detect(),
        allow: AllowRules::new(&config.general.allow_shell),
    }
}

/// A `cli` tier's resolved spec, as the report shows it.
fn cli_detail(library: &Library, tier: &crate::config::Tier) -> Option<CliDetail> {
    if tier.kind != TierKind::Cli {
        return None;
    }

    // A spec that will not resolve is already the tier's own failure line above.
    let spec: CliSpec = cli_spec(library, tier).ok()?;

    Some(CliDetail {
        bin_path: which(&spec.bin),
        resume_args: spec.resume_args.clone(),
        session_args: spec.session_args.clone(),
        read_only_args: spec.read_only_args.clone(),
    })
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
            origin: Origin::Found(PathBuf::from("/home/you/.config/spill/config.toml")),
            workspace: "~".to_string(),
            sticky_fallback: true,
            max_steps: crate::config::DEFAULT_MAX_STEPS,
            shell_timeout_secs: crate::config::DEFAULT_SHELL_TIMEOUT_SECS,
            credentials: None,
            machine: machine(),
            allow: AllowRules::new(
                &crate::allow::READ_ONLY_SHELL
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>(),
            ),
        }
    }

    /// A fixed machine, so a render test reads the same wherever it runs. The
    /// real one is asked once, by `diagnose`, and cannot be asserted about: it
    /// is whatever terminal the suite happens to be running in.
    fn machine() -> Machine {
        Machine {
            terminal: Some((110, 40)),
            colours: u16::MAX,
            no_color: false,
        }
    }

    /// A `cli` tier's detail, as a preset would resolve it.
    fn cli_detail(bin: &str, resume: &[&str], session: &[&str], read_only: &[&str]) -> CliDetail {
        let owned = |flags: &[&str]| flags.iter().map(|flag| flag.to_string()).collect();
        CliDetail {
            bin_path: Some(PathBuf::from(format!("/usr/local/bin/{bin}"))),
            resume_args: owned(resume),
            session_args: owned(session),
            read_only_args: owned(read_only),
        }
    }

    /// A `cli` tier reported with those flags, for the render tests.
    fn cli_tier(id: &str, resume: &[&str], session: &[&str], read_only: &[&str]) -> TierReport {
        TierReport {
            kind: "cli".to_string(),
            target: id.to_string(),
            cli: Some(cli_detail(id, resume, session, read_only)),
            ..tier(id, true)
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
            cli: None,
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

    // ---- the five questions a bug report has to answer on its own ----------

    #[test]
    fn the_report_names_the_file_in_force_and_how_it_was_chosen() {
        // The first question when the run does not match the file you edited,
        // and the one thing a report previously could not answer at all.
        let mut report = report(vec![tier("local", true)]);
        report.origin = Origin::Found(PathBuf::from("/home/you/.config/spill/config.toml"));
        let text = report.render();
        assert!(
            text.contains("/home/you/.config/spill/config.toml"),
            "{text}"
        );
        assert!(text.contains("found at the default path"), "{text}");

        // And an explicit file says so, because "which of the two am I editing"
        // is the whole reason to print the path rather than assume it.
        report.origin = Origin::Given(PathBuf::from("/tmp/other.toml"));
        let text = report.render();
        assert!(text.contains("/tmp/other.toml"), "{text}");
        assert!(text.contains("named by --config"), "{text}");
    }

    #[test]
    fn a_configuration_that_is_not_there_names_the_path_it_looked_in() {
        // "No tiers are configured" is only actionable with the path beside it:
        // the file may be somewhere else, or not written yet.
        let mut report = report(Vec::new());
        report.origin = Origin::Missing(PathBuf::from("/home/you/.config/spill/config.toml"));
        let text = report.render();

        assert!(
            text.contains("/home/you/.config/spill/config.toml"),
            "{text}"
        );
        assert!(text.contains("built-in defaults are in force"), "{text}");
        assert!(text.contains("No tiers are configured"), "{text}");
    }

    #[test]
    fn the_config_line_carries_the_settings_that_change_how_a_session_behaves() {
        let mut report = report(vec![tier("local", true)]);
        report.workspace = "/home/you/code".to_string();
        report.sticky_fallback = false;
        let text = report.render();

        assert!(text.contains("workspace /home/you/code"), "{text}");
        assert!(
            text.contains("sticky fallback off"),
            "a session that does not stick is worth seeing before it surprises someone: {text}"
        );
        assert!(text.contains("32 steps"), "{text}");
        assert!(text.contains("shell 300s"), "{text}");
    }

    #[test]
    fn a_cli_tier_says_which_binary_and_which_flags_it_will_use() {
        // None of this is visible anywhere else: the flags come from a preset
        // nobody read, and the path is the answer to "it works in my shell".
        let report = report(vec![cli_tier(
            "helper",
            &["-r", "{session}"],
            &["-s", "{session}"],
            &["--permission-mode", "plan"],
        )]);
        let text = report.render();

        assert!(text.contains("binary: /usr/local/bin/helper"), "{text}");
        assert!(
            text.contains("opens with -s {session}"),
            "opening a session and returning to it are different flags: {text}"
        );
        assert!(text.contains("resumes with -r {session}"), "{text}");
        assert!(
            text.contains("consult: available read-only · --permission-mode plan"),
            "{text}"
        );
    }

    #[test]
    fn a_cli_tier_that_cannot_remember_says_the_whole_transcript_is_resent() {
        let report = report(vec![cli_tier("helper", &[], &[], &[])]);
        let text = report.render();

        assert!(text.contains("sessions: off"), "{text}");
        assert!(
            text.contains("the whole transcript is sent every turn"),
            "the cost of a CLI that cannot remember should be stated, not implied: {text}"
        );
        assert!(
            text.contains("no read-only flag"),
            "and a tier that cannot be consulted should say so: {text}"
        );
    }

    #[test]
    fn a_cli_that_never_prints_its_session_id_is_described_that_way() {
        // A CLI spill can continue but cannot name: it mints the id itself. That
        // is a different arrangement from one spill opens, and the flags alone
        // do not say which.
        let report = report(vec![cli_tier("helper", &["-r", "{session}"], &[], &[])]);
        assert!(
            report.render().contains("the CLI names the session"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn an_endpoint_tier_has_nothing_to_say_about_cli_flags() {
        // The lines belong to the kind. An endpoint has no binary, no session
        // and no read-only mode, and printing empty ones would be noise in
        // exactly the report that has to stay readable.
        let text = report(vec![tier("local", true)]).render();
        for line in ["binary:", "sessions:", "consult:"] {
            assert!(!text.contains(line), "{line} should not appear: {text}");
        }
    }

    #[test]
    fn the_credentials_section_carries_the_whole_list_not_only_the_set_ones() {
        // The question people arrive with is "is my key the one being taken
        // away?", and a list that answers by omission cannot be read.
        let mut report = report(vec![cli_tier("helper", &[], &[], &[])]);
        report.credentials = Some(Credentials {
            removed: crate::spawn::MODEL_CREDENTIALS.to_vec(),
            set: vec!["XAI_API_KEY"],
        });
        let text = report.render();

        for name in crate::spawn::MODEL_CREDENTIALS {
            assert!(text.contains(name), "{name} should be listed: {text}");
        }
        assert!(
            text.contains("* set here right now"),
            "the ones in force are the ones doing something: {text}"
        );
        assert!(text.contains("XAI_API_KEY"), "{text}");

        // Wrapped, and every name intact as one word. Checked against the block
        // rather than the whole report: this is the text long enough to need
        // wrapping, and a tier's own detail line is allowed to run past it.
        let block = render_credentials(report.credentials.as_ref().expect("set above"));
        for line in block.lines() {
            assert!(
                line.chars().count() <= REPORT_WIDTH,
                "a line is wider than the report wraps: {line:?}"
            );
        }
        for name in crate::spawn::MODEL_CREDENTIALS {
            assert!(
                block
                    .lines()
                    .any(|line| line.split(' ').any(|word| word == *name)),
                "{name} should be greppable as one word: {block}"
            );
        }
    }

    #[test]
    fn a_report_with_nothing_delegated_prints_no_credentials_section_at_all() {
        // Whether to ask is `diagnose`'s call, from the configuration — that
        // half is covered below, against a real config. This is the other half:
        // an absent answer prints nothing rather than an empty heading.
        let text = report(vec![tier("local", true)]).render();

        assert!(text.contains("this machine"), "{text}");
        assert!(!text.contains("ANTHROPIC_API_KEY"), "{text}");
        assert!(!text.contains("delegated CLIs"), "{text}");
    }

    #[test]
    fn the_machine_block_says_what_the_interface_was_given() {
        let mut report = report(vec![tier("local", true)]);
        report.machine = Machine {
            terminal: Some((120, 40)),
            colours: u16::MAX,
            no_color: false,
        };
        let text = report.render();

        assert!(text.contains("terminal  120x40"), "{text}");
        assert!(text.contains("colours   truecolor"), "{text}");
        assert!(
            text.contains("no Rust toolchain"),
            "the question this line answers is whether anything else must be installed: {text}"
        );
    }

    #[test]
    fn a_report_written_to_a_pipe_does_not_invent_a_terminal_size() {
        // The usual case for a pasted report, and the one where a made-up
        // "80x24" would send someone looking at the wrong thing entirely.
        let mut report = report(vec![tier("local", true)]);
        report.machine = Machine {
            terminal: None,
            colours: 256,
            no_color: false,
        };
        let text = report.render();

        assert!(text.contains("not a terminal"), "{text}");
        assert!(text.contains("colours   256 colours"), "{text}");
    }

    #[test]
    fn no_color_is_reported_as_the_answer_about_colour() {
        let mut report = report(vec![tier("local", true)]);
        report.machine = Machine {
            terminal: Some((80, 24)),
            colours: u16::MAX,
            no_color: true,
        };
        let text = report.render();

        assert!(
            text.contains("NO_COLOR is set so none of them are used"),
            "the depth is not the answer once NO_COLOR has the last word: {text}"
        );
    }

    #[test]
    fn the_json_carries_every_new_field_even_where_the_prose_leaves_it_out() {
        // A machine reading the report should not have to infer a field from its
        // absence — the same reason `onStuck` is carried for every tier.
        let mut built = report(vec![cli_tier(
            "helper",
            &["-r", "{session}"],
            &["-s", "{session}"],
            &["--permission-mode", "plan"],
        )]);
        built.origin = Origin::Given(PathBuf::from("/tmp/other.toml"));
        built.workspace = "/home/you/code".to_string();
        built.sticky_fallback = false;
        built.credentials = Some(Credentials {
            removed: crate::spawn::MODEL_CREDENTIALS.to_vec(),
            set: Vec::new(),
        });
        built.machine = Machine {
            terminal: None,
            colours: 256,
            no_color: true,
        };

        let parsed: serde_json::Value = serde_json::from_str(&built.to_json()).expect("valid JSON");

        assert_eq!(parsed["config"]["path"], "/tmp/other.toml");
        assert_eq!(
            parsed["config"]["source"], "given",
            "a stable token, not prose"
        );
        assert_eq!(parsed["config"]["workspace"], "/home/you/code");
        assert_eq!(parsed["config"]["stickyFallback"], false);
        assert_eq!(
            parsed["config"]["maxSteps"],
            crate::config::DEFAULT_MAX_STEPS
        );
        assert_eq!(
            parsed["config"]["shellTimeoutSecs"],
            crate::config::DEFAULT_SHELL_TIMEOUT_SECS
        );
        assert_eq!(
            parsed["credentials"]["removed"].as_array().map(Vec::len),
            Some(crate::spawn::MODEL_CREDENTIALS.len())
        );
        assert_eq!(parsed["machine"]["terminal"], serde_json::Value::Null);
        assert_eq!(parsed["machine"]["colours"], 256);
        assert_eq!(parsed["machine"]["noColor"], true);

        let cli = &parsed["tiers"][0]["cli"];
        assert_eq!(cli["binPath"], "/usr/local/bin/helper");
        assert_eq!(cli["sessions"], true);
        assert_eq!(cli["sessionArgs"][0], "-s");
        assert_eq!(cli["resumeArgs"][0], "-r");
        assert_eq!(cli["consultable"], true);

        // And absent rather than empty for a tier that has none.
        let mut endpoint = report(vec![tier("local", true)]);
        endpoint.machine = built.machine.clone();
        let parsed: serde_json::Value =
            serde_json::from_str(&endpoint.to_json()).expect("valid JSON");
        assert_eq!(parsed["tiers"][0]["cli"], serde_json::Value::Null);
        assert_eq!(parsed["credentials"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_cli_tier_is_resolved_from_its_config_not_from_a_preset_being_present() {
        // The flags in the report have to be the ones the tier will actually
        // run with, which means they come from the same resolver the agent uses.
        let library = Library::embedded();
        let report = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "helper"
                kind = "cli"
                bin = "spill-nothing-is-called-this"
                args = ["-p", "{prompt}"]
                session_args = ["--session-id", "{session}"]
                resume_args = ["--resume", "{session}"]
                "#,
            ),
        )
        .await;

        let cli = report.tiers[0].cli.as_ref().expect("a resolved spec");
        assert_eq!(cli.resume_args, vec!["--resume", "{session}"]);
        assert!(
            !cli.captures_session(),
            "this tier declares its own session flags, so spill opens the session"
        );
        assert!(
            cli.read_only_args.is_empty(),
            "and it declares no read-only flag, so it cannot be consulted"
        );
        // The binary cannot be found, so there is no path to print — and the
        // tier's own failure line is already saying so.
        assert!(cli.bin_path.is_none());
        assert!(!report.tiers[0].reachable);
        assert!(!report.render().contains("binary:"), "{}", report.render());
    }

    #[tokio::test]
    async fn credentials_are_asked_for_only_when_a_delegated_cli_is_configured() {
        let library = Library::embedded();

        let with_cli = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "helper"
                kind = "cli"
                bin = "spill-nothing-is-called-this"
                args = ["-p", "{prompt}"]
                "#,
            ),
        )
        .await;
        let credentials = with_cli.credentials.as_ref().expect("a CLI tier");
        assert_eq!(credentials.removed, crate::spawn::MODEL_CREDENTIALS);
        assert!(
            credentials
                .set
                .iter()
                .all(|name| credentials.removed.contains(name)),
            "everything reported as set must be one of the ones being removed"
        );

        let without = diagnose(
            &library,
            &config(
                r#"
                [[tier]]
                id = "local"
                kind = "openai"
                base_url = "http://127.0.0.1:9/v1"
                model = "m"
                "#,
            ),
        )
        .await;
        assert!(
            without.credentials.is_none(),
            "with nothing delegated there is nothing to strip"
        );
    }

    #[tokio::test]
    async fn an_endpoint_tier_never_carries_cli_detail() {
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
                "#,
            ),
        )
        .await;

        assert!(report.tiers[0].cli.is_none());
    }

    // ---- what runs without being asked --------------------------------------

    #[test]
    fn the_rules_are_printed_only_when_they_are_not_the_defaults() {
        // Twenty-three lines reciting the read-only defaults would drown the tier
        // list they sit under, so prose shows a *choice* — the same reasoning as
        // the stuck policy. The JSON carries it either way.
        let text = report(vec![tier("local", true)]).render();
        assert!(!text.contains("runs without asking"), "{text}");

        let mut chosen = report(vec![tier("local", true)]);
        chosen.allow = AllowRules::new(&["make".to_string(), "cargo test".to_string()]);
        let text = chosen.render();
        assert!(text.contains("runs without asking"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(
            !text.contains("git status"),
            "and not the defaults it replaced: {text}"
        );
    }

    #[test]
    fn the_json_carries_the_rules_and_whether_they_are_the_defaults() {
        // The question a rule answers badly is "why did that run without asking
        // me?", and a script asking it should not have to guess from a missing
        // line — the same reason `onStuck` is carried for every tier.
        let mut chosen = report(vec![tier("local", true)]);
        chosen.allow = AllowRules::new(&["git status".to_string()]);
        let parsed: serde_json::Value =
            serde_json::from_str(&chosen.to_json()).expect("valid JSON");

        assert_eq!(parsed["allowShell"]["default"], false);
        assert_eq!(parsed["allowShell"]["rules"][0], "git status");

        let parsed: serde_json::Value =
            serde_json::from_str(&report(vec![tier("local", true)]).to_json()).expect("valid JSON");
        assert_eq!(parsed["allowShell"]["default"], true);
        assert_eq!(
            parsed["allowShell"]["rules"].as_array().map(Vec::len),
            Some(crate::allow::READ_ONLY_SHELL.len())
        );
    }

    #[tokio::test]
    async fn the_report_reads_the_rules_out_of_the_configuration() {
        let library = Library::embedded();

        let chosen = diagnose(
            &library,
            &config("[general]\nallow_shell = [\"make\", \"git status\"]\n"),
        )
        .await;
        assert_eq!(
            chosen.allow.texts(),
            vec!["make".to_string(), "git status".to_string()]
        );
        assert!(!chosen.allow.is_default());

        // And the empty list — the way to ask about everything — is a choice too.
        let emptied = diagnose(&library, &config("[general]\nallow_shell = []\n")).await;
        assert!(emptied.allow.texts().is_empty());
        assert!(!emptied.allow.is_default());
        assert!(
            !emptied.render().contains("runs without asking"),
            "nothing runs unasked, so there is no list to print"
        );
    }
}
