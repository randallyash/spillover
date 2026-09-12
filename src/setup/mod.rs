//! `spill setup`: choosing tiers, and writing the choice down.
//!
//! The flow follows the shape the tool is built around: one local model first,
//! then up to two online models in the order you want them tried. The state
//! machine here is pure — no terminal, no network — so the whole path from
//! "nothing detected" to "a config file" can be tested directly.

pub mod probe;
pub mod ui;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{LimitOverrides, OnStuck, Tier, TierKind};
use crate::preset::{Cli, Library};
use crate::provider::cli::on_path;
use crate::setup::probe::{Found, Readiness, unserved_model};

/// How many online fallbacks the wizard offers, after the local model.
pub const MAX_ONLINE: usize = 2;

/// The agent CLIs a first run will accept as a fallback, most likely first.
///
/// A *preference order among the ones actually installed* — the first on PATH
/// wins and the rest are never looked at — so this says nothing about which is
/// better. It is roughly by how common each is, because a first run should land
/// on something the person already uses rather than on a niche one they happen
/// to also have.
///
/// `command-code` is last on purpose. It is the harness you may be running spill
/// inside, and quietly choosing it bills a plan the person is already using —
/// fine to offer, not fine to pick for them.
const FIRST_RUN_FALLBACKS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "copilot",
    "cursor-agent",
    "grok",
    "opencode",
    "crush",
    "command-code",
];

/// The first agent CLI on this machine, if there is one.
///
/// This is what makes a first run a *complete* setup instead of half of one.
/// "Local until it isn't" has nowhere to spill to with a single tier, so the
/// headline behaviour is inert until a second one exists — and the moment to
/// add it is the moment someone has nothing configured, not after their first
/// stalled turn.
///
/// `None` is a normal answer, not a failure: plenty of machines have no agent
/// CLI installed, and an all-local chain is a legitimate way to run.
pub fn first_run_fallback(library: &Library) -> Option<&Cli> {
    first_run_fallback_with(library, on_path)
}

/// The same question, with "is it installed" supplied.
///
/// `PATH` is process-wide and tests run in threads, so the one thing worth
/// varying here — which CLIs exist on this machine — is passed in rather than
/// read, and the ordering can be tested without one test's environment leaking
/// into another's.
fn first_run_fallback_with(library: &Library, installed: impl Fn(&str) -> bool) -> Option<&Cli> {
    FIRST_RUN_FALLBACKS.iter().find_map(|id| {
        let preset = library.cli.iter().find(|preset| preset.id == *id)?;
        installed(&preset.bin).then_some(preset)
    })
}

/// The row that lets someone type an address not in the shipped list.
pub const CUSTOM_ENDPOINT_ID: &str = "custom-online";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Probing local servers.
    Finding,
    ChooseLocal,
    /// Typing a local server's address.
    CustomLocal,
    ChooseOnline,
    /// Typing the address of an online endpoint that is not in the list.
    CustomOnline,
    /// Picking from the models an endpoint advertises.
    ChooseModel,
    /// Typing a model id the endpoint did not advertise.
    OnlineModel,
    Review,
}

impl Step {
    /// Whether this step is a text box, and so every printable key belongs to
    /// what is being typed rather than to a shortcut.
    ///
    /// Kept here so that adding a text step cannot leave it routed to the
    /// shortcut keys by omission — which would swallow what the user typed and
    /// could fire `w` or `q` from inside an address.
    pub fn accepts_text(self) -> bool {
        matches!(
            self,
            Step::CustomLocal | Step::CustomOnline | Step::OnlineModel
        )
    }
}

/// One thing the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Stable identity, so nothing can be chosen twice.
    pub id: String,
    pub name: String,
    /// A second line: where it is, or what command it runs.
    pub detail: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A local server that answered the probe.
    Local {
        preset_id: String,
        base_url: String,
        model: Option<String>,
    },
    /// A local server the user typed in.
    CustomLocal { base_url: String },
    /// A hosted endpoint or agent CLI, used as a fallback.
    Online {
        preset_id: String,
        kind: TierKind,
        base_url: Option<String>,
        bin: Option<String>,
        /// The environment variable a hosted endpoint's key comes from, needed
        /// even to ask it which models it serves.
        api_key_env: Option<String>,
        models: Vec<String>,
    },
}

pub struct Wizard {
    pub step: Step,
    /// Local servers that answered, then the two ways out.
    pub local: Vec<Choice>,
    pub local_cursor: usize,
    /// Every online option, hosted endpoints and agent CLIs alike.
    pub online: Vec<Choice>,
    pub online_cursor: usize,
    /// Models the endpoint being set up advertised.
    pub models: Vec<String>,
    pub model_cursor: usize,
    /// Whether the model list is still being fetched.
    pub models_loading: bool,
    /// Why the model list could not be fetched, if it could not.
    pub models_problem: Option<String>,
    pub local_choice: Option<Choice>,
    pub online_chosen: Vec<Choice>,
    /// A hosted choice waiting on its model id.
    pending: Option<Choice>,
    /// Text being typed for a custom address or a model id.
    pub text: String,
    pub problem: Option<String>,
    /// How each tier fared, filled in as the checks come back. A tier with no
    /// answer yet is still being checked.
    pub checks: Vec<Option<Readiness>>,
    pub written: Option<PathBuf>,
    pub quit: bool,
}

/// A way to add an endpoint that is not in the shipped list, such as a company
/// gateway or a provider added after this build.
fn custom_endpoint() -> Choice {
    Choice {
        id: CUSTOM_ENDPOINT_ID.to_string(),
        name: "Another endpoint".to_string(),
        detail: "type an address, for anything not listed above".to_string(),
        source: Source::Online {
            preset_id: String::new(),
            kind: TierKind::OpenAi,
            base_url: None,
            bin: None,
            api_key_env: None,
            models: Vec::new(),
        },
    }
}

impl Wizard {
    pub fn new(library: &Library) -> Self {
        let hosted = library.online.iter().map(|preset| Choice {
            id: preset.id.clone(),
            name: preset.name.clone(),
            detail: preset.base_url.clone(),
            source: Source::Online {
                preset_id: preset.id.clone(),
                kind: TierKind::OpenAi,
                base_url: Some(preset.base_url.clone()),
                bin: None,
                api_key_env: preset.api_key_env.clone(),
                models: Vec::new(),
            },
        });

        let commands = library.cli.iter().map(|preset| Choice {
            id: preset.id.clone(),
            name: preset.name.clone(),
            detail: format!("{} · {}", preset.bin, preset.name),
            source: Source::Online {
                preset_id: preset.id.clone(),
                kind: TierKind::Cli,
                base_url: None,
                bin: Some(preset.bin.clone()),
                api_key_env: None,
                models: preset.models.clone(),
            },
        });

        Self {
            step: Step::Finding,
            local: Vec::new(),
            local_cursor: 0,
            // Agent CLIs first: they need no key and no model id typed in.
            online: commands
                .chain(hosted)
                .chain(std::iter::once(custom_endpoint()))
                .collect(),
            online_cursor: 0,
            models: Vec::new(),
            model_cursor: 0,
            models_loading: false,
            models_problem: None,
            local_choice: None,
            online_chosen: Vec::new(),
            pending: None,
            text: String::new(),
            problem: None,
            checks: Vec::new(),
            written: None,
            quit: false,
        }
    }

