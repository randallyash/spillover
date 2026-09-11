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

use crate::config::{Limits, Tier, TierKind};
use crate::preset::Library;
use crate::setup::probe::{Found, Readiness};

/// How many online fallbacks the wizard offers, after the local model.
pub const MAX_ONLINE: usize = 2;

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
    pub fn pending_endpoint(&self) -> Option<(String, String, Option<String>)> {
        match self.pending.as_ref()?.source.clone() {
            Source::Online {
                preset_id,
                base_url: Some(base_url),
                api_key_env,
                ..
            } => Some((preset_id, base_url, api_key_env)),
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
    pub fn models_arrived(&mut self, result: Result<Vec<String>, String>) {
        if self.step != Step::ChooseModel {
            // The user moved on while this was in flight.
            return;
        }
        self.models_loading = false;

        match result {
            Ok(models) if !models.is_empty() => {
                self.models = models;
                self.model_cursor = 0;
            }
            Ok(_) => {
                self.models_problem =
                    Some("this endpoint listed no models, so type the id to use".to_string());
                self.begin_text();
                self.step = Step::OnlineModel;
            }
            Err(message) => {
                // Not being able to list models is not a dead end; many
                // endpoints serve chat completions without advertising them.
                self.models_problem = Some(format!("{message} — type the id to use"));
                self.begin_text();
                self.step = Step::OnlineModel;
            }
        }
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
        workdir_args: Vec::new(),
        // Session flags come from the preset, which is what makes continuity
        // work without the wizard knowing anything about it.
        session_args: Vec::new(),
        resume_args: Vec::new(),
        dialect: None,
        // Unattended runs are never something setup turns on for you.
        approve_all: false,
        limits: Limits::default(),
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
        wizard.models_arrived(Ok(vec!["their-model".to_string()]));
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
        wizard.models_arrived(Err("no list".to_string()));
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
        wizard.models_arrived(Ok(vec!["model-a".to_string(), "model-b".to_string()]));

        assert!(!wizard.models_loading);
        assert_eq!(wizard.models.len(), 2);

        wizard.confirm();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("model-a"));
    }

    #[test]
    fn a_model_further_down_the_list_can_be_picked() {
        let mut wizard = at_hosted_model();
        wizard.models_arrived(Ok(vec!["first".to_string(), "second".to_string()]));

        wizard.move_cursor(1);
        wizard.confirm();

        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("second"));
    }

    #[test]
    fn the_model_cursor_stops_at_the_type_it_myself_row() {
        let mut wizard = at_hosted_model();
        wizard.models_arrived(Ok(vec!["only".to_string()]));

        wizard.move_cursor(100);
        // Two rows: the model, then "type it myself".
        assert_eq!(wizard.model_cursor, 1);

        wizard.move_cursor(-100);
        assert_eq!(wizard.model_cursor, 0);
    }

    #[test]
    fn type_it_myself_falls_through_to_typing() {
        let mut wizard = at_hosted_model();
        wizard.models_arrived(Ok(vec!["advertised".to_string()]));

        wizard.move_cursor(1); // the "type it myself" row
        wizard.confirm();

        assert_eq!(wizard.step, Step::OnlineModel);

        for ch in "my-own-model".chars() {
            wizard.push_char(ch);
        }
        wizard.commit_text();

        assert_eq!(wizard.online_chosen.len(), 1);
        assert_eq!(wizard.tiers()[1].model.as_deref(), Some("my-own-model"));
    }

    #[test]
    fn a_failed_lookup_falls_back_to_typing_with_the_reason() {
        let mut wizard = at_hosted_model();

        wizard.models_arrived(Err("HTTP 401 — check that the key is set".to_string()));

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

        wizard.models_arrived(Ok(Vec::new()));

        assert_eq!(wizard.step, Step::OnlineModel);
        assert!(wizard.models_problem.is_some());
    }

    #[test]
    fn a_late_answer_is_ignored_once_the_user_has_moved_on() {
        let mut wizard = at_hosted_model();
        wizard.cancel_step();
        assert_eq!(wizard.step, Step::ChooseOnline);

        wizard.models_arrived(Ok(vec!["arrived-late".to_string()]));

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
        wizard.models_arrived(Err("no list".to_string()));
        assert_eq!(wizard.step, Step::OnlineModel);

        wizard.commit_text();

        assert_eq!(wizard.step, Step::OnlineModel, "it stays put");
        assert!(wizard.problem.is_some());
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
        wizard.models_arrived(Err("no list".to_string()));
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