    /// Called with whatever the local probe turned up.
    pub fn local_probed(&mut self, found: Vec<Found>) {
        self.local = found
            .into_iter()
            .map(|found| Choice {
                id: found.preset_id.clone(),
                name: found.name,
                detail: match &found.model {
                    Some(model) => format!("{} · {}", found.base_url, model),
                    None => found.base_url.clone(),
                },
                source: Source::Local {
                    preset_id: found.preset_id,
                    base_url: found.base_url,
                    model: found.model,
                },
            })
            .collect();

        self.local.push(Choice {
            id: "custom".to_string(),
            name: "Enter an address myself".to_string(),
            detail: "a server on another machine, say".to_string(),
            source: Source::CustomLocal {
                base_url: String::new(),
            },
        });
        self.local.push(Choice {
            id: "none".to_string(),
            name: "No local model".to_string(),
            detail: "start with an online model instead".to_string(),
            source: Source::Local {
                preset_id: String::new(),
                base_url: String::new(),
                model: None,
            },
        });

        self.local_cursor = 0;
        self.step = Step::ChooseLocal;
    }

    /// The online options not already taken.
    pub fn available_online(&self) -> Vec<Choice> {
        self.online
            .iter()
            .filter(|choice| !self.online_chosen.iter().any(|taken| taken.id == choice.id))
            .cloned()
            .collect()
    }

    /// Which online question is being asked: 0 is the first fallback.
    pub fn online_slot(&self) -> usize {
        self.online_chosen.len()
    }

    /// The hosted endpoint waiting on a model choice, as its id, address, and
    /// the environment variable its key comes from.
    ///
    /// The id returned is the *choice's*, not the preset's. They are the same
    /// for a shipped preset, but a typed endpoint has no preset and carries an
    /// empty one — so returning the preset id named the custom endpoint `""` and
    /// the model list fetched for it was discarded as belonging to someone else,
    /// leaving the screen asking forever.
    pub fn pending_endpoint(&self) -> Option<(String, String, Option<String>)> {
        let choice = self.pending.as_ref()?;
        match &choice.source {
            Source::Online {
                base_url: Some(base_url),
                api_key_env,
                ..
            } => Some((choice.id.clone(), base_url.clone(), api_key_env.clone())),
            _ => None,
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let (cursor, len) = match self.step {
            Step::ChooseLocal => (&mut self.local_cursor, self.local.len()),
            Step::ChooseOnline => {
                // Counted first, so the list is not borrowed while the cursor moves.
                let len = self.available_online().len();
                (&mut self.online_cursor, len)
            }
            // One extra row: "type a model id myself".
            Step::ChooseModel => (&mut self.model_cursor, self.models.len() + 1),
            _ => return,
        };

        if len == 0 {
            return;
        }
        let next = *cursor as isize + delta;
        *cursor = next.clamp(0, len as isize - 1) as usize;
    }

    pub fn confirm(&mut self) {
        match self.step {
            Step::ChooseLocal => {
                let Some(choice) = self.local.get(self.local_cursor).cloned() else {
                    return;
                };
                match &choice.source {
                    Source::CustomLocal { .. } => {
                        self.begin_text();
                        self.step = Step::CustomLocal;
                    }
                    // The "no local model" entry is a real answer: no tier.
                    Source::Local { preset_id, .. } if preset_id.is_empty() => {
                        self.local_choice = None;
                        self.step = Step::ChooseOnline;
                    }
                    _ => {
                        self.local_choice = Some(choice);
                        self.step = Step::ChooseOnline;
                    }
                }
            }
            Step::ChooseOnline => {
                let available = self.available_online();
                let Some(choice) = available.get(self.online_cursor).cloned() else {
                    return;
                };

                let needs_model = matches!(
                    &choice.source,
                    Source::Online {
                        kind: TierKind::OpenAi,
                        ..
                    }
                );
                if choice.id == CUSTOM_ENDPOINT_ID {
                    // Ask for the address before asking for a model.
                    self.begin_text();
                    self.pending = Some(choice);
                    self.step = Step::CustomOnline;
                } else if needs_model {
                    // Ask the endpoint what it offers before asking the user to
                    // type anything: a model id is easy to get wrong.
                    self.models.clear();
                    self.model_cursor = 0;
                    self.models_problem = None;
                    self.models_loading = true;
                    self.pending = Some(choice);
                    self.step = Step::ChooseModel;
                } else {
                    self.accept_online(choice);
                }
            }
            Step::ChooseModel => {
                // The row after the advertised models is "type it myself".
                if self.model_cursor >= self.models.len() {
                    self.begin_text();
                    self.step = Step::OnlineModel;
                } else {
                    let model = self.models[self.model_cursor].clone();
                    self.accept_model(model);
                }
            }
            _ => {}
        }
    }

    /// The endpoint's models arrived: offer them, or fall back to typing if
    /// there are none to offer.
    pub fn models_arrived(&mut self, endpoint_url: &str, result: Result<Vec<String>, String>) {
        // The list belongs to the endpoint it was fetched for, identified by its
        // address. A slow answer landing after the user has moved on would
        // otherwise be offered as the new endpoint's models — and since this list
        // is what a typed id is checked against, it would refuse a perfectly good
        // one. The address rather than the id, because every typed endpoint
        // shares the one menu-row id, so the id cannot tell two of them apart.
        let current = self.pending_endpoint().map(|(_, url, _)| url);
        if current.as_deref() != Some(endpoint_url) {
            return;
        }
        // Cleared whatever the outcome, so a late answer cannot leave the screen
        // saying it is still asking.
        self.models_loading = false;

        match result {
            Ok(models) if !models.is_empty() => {
                // Kept even if the user has already started typing, so their id
                // can still be checked when they commit it.
                self.models = models;
                self.model_cursor = 0;
            }
            // Only move the user when they are still waiting on this step: an
            // answer arriving over their typing must not wipe it.
            Ok(_) if self.step == Step::ChooseModel => {
                self.models_problem =
                    Some("this endpoint listed no models, so type the id to use".to_string());
                self.begin_text();
                self.step = Step::OnlineModel;
            }
            Ok(_) => {}
            Err(message) if self.step == Step::ChooseModel => {
                // Not being able to list models is not a dead end; many
                // endpoints serve chat completions without advertising them.
                self.models_problem = Some(format!("{message} — type the id to use"));
                self.begin_text();
                self.step = Step::OnlineModel;
            }
            Err(_) => {}
        }
    }

    /// Why the config cannot be written yet, if it cannot.
    ///
    /// The README promises that every choice is checked before anything is
    /// written. Until now nothing enforced it: a failed check was only drawn, and
    /// `w` wrote regardless, so a tier that could not answer — a typed model id
    /// the endpoint does not serve, a CLI that is not installed — reached the
    /// config and failed on the first turn instead of here.
    pub fn write_blocked(&self) -> Option<String> {
        if self.tiers().is_empty() {
            return Some("no tiers chosen yet, so there is nothing to write".to_string());
        }

        let checks = &self.checks;
        // Still checking. Writing now would be writing unchecked, which is the
        // thing this guard exists to prevent.
        if checks.is_empty() || checks.iter().any(Option::is_none) {
            return Some("still checking these tiers — wait for the results".to_string());
        }

        let failed = checks.iter().flatten().filter(|check| !check.ok).count();
        if failed > 0 {
            return Some(format!(
                "{failed} of {} tiers cannot answer; fix or remove {} before writing",
                checks.len(),
                if failed == 1 { "it" } else { "them" }
            ));
        }

        None
    }

    /// The endpoint whose model list still needs asking for, if any.
    ///
    /// The only place that decides whether to ask, so the answer cannot be asked
    /// for repeatedly: once it lands, `models_loading` is false and this stops
    /// saying yes.
    ///
    /// Without that, the wizard re-requested the list on every arrival while the
    /// model step was open — a request per round trip against the endpoint, and
    /// a cursor that snapped back to the top each time, which made "type it
    /// myself" unreachable behind a list that kept re-selecting its first row.
    pub fn wants_models(&self) -> Option<(String, String, Option<String>)> {
        if self.step != Step::ChooseModel || !self.models_loading {
            return None;
        }
        self.pending_endpoint()
    }

    /// Attach a model to the endpoint being set up and keep it.
    fn accept_model(&mut self, model: String) {
        if let Some(mut choice) = self.pending.take() {
            if let Source::Online { models, .. } = &mut choice.source {
                models.push(model.clone());
            }
            choice.detail = format!("{} · {model}", choice.detail);
            self.accept_online(choice);
        }
    }

    /// Finish typing whatever the current text step is asking for.
    pub fn commit_text(&mut self) {
        let typed = self.text.trim().to_string();

        match self.step {
            Step::CustomLocal => match check_url(&typed) {
                Ok(()) => {
                    self.local_choice = Some(Choice {
                        id: "custom".to_string(),
                        name: "Custom server".to_string(),
                        detail: typed.clone(),
                        source: Source::CustomLocal { base_url: typed },
                    });
                    self.step = Step::ChooseOnline;
                }
                Err(message) => self.problem = Some(message),
            },
            Step::CustomOnline => match check_url(&typed) {
                Ok(()) => {
                    // Now that its address is known, treat it like any other
                    // endpoint: ask which models it serves.
                    if let Some(mut choice) = self.pending.take() {
                        if let Source::Online { base_url, .. } = &mut choice.source {
                            *base_url = Some(typed.clone());
                        }
                        choice.detail = typed;
                        self.pending = Some(choice);
                    }
                    self.models.clear();
                    self.model_cursor = 0;
                    self.models_problem = None;
                    self.models_loading = true;
                    self.step = Step::ChooseModel;
                }
                Err(message) => self.problem = Some(message),
            },
            Step::OnlineModel => {
                if typed.is_empty() {
                    self.problem =
                        Some("a hosted endpoint needs a model id to talk to".to_string());
                    return;
                }

                // Checked against the list this endpoint already gave us, which
                // is the same list the picker offers — so a mistyped id is
                // caught here rather than mid-conversation, and it costs no
                // second request to do it.
                //
                // No list means no opinion: the user reaches this step precisely
                // when the endpoint would not say what it serves, and refusing
                // then would block a working setup on a guess.
                if let Some(problem) = unserved_model(&self.models, &typed) {
                    self.problem = Some(problem);
                    return;
                }

                self.accept_model(typed);
            }
            _ => {}
        }
    }

    fn accept_online(&mut self, choice: Choice) {
        self.online_chosen.push(choice);
        self.online_cursor = 0;
        self.step = if self.online_chosen.len() >= MAX_ONLINE {
            Step::Review
        } else {
            Step::ChooseOnline
        };
    }

    /// Move on with fewer than the maximum number of online tiers.
    pub fn skip_online(&mut self) {
        if self.step == Step::ChooseOnline {
            self.step = Step::Review;
        }
    }

    pub fn cancel_step(&mut self) {
        self.problem = None;
        self.step = match self.step {
            Step::CustomLocal => Step::ChooseLocal,
            // Backing out of either model step returns to the endpoint list,
            // with nothing half-chosen left behind.
            Step::OnlineModel | Step::ChooseModel | Step::CustomOnline => {
                self.pending = None;
                self.models.clear();
                Step::ChooseOnline
            }
            _ => self.step,
        };
    }

    fn begin_text(&mut self) {
        self.text.clear();
        self.problem = None;
    }

    pub fn push_char(&mut self, ch: char) {
        self.text.push(ch);
        self.problem = None;
    }

    pub fn pop_char(&mut self) {
        self.text.pop();
        self.problem = None;
    }

    /// Record one tier's check as it comes back, so a slow tier does not hold
    /// up the answer for the others.
    pub fn note_check(&mut self, index: usize, readiness: Readiness) {
        if self.checks.len() <= index {
            self.checks.resize(index + 1, None);
        }
        self.checks[index] = Some(readiness);
    }

    /// Forget the checks, for when the tiers change and they need redoing.
    pub fn clear_checks(&mut self) {
        self.checks.clear();
    }

    /// The tier list the wizard has arrived at.
    pub fn tiers(&self) -> Vec<Tier> {
        let mut tiers = Vec::new();

        match self.local_choice.as_ref().map(|choice| &choice.source) {
            Some(Source::Local {
                preset_id,
                base_url,
                model: _,
            }) if !preset_id.is_empty() => {
                // The model is deliberately left empty: a local server should
                // use whatever it has loaded, so restarting it with a different
                // model does not break the tier.
                tiers.push(make_tier(
                    "local",
                    TierKind::OpenAi,
                    &self
                        .local_choice
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    Some(preset_id.clone()),
                    Some(base_url.clone()),
                    None,
                    None,
                ));
            }
            Some(Source::CustomLocal { base_url }) => {
                tiers.push(make_tier(
                    "local",
                    TierKind::OpenAi,
                    "Custom server",
                    None,
                    Some(base_url.clone()),
                    None,
                    None,
                ));
            }
            _ => {}
        }

        for (position, choice) in self.online_chosen.iter().enumerate() {
            if let Source::Online {
                preset_id,
                kind,
                base_url,
                bin,
                models,
                ..
            } = &choice.source
            {
                // A typed endpoint has no preset to name, so it needs an id of
                // its own — tier ids have to be unique.
                let typed_endpoint = preset_id.is_empty();
                let id = if typed_endpoint {
                    format!("online-{}", position + 1)
                } else {
                    preset_id.clone()
                };

                tiers.push(make_tier(
                    &id,
                    *kind,
                    &choice.name,
                    (!typed_endpoint).then(|| preset_id.clone()),
                    base_url.clone(),
                    models.first().cloned(),
                    bin.clone(),
                ));
            }
        }

        tiers
    }

    /// The file the wizard will write, written to be read by a person.
    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# Written by `spill setup`.\n\
             #\n\
             # Tiers are tried in order. The ones below a tier are only reached when\n\
             # the one above it stalls, loops, or fails. `spill presets` lists every\n\
             # name that can appear here.\n\n\
             schema = 1\n\n\
             [general]\n\
             workspace = \"~\"\n\
             sticky_fallback = true\n",
        );

        for tier in self.tiers() {
            out.push_str("\n[[tier]]\n");
            out.push_str(&format!("id = {:?}\n", tier.id));
            out.push_str(&format!("kind = {:?}\n", tier.kind.to_string()));
            if let Some(name) = &tier.name {
                out.push_str(&format!("name = {name:?}\n"));
            }
            if let Some(preset) = &tier.preset {
                out.push_str(&format!("preset = {preset:?}\n"));
            }
            if let Some(base_url) = &tier.base_url {
                out.push_str(&format!("base_url = {base_url:?}\n"));
            }
            if let Some(model) = &tier.model {
                out.push_str(&format!("model = {model:?}\n"));
            }
            // A hand-written CLI tier needs its invocation spelled out; a preset
            // supplies it, and so does the shipped library.
            if tier.kind == TierKind::Cli && tier.preset.is_none() {
                out.push_str("args = [\"-p\", \"{prompt}\"]\n");
            }
        }

        out
    }
}

/// A URL we would be willing to hand to an HTTP client.
fn check_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err(
            "give the address of your local server, such as http://localhost:1234/v1".to_string(),
        );
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("that needs to start with http:// or https://".to_string());
    }
    Ok(())
}

/// The tier for a local server that was found running.
///
/// Used when spill starts with no configuration at all: rather than making
/// someone configure a thing that is already working, it uses it and says so.
pub fn tier_for_local(found: &Found) -> Tier {
    make_tier(
        "local",
        TierKind::OpenAi,
        &found.name,
        Some(found.preset_id.clone()),
        Some(found.base_url.clone()),
        // Empty on purpose: whatever the server has loaded is the right answer.
        None,
        None,
    )
}

/// The tier for an agent CLI that was found on PATH.
///
/// Same shape the wizard writes for the same choice — the preset id as the tier
/// id, and no model, because a CLI tier's model is the CLI's own business.
pub fn tier_for_cli(preset: &Cli) -> Tier {
    make_tier(
        &preset.id,
        TierKind::Cli,
        &preset.name,
        Some(preset.id.clone()),
        None,
        None,
        Some(preset.bin.clone()),
    )
}

#[allow(clippy::too_many_arguments)]
fn make_tier(
    id: &str,
    kind: TierKind,
    name: &str,
    preset: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    bin: Option<String>,
) -> Tier {
    Tier {
        id: id.to_string(),
        kind,
        name: Some(name.to_string()),
        base_url,
        model,
        // A preset already knows which variable holds its key.
        api_key_env: None,
        preset,
        bin,
        args: Vec::new(),
        model_args: Vec::new(),
        extra_args: Vec::new(),
        approve_args: Vec::new(),
        // Read-only flags come from the preset too, and only matter to a
        // consult: a CLI that has none is never asked as a consultant.
        read_only_args: Vec::new(),
        workdir_args: Vec::new(),
        // Session flags come from the preset, which is what makes continuity
        // work without the wizard knowing anything about it.
        session_args: Vec::new(),
        resume_args: Vec::new(),
        dialect: None,
        // Unattended runs are never something setup turns on for you.
        approve_all: false,
        // The wizard writes the reliable path: hand the turn over when a tier
        // is stuck. Consult is opt-in by hand, because it is the mode whose
        // value depends on the model and the answer.
        on_stuck: OnStuck::default(),
        consults_per_turn: crate::config::DEFAULT_CONSULTS_PER_TURN,
        // No overrides: the tier's class decides the timeouts, so a local server
        // found running gets the patience a local server needs without the wizard
        // knowing anything about it.
        limits: LimitOverrides::default(),
    }
}

/// Write the config, keeping a copy of whatever was there before.
///
/// Written to a temporary file and renamed into place, so an interrupted setup
/// cannot leave a half-written config behind.
pub fn write_config(path: &Path, text: &str) -> io::Result<Option<PathBuf>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let backup = if path.exists() {
        let backup = path.with_extension("toml.bak");
        fs::copy(path, &backup)?;
        Some(backup)
    } else {
        None
    };

    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, text)?;
    set_private(&temporary)?;
    fs::rename(&temporary, path)?;

    Ok(backup)
}

#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    // Windows has no equivalent mode; the file is already user-scoped.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn wizard() -> Wizard {
        Wizard::new(&Library::embedded())
    }

    fn found(id: &str, model: Option<&str>) -> Found {
        Found {
            preset_id: id.to_string(),
            name: id.to_string(),
            base_url: format!("http://localhost/{id}/v1"),
            model: model.map(str::to_string),
        }
    }

    /// Walk to the online step with a local model already chosen.
    fn at_online() -> Wizard {
        let mut wizard = wizard();
        wizard.local_probed(vec![found("lmstudio", Some("qwen3-coder"))]);
        wizard.confirm(); // the only detected server
        wizard
    }

    #[test]
    fn it_starts_by_looking_for_a_local_model() {
        assert_eq!(wizard().step, Step::Finding);
    }

    #[test]
    fn probed_servers_become_the_first_options() {
        let mut wizard = wizard();
        wizard.local_probed(vec![found("lmstudio", Some("qwen3-coder"))]);

        assert_eq!(wizard.step, Step::ChooseLocal);
        assert_eq!(wizard.local.len(), 3, "one server, plus two ways out");
        assert_eq!(wizard.local[0].name, "lmstudio");
        assert!(wizard.local[0].detail.contains("qwen3-coder"));
        assert_eq!(wizard.local[2].id, "none");
    }

    #[test]
    fn a_detected_server_becomes_the_local_tier_and_leaves_model_empty() {
        let wizard = at_online();

        assert_eq!(wizard.step, Step::ChooseOnline);
        let tiers = wizard.tiers();
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].kind, TierKind::OpenAi);
        assert_eq!(tiers[0].preset.as_deref(), Some("lmstudio"));
        // Left empty on purpose: whatever the server has loaded is the answer.
        assert_eq!(tiers[0].model, None);
    }

    #[test]
    fn choosing_no_local_model_starts_with_an_online_one() {
        let mut wizard = wizard();
        wizard.local_probed(Vec::new());
        wizard.move_cursor(10); // clamp to the last entry
        wizard.confirm();

        assert_eq!(wizard.step, Step::ChooseOnline);
        assert!(wizard.local_choice.is_none());
        assert!(wizard.tiers().is_empty());
    }

    #[test]
    fn forcing_the_cursor_stays_in_range() {
        let mut wizard = wizard();
        wizard.local_probed(vec![found("lmstudio", None)]);

        wizard.move_cursor(-5);
        assert_eq!(wizard.local_cursor, 0);
        wizard.move_cursor(100);
        assert_eq!(wizard.local_cursor, wizard.local.len() - 1);
    }

    #[test]
    fn an_agent_cli_needs_no_model_id_and_lands_straight_away() {
        let mut wizard = at_online();
        // The online list starts with the agent CLIs.
        assert_eq!(wizard.available_online()[0].id, "command-code");

        wizard.confirm();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(
            wizard.step,
            Step::ChooseOnline,
            "one more fallback is offered"
        );
        let tiers = wizard.tiers();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[1].kind, TierKind::Cli);
        assert_eq!(tiers[1].preset.as_deref(), Some("command-code"));
        // The preset suggests a model, so one is written in.
        assert_eq!(
            tiers[1].model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
    }

    #[test]
    fn a_custom_endpoint_is_offered_at_the_end_of_the_online_list() {
        let wizard = at_online();
        let last = wizard
            .available_online()
            .last()
            .cloned()
            .expect("a last row");
        assert_eq!(last.id, CUSTOM_ENDPOINT_ID);
        assert!(last.detail.contains("not listed"), "{}", last.detail);
    }

    #[test]
    fn a_custom_endpoint_asks_for_its_address_then_its_model() {
        let mut wizard = at_online();
        let custom = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
            .expect("the custom row is offered");
        wizard.move_cursor(custom as isize);
        wizard.confirm();

        assert_eq!(wizard.step, Step::CustomOnline);

        for ch in "https://gateway.example.com/v1".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        // Its address is known, so now it can be asked what it serves.
        assert_eq!(wizard.step, Step::ChooseModel);
        assert!(wizard.models_loading);
        assert_eq!(
            wizard.pending_endpoint().map(|(_, url, _)| url),
            Some("https://gateway.example.com/v1".to_string())
        );
    }

    /// The endpoint the wizard is asking about right now, so a test can hand its
    /// answer back under the right key. A late answer for a different endpoint is
    /// ignored on purpose, so the key has to be the current one — and the key is
    /// the address, which is what tells two typed endpoints apart.
    fn asking(wizard: &Wizard) -> String {
        wizard
            .pending_endpoint()
            .map(|(_, url, _)| url)
            .unwrap_or_default()
    }

    #[test]
    fn a_custom_endpoint_becomes_a_tier_with_no_preset() {
        let mut wizard = at_online();
        let custom = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
            .expect("offered");
        wizard.move_cursor(custom as isize);
        wizard.confirm();
        for ch in "https://gateway.example.com/v1".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Ok(vec!["their-model".to_string()]));
        wizard.confirm();

        let tiers = wizard.tiers();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[1].kind, TierKind::OpenAi);
        assert_eq!(tiers[1].preset, None, "there is no preset to name");
        assert_eq!(
            tiers[1].base_url.as_deref(),
            Some("https://gateway.example.com/v1")
        );
        assert_eq!(tiers[1].model.as_deref(), Some("their-model"));
        assert_eq!(tiers[1].id, "online-1");
    }

    #[test]
    fn a_custom_endpoint_address_without_a_scheme_is_refused() {
        let mut wizard = at_online();
        let custom = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
            .expect("offered");
        wizard.move_cursor(custom as isize);
        wizard.confirm();

        for ch in "gateway.example.com".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.step, Step::CustomOnline);
        assert!(wizard.problem.is_some());
    }

    #[test]
    fn the_generated_config_with_a_custom_endpoint_is_valid() {
        let mut wizard = at_online();
        let custom = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
            .expect("offered");
        wizard.move_cursor(custom as isize);
        wizard.confirm();
        for ch in "https://gateway.example.com/v1".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Err("no list".to_string()));
        for ch in "their-model".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        let text = wizard.to_toml();
        let parsed = Config::parse(std::path::Path::new("generated.toml"), &text)
            .expect("a hand-typed endpoint must produce a valid config");
        assert_eq!(parsed.tiers.len(), 2);
        assert_eq!(
            parsed.tiers[1].base_url.as_deref(),
            Some("https://gateway.example.com/v1")
        );
        assert_eq!(parsed.tiers[1].preset, None);
    }

    #[test]
    fn only_the_text_steps_accept_typed_characters() {
        // The routing keys off this, so a text step missing from it would have
        // what the user types interpreted as shortcuts instead.
        for step in [Step::CustomLocal, Step::CustomOnline, Step::OnlineModel] {
            assert!(step.accepts_text(), "{step:?} is a text step");
        }
        for step in [
            Step::Finding,
            Step::ChooseLocal,
            Step::ChooseOnline,
            Step::ChooseModel,
            Step::Review,
        ] {
            assert!(!step.accepts_text(), "{step:?} is not a text step");
        }
    }

    #[test]
    fn an_address_containing_shortcut_letters_is_taken_literally() {
        // "w", "q", "s" and "b" are shortcuts on the list screens; typed into an
        // address they must be ordinary characters.
        let mut wizard = at_online();
        let custom = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
            .expect("offered");
        wizard.move_cursor(custom as isize);
        wizard.confirm();

        for ch in "https://qwest.example.com/v1".chars() {
            wizard.push_char(ch);
        }
        assert!(!wizard.quit, "nothing should have quit");
        assert_eq!(
            wizard.step,
            Step::CustomOnline,
            "and nothing should navigate"
        );

        wizard.commit_text();

        assert_eq!(
            wizard.pending_endpoint().map(|(_, url, _)| url),
            Some("https://qwest.example.com/v1".to_string())
        );
    }

    /// Walk to the model step for a hosted endpoint, as if it had been chosen.
    fn at_hosted_model() -> Wizard {
        let mut wizard = at_online();
        let hosted = wizard
            .available_online()
            .iter()
            .position(|choice| choice.id == "openrouter")
            .expect("openrouter is offered");
        wizard.move_cursor(hosted as isize);
        wizard.confirm();
        wizard
    }

    #[test]
    fn a_hosted_endpoint_asks_which_model_to_use() {
        let wizard = at_hosted_model();

        assert_eq!(wizard.step, Step::ChooseModel);
        assert!(wizard.models_loading, "it asks the endpoint what it offers");
        assert_eq!(
            wizard.online_chosen.len(),
            0,
            "nothing is chosen until a model is picked"
        );
    }

    #[test]
    fn picking_an_advertised_model_completes_the_choice() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(
            &endpoint,
            Ok(vec!["model-a".to_string(), "model-b".to_string()]),
        );

        assert!(!wizard.models_loading);
        assert_eq!(wizard.models.len(), 2);

        wizard.confirm();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("model-a"));
    }

    #[test]
    fn a_model_further_down_the_list_can_be_picked() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(
            &endpoint,
            Ok(vec!["first".to_string(), "second".to_string()]),
        );

        wizard.move_cursor(1);
        wizard.confirm();

        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("second"));
    }

    #[test]
    fn the_model_cursor_stops_at_the_type_it_myself_row() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Ok(vec!["only".to_string()]));

        wizard.move_cursor(100);
        // Two rows: the model, then "type it myself".
        assert_eq!(wizard.model_cursor, 1);

        wizard.move_cursor(-100);
        assert_eq!(wizard.model_cursor, 0);
    }

    #[test]
    fn type_it_myself_falls_through_to_typing() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        // A long list is exactly why someone types an id instead of scrolling
        // to it, so the fixture offers several and the user names one.
        wizard.models_arrived(
            &endpoint,
            Ok(vec![
                "advertised".to_string(),
                "also-offered".to_string(),
                "typed-by-hand".to_string(),
            ]),
        );

        // The last row is "type it myself", whatever the list length.
        wizard.move_cursor(100);
        wizard.confirm();

        assert_eq!(wizard.step, Step::OnlineModel);

        for ch in "typed-by-hand".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("typed-by-hand"));
    }

    #[test]
    fn a_typed_model_the_endpoint_does_not_serve_is_refused() {
        // The gap this closes: a hand-typed id that the endpoint does not
        // serve used to be accepted silently, written into the config, and then
        // 404 on the first turn of the first conversation.
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(
            &endpoint,
            Ok(vec![
                "deepseek/deepseek-v4-flash".to_string(),
                "deepseek/deepseek-v4-pro".to_string(),
            ]),
        );

        wizard.move_cursor(100);
        wizard.confirm();
        assert_eq!(wizard.step, Step::OnlineModel);

        // A transposition, which is what a real typo looks like.
        for ch in "deepseek/deepseek-v4-falsh".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(
            wizard.step,
            Step::OnlineModel,
            "it should not have moved on"
        );
        assert!(
            wizard.online_chosen.is_empty(),
            "nothing should have been accepted"
        );
        let problem = wizard.problem.clone().expect("a reason should be shown");
        assert!(problem.contains("not among the 2 models"), "{problem}");
        assert!(
            problem.contains("deepseek/deepseek-v4-flash"),
            "a near miss should be named: {problem}"
        );

        // And the corrected id goes through, so it is a refusal and not a dead
        // end.
        for _ in 0.."deepseek/deepseek-v4-falsh".len() {
            wizard.pop_char();
        }
        for ch in "deepseek/deepseek-v4-flash".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(
            wizard.tiers()[1].model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
    }

    #[test]
    fn a_typed_model_is_let_through_when_the_endpoint_listed_nothing() {
        // An endpoint that will not say what it serves tells us nothing, so
        // nothing is refused: this step exists precisely for that case, and
        // guessing here would block a working setup.
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Err("no list".to_string()));
        assert_eq!(wizard.step, Step::OnlineModel, "it falls back to typing");

        for ch in "some-model".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(
            wizard.online_chosen.len(),
            1,
            "{}",
            wizard.problem.clone().unwrap_or_default()
        );
    }

    #[test]
    fn a_model_list_is_asked_for_once_and_not_again_when_it_arrives() {
        // The guard against re-asking forever. Arriving clears `models_loading`,
        // and this is what then stops saying an answer is still wanted — without
        // it the wizard asked again the moment each answer landed, for as long as
        // the model step was open. Against a real endpoint that is a request per
        // round trip, and every arrival reset the cursor to the top of the list,
        // which put "type it myself" out of reach.
        let mut wizard = at_hosted_model();

        let (_, url, _) = wizard
            .wants_models()
            .expect("the step should be asking for the list");
        assert_eq!(url, "https://openrouter.ai/api/v1", "{url}");

        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Ok(vec!["a-model".to_string()]));

        assert!(
            wizard.wants_models().is_none(),
            "the answer arrived, so it must not ask a second time"
        );
    }

    #[test]
    fn leaving_the_model_step_stops_wanting_the_list() {
        // Otherwise a fetch would be started for a step the user has left, and
        // its answer could land on a screen that is no longer asking.
        let mut wizard = at_hosted_model();
        assert!(wizard.wants_models().is_some());

        wizard.cancel_step();

        assert!(
            wizard.wants_models().is_none(),
            "the step was left, so nothing is wanted"
        );
    }

    #[test]
    fn the_endpoint_identity_matches_what_its_model_list_is_filed_under() {
        // The key the fetch is filed under has to be the key arrival compares,
        // or the answer is thrown away as somebody else's — which is what
        // happened twice here: once when a typed endpoint reported an empty
        // preset id, and once when the shared menu-row id could not tell two
        // typed addresses apart.
        for typed in [false, true] {
            let mut wizard = at_online();
            if typed {
                let custom = wizard
                    .available_online()
                    .iter()
                    .position(|choice| choice.id == CUSTOM_ENDPOINT_ID)
                    .expect("offered");
                wizard.move_cursor(custom as isize);
                wizard.confirm();
                for ch in "https://gateway.example.com/v1".chars() {
                    wizard.push_char(ch);
                }
                wizard.commit_text();
            } else {
                // A shipped endpoint, which does ask for a model.
                let hosted = wizard
                    .available_online()
                    .iter()
                    .position(|choice| {
                        matches!(
                            choice.source,
                            Source::Online {
                                kind: TierKind::OpenAi,
                                ..
                            }
                        )
                    })
                    .expect("a hosted endpoint is offered");
                wizard.move_cursor(hosted as isize);
                wizard.confirm();
            }

            let key = asking(&wizard);
            assert!(
                !key.is_empty(),
                "the key cannot be empty, or nothing matches"
            );

            // The answer filed under that key is accepted...
            let before = wizard.models.len();
            let endpoint = asking(&wizard);
            wizard.models_arrived(&endpoint, Ok(vec!["filed-under-this-key".to_string()]));
            assert_eq!(
                wizard.models.len(),
                before + 1,
                "the answer for the pending endpoint should be taken"
            );

            // ...and one filed under any other address is not.
            wizard.models.clear();
            wizard.models_arrived("http://elsewhere.invalid/v1", Ok(vec!["stale".to_string()]));
            assert!(
                wizard.models.is_empty(),
                "another endpoint's list must not be adopted"
            );
        }
    }

    #[test]
    fn a_late_model_list_for_another_endpoint_is_ignored() {
        // A slow answer landing after the user moved on would otherwise be
        // offered — and checked against — as the wrong endpoint's list.
        let mut wizard = at_hosted_model();
        wizard.models_arrived(
            "http://an-address-the-user-left/v1",
            Ok(vec!["stale".to_string()]),
        );

        assert!(
            wizard.models.is_empty(),
            "another endpoint's models must not be adopted: {:?}",
            wizard.models
        );
    }

    #[test]
    fn a_failed_lookup_falls_back_to_typing_with_the_reason() {
        let mut wizard = at_hosted_model();

        let endpoint = asking(&wizard);

        wizard.models_arrived(
            &endpoint,
            Err("HTTP 401 — check that the key is set".to_string()),
        );

        // Not a dead end: many endpoints serve chats without listing models.
        assert_eq!(wizard.step, Step::OnlineModel);
        assert!(
            wizard
                .models_problem
                .as_deref()
                .is_some_and(|message| message.contains("401")),
            "{:?}",
            wizard.models_problem
        );
    }

    #[test]
    fn an_endpoint_that_lists_nothing_falls_back_to_typing() {
        let mut wizard = at_hosted_model();

        let endpoint = asking(&wizard);

        wizard.models_arrived(&endpoint, Ok(Vec::new()));

        assert_eq!(wizard.step, Step::OnlineModel);
        assert!(wizard.models_problem.is_some());
    }

    #[test]
    fn a_late_answer_is_ignored_once_the_user_has_moved_on() {
        let mut wizard = at_hosted_model();
        wizard.cancel_step();
        assert_eq!(wizard.step, Step::ChooseOnline);

        let endpoint = asking(&wizard);

        wizard.models_arrived(&endpoint, Ok(vec!["arrived-late".to_string()]));

        assert_eq!(wizard.step, Step::ChooseOnline, "it must not jump back");
        assert!(wizard.models.is_empty());
    }

    #[test]
    fn backing_out_of_the_model_step_returns_to_the_endpoint_list() {
        let mut wizard = at_hosted_model();

        wizard.cancel_step();

        assert_eq!(wizard.step, Step::ChooseOnline);
        assert!(wizard.pending.is_none(), "nothing should be half-chosen");
        assert!(wizard.models.is_empty());
    }

    #[test]
    fn an_empty_typed_model_id_is_refused_with_a_reason() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Err("no list".to_string()));
        assert_eq!(wizard.step, Step::OnlineModel);

        wizard.commit_text();

        assert_eq!(wizard.step, Step::OnlineModel, "it stays put");
        assert!(wizard.problem.is_some());
    }

    #[test]
    fn writing_is_blocked_while_a_tier_cannot_answer() {
        // The README says every choice is checked before anything is written.
        // Nothing enforced that: a failed check was drawn and `w` wrote anyway,
        // so a tier that could never answer reached the config and failed on the
        // first turn instead of here.
        let mut wizard = at_online();
        wizard.confirm();
        wizard.confirm();
        assert_eq!(wizard.step, Step::Review);

        // Both of the two tiers that are checked report a problem.
        wizard.note_check(
            0,
            Readiness {
                ok: false,
                detail: "could not reach the server".to_string(),
            },
        );
        wizard.note_check(
            1,
            Readiness {
                ok: false,
                detail: "not installed".to_string(),
            },
        );

        let blocked = wizard.write_blocked().expect("it should be blocked");
        assert!(blocked.contains("2 of 2"), "{blocked}");
        assert!(
            blocked.contains("cannot answer"),
            "it should say why: {blocked}"
        );
    }

    #[test]
    fn writing_is_blocked_while_the_checks_are_still_out() {
        // Writing before an answer arrives is writing unchecked, which is the
        // thing the guard exists to prevent.
        let mut wizard = at_online();
        wizard.confirm();
        wizard.confirm();

        let blocked = wizard.write_blocked().expect("it should be blocked");
        assert!(blocked.contains("still checking"), "{blocked}");
        assert!(
            !wizard.write_blocked().is_none(),
            "and it is not ready, so nothing is written"
        );
    }

    #[test]
    fn writing_is_allowed_once_every_tier_is_ready() {
        let mut wizard = at_online();
        wizard.confirm();
        wizard.confirm();

        for index in 0..wizard.tiers().len() {
            wizard.note_check(
                index,
                Readiness {
                    ok: true,
                    detail: "would use a model".to_string(),
                },
            );
        }

        assert!(
            wizard.write_blocked().is_none(),
            "nothing is wrong with this config: {:?}",
            wizard.write_blocked()
        );
        assert_eq!(wizard.write_blocked(), None);
    }

    #[test]
    fn writing_is_blocked_when_no_tier_was_chosen() {
        let wizard = Wizard::new(&Library::embedded());
        let blocked = wizard.write_blocked().expect("there is nothing to write");
        assert!(blocked.contains("no tiers"), "{blocked}");
    }

    #[test]
    fn a_single_failing_tier_is_named_as_one() {
        let mut wizard = at_online();
        wizard.confirm();
        wizard.confirm();
        wizard.note_check(
            0,
            Readiness {
                ok: true,
                detail: "fine".to_string(),
            },
        );
        wizard.note_check(
            1,
            Readiness {
                ok: false,
                detail: "not installed".to_string(),
            },
        );

        let blocked = wizard.write_blocked().expect("it should be blocked");
        assert!(blocked.contains("1 of 2"), "{blocked}");
        assert!(
            blocked.contains("fix or remove it before writing"),
            "one tier reads as singular: {blocked}"
        );
    }

    #[test]
    fn two_online_choices_finish_the_choosing() {
        let mut wizard = at_online();
        wizard.confirm(); // command-code
        wizard.confirm(); // the next CLI offered

        assert_eq!(wizard.online_chosen.len(), MAX_ONLINE);
        assert_eq!(wizard.step, Step::Review);
        assert_eq!(wizard.tiers().len(), 3, "local plus two fallbacks");
    }

    #[test]
    fn the_same_fallback_cannot_be_chosen_twice() {
        let mut wizard = at_online();
        wizard.confirm();
        let taken = wizard.online_chosen[0].id.clone();

        assert!(
            !wizard.available_online().iter().any(|c| c.id == taken),
            "an already-chosen tier should leave the list"
        );
    }

    #[test]
    fn fewer_than_two_fallbacks_is_allowed() {
        let mut wizard = at_online();
        wizard.confirm();

        wizard.skip_online();

        assert_eq!(wizard.step, Step::Review);
        assert_eq!(wizard.tiers().len(), 2);
    }

    #[test]
    fn no_fallbacks_at_all_is_allowed() {
        let mut wizard = at_online();

        wizard.skip_online();

        assert_eq!(wizard.tiers().len(), 1, "just the local model");
    }

    /// Point the cursor at a local option by id.
    fn select_local(wizard: &mut Wizard, id: &str) {
        wizard.local_cursor = wizard
            .local
            .iter()
            .position(|choice| choice.id == id)
            .unwrap_or_else(|| panic!("no local option called {id}"));
    }

    #[test]
    fn a_typed_address_becomes_the_local_tier() {
        let mut wizard = wizard();
        wizard.local_probed(Vec::new());
        select_local(&mut wizard, "custom");
        wizard.confirm();
        assert_eq!(wizard.step, Step::CustomLocal);

        for ch in "http://192.168.1.50:1234/v1".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.step, Step::ChooseOnline);
        let tiers = wizard.tiers();
        assert_eq!(
            tiers[0].base_url.as_deref(),
            Some("http://192.168.1.50:1234/v1")
        );
        assert_eq!(tiers[0].preset, None, "a typed address is not a preset");
    }

    #[test]
    fn a_typed_address_without_a_scheme_is_refused_with_a_reason() {
        let mut wizard = wizard();
        wizard.local_probed(Vec::new());
        select_local(&mut wizard, "custom");
        wizard.confirm();

        for ch in "192.168.1.50:1234".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.step, Step::CustomLocal);
        assert!(
            wizard
                .problem
                .as_deref()
                .is_some_and(|message| message.contains("http://")),
            "{:?}",
            wizard.problem
        );
    }

    #[test]
    fn an_empty_address_is_refused_with_a_reason() {
        let mut wizard = wizard();
        wizard.local_probed(Vec::new());
        select_local(&mut wizard, "custom");
        wizard.confirm();
        wizard.commit_text();

        assert_eq!(wizard.step, Step::CustomLocal);
        assert!(wizard.problem.is_some());
    }

    #[test]
    fn backing_out_of_a_text_step_returns_to_the_list() {
        let mut wizard = at_hosted_model();
        let endpoint = asking(&wizard);
        wizard.models_arrived(&endpoint, Err("no list".to_string()));
        assert_eq!(wizard.step, Step::OnlineModel);

        wizard.cancel_step();

        assert_eq!(wizard.step, Step::ChooseOnline);
        assert!(wizard.pending.is_none());
    }

    #[test]
    fn typing_can_be_corrected() {
        let mut wizard = wizard();
        wizard.local_probed(Vec::new());
        select_local(&mut wizard, "custom");
        wizard.confirm();

        for ch in "http://aax".chars() {
            wizard.push_char(ch);
        }
        wizard.pop_char();
        assert_eq!(wizard.text, "http://aa");

        wizard.push_char('b');
        wizard.commit_text();
        assert_eq!(wizard.tiers()[0].base_url.as_deref(), Some("http://aab"));
    }

    // ---- generated file ---------------------------------------------------

    #[test]
    fn the_generated_file_is_valid_configuration() {
        let mut wizard = at_online();
        wizard.confirm();
        wizard.confirm();

        let text = wizard.to_toml();
        let parsed = Config::parse(std::path::Path::new("generated.toml"), &text)
            .expect("the wizard must produce a config that loads");

        assert_eq!(parsed.tiers.len(), 3);
        assert_eq!(parsed.tiers[0].id, "local");
        assert_eq!(parsed.tiers[1].kind, TierKind::Cli);
        assert_eq!(parsed.tiers[2].kind, TierKind::Cli);
        // Unattended runs are never turned on by setup.
        assert!(parsed.tiers.iter().all(|tier| !tier.approve_all));
    }

    #[test]
    fn the_generated_file_explains_itself() {
        let mut wizard = at_online();
        wizard.confirm();
        let text = wizard.to_toml();

        assert!(text.contains("spill presets"), "{text}");
        assert!(text.contains("tried in order"), "{text}");
        assert!(text.contains("sticky_fallback = true"), "{text}");
    }

    #[test]
    fn a_config_with_only_a_local_tier_is_valid() {
        let wizard = at_online();
        let text = wizard.to_toml();
        let parsed = Config::parse(std::path::Path::new("generated.toml"), &text).expect("valid");
        assert_eq!(parsed.tiers.len(), 1);
    }

    // ---- writing ----------------------------------------------------------

    #[test]
    fn writing_creates_the_file_with_no_backup_the_first_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spill").join("config.toml");

        let backup = write_config(&path, "schema = 1\n").expect("write");

        assert!(backup.is_none());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "schema = 1\n"
        );
    }

    #[test]
    fn writing_over_a_config_keeps_a_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old = true\n").expect("seed");

        let backup = write_config(&path, "schema = 1\n").expect("write");

        let backup = backup.expect("the old file should have been kept");
        assert_eq!(
            std::fs::read_to_string(backup).expect("read"),
            "old = true\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "schema = 1\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_written_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        write_config(&path, "schema = 1\n").expect("write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }

    #[test]
    fn a_temporary_file_is_not_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        write_config(&path, "schema = 1\n").expect("write");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::*;

    /// Is anything installed? Every case passes its own answer, because PATH is
    /// process-wide and the thing under test is the *choice*, not the lookup.
    fn only(bins: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |bin: &str| bins.contains(&bin)
    }

    #[test]
    fn the_fallback_is_chosen_in_the_declared_order_not_the_library_order() {
        // The library happens to list `command-code` first, which is the last
        // thing a first run should pick. Both are installed; the preference
        // order is what decides.
        let library = Library::embedded();
        let chosen = first_run_fallback_with(&library, only(&["grok", "cmd"]));

        assert_eq!(chosen.map(|preset| preset.id.as_str()), Some("grok"));
    }

    #[test]
    fn a_cli_that_is_installed_but_not_the_first_choice_is_still_chosen() {
        let library = Library::embedded();
        let chosen = first_run_fallback_with(&library, only(&["opencode"]));
        assert_eq!(chosen.map(|preset| preset.id.as_str()), Some("opencode"));
    }

    #[test]
    fn a_cli_that_is_not_installed_is_skipped_for_the_next_one() {
        // The bug the naive `find(...).filter(...)` has: the first preset that
        // *exists in the library* is checked, found missing, and the search
        // stops there instead of trying the next.
        let library = Library::embedded();
        let chosen = first_run_fallback_with(&library, only(&["codex"]));
        assert_eq!(
            chosen.map(|preset| preset.id.as_str()),
            Some("codex"),
            "claude is missing, so codex is the answer"
        );
    }

    #[test]
    fn no_agent_cli_at_all_is_a_normal_answer() {
        // Not an error: plenty of machines have none, and an all-local chain is
        // a legitimate way to run.
        let library = Library::embedded();
        assert!(first_run_fallback_with(&library, only(&[])).is_none());
    }

    #[test]
    fn a_first_run_fallback_is_shaped_like_the_wizards_own_choice() {
        // Both paths must produce the same config for the same decision, or the
        // first run and `spill setup` would disagree about a tier someone then
        // edits by hand.
        let library = Library::embedded();
        let grok = library
            .cli
            .iter()
            .find(|preset| preset.id == "grok")
            .expect("the grok preset ships");

        let tier = tier_for_cli(grok);

        assert_eq!(tier.id, "grok");
        assert_eq!(tier.kind, TierKind::Cli);
        assert_eq!(tier.name.as_deref(), Some(grok.name.as_str()));
        assert_eq!(tier.preset.as_deref(), Some("grok"));
        assert_eq!(tier.bin.as_deref(), Some("grok"));
        assert_eq!(tier.model, None, "a CLI tier's model is its own business");
        // The configured default, which for a first run's fallback happens to be
        // inert either way: it is the last tier, so a stall ends the turn rather
        // than consulting or handing over. Asserted so that a first run and
        // `spill setup` keep writing the same thing for the same decision.
        assert_eq!(tier.on_stuck, OnStuck::default());
    }

    #[test]
    fn every_declared_fallback_names_a_preset_that_ships() {
        // A typo here would silently drop a CLI from consideration, which is
        // invisible until someone with only that one installed gets no fallback.
        let library = Library::embedded();
        for id in FIRST_RUN_FALLBACKS {
            assert!(
                library.cli.iter().any(|preset| preset.id == *id),
                "{id} is in the preference order but no preset ships with that id"
            );
        }
    }
}
