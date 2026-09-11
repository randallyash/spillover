//! Terminal lifecycle and the main event loop.

mod agent;
mod app;
mod commands;
mod config;
mod detect;
mod doctor;
mod event;
mod fallback;
mod oneshot;
mod preset;
mod provider;
mod session;
mod setup;
mod spawn;
mod text;
mod tiers;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use clap::Parser;
use crossterm::cursor;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyCode,
    KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use std::sync::Arc;

use crate::agent::approval::{ApprovalRequest, UiApprover};
use crate::agent::tools::Registry;
use crate::agent::{AgentConfig, AgentEvent};
use crate::app::{App, Message};
use crate::config::Config;
use crate::event::InputEvent;
use crate::setup::probe::Readiness;
use crate::setup::{Step, Wizard};

#[derive(Debug, Parser)]
#[command(
    name = "spill",
    version,
    about = "Agentic AI TUI that spills over across model tiers",
    long_about = "Runs your prompt on a local model first, and spills over to the next tier \
                  when that tier loops, stalls, or fails.\n\n\
                  With no configuration, spill looks for a local server on the usual ports and \
                  just starts. Run `spill setup` to choose your tiers."
)]
struct Cli {
    /// Path to the config file (default: ~/.config/spill/config.toml)
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Answer one prompt and exit, without the interface.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// With -p, print a JSON object instead of the answer text.
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    output_format: OutputFormat,

    /// With -p, let the model write files and run commands without asking.
    #[arg(long)]
    yolo: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// List the endpoints and agent CLIs a tier can name
    Presets,
    /// Choose your tiers and write a config file
    Setup,
    /// Check what spill can reach, and why anything it cannot
    Doctor {
        /// Print the report as JSON, for a script to read
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Presets) => {
            println!("{}", crate::preset::Library::embedded().catalogue());
            println!("\nName one in a tier with preset = \"<id>\".");
            return;
        }
        Some(Command::Setup) => {
            let path = match config_path(cli.config.as_deref()) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("spill: {error}");
                    std::process::exit(1);
                }
            };
            match run_wizard(path.clone()).await {
                Ok(Some(written)) => {
                    println!("Wrote {}.", written.display());
                    println!("Run `spill` to start.");
                }
                Ok(None) => println!("Nothing was written."),
                Err(error) => {
                    eprintln!("spill: {error}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(Command::Doctor { json }) => {
            let config = match Config::load(cli.config.as_deref()) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("spill: {error}");
                    std::process::exit(1);
                }
            };

            let report = doctor::diagnose(&preset::Library::embedded(), &config).await;
            if json {
                println!("{}", report.to_json());
            } else {
                println!("{}", report.render());
            }
            std::process::exit(report.exit_code());
        }
        None => {}
    }

    // Without a prompt, this is the interactive app.
    let prompt = match cli.print {
        Some(prompt) => prompt,
        None => {
            if cli.yolo {
                eprintln!("spill: --yolo only means something with -p");
            }
            let config = match Config::load(cli.config.as_deref()) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("spill: {error}");
                    std::process::exit(1);
                }
            };
            if let Err(error) = run(config).await {
                eprintln!("spill: {error}");
                std::process::exit(1);
            }
            return;
        }
    };

    // `-p`: answer once and exit.
    let config = match Config::load(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("spill: {error}");
            std::process::exit(1);
        }
    };

    let options = oneshot::Options {
        prompt,
        json: cli.output_format == OutputFormat::Json,
        yolo: cli.yolo,
    };
    let outcome = oneshot::run(&preset::Library::embedded(), &config, &options).await;

    if options.json {
        println!("{}", outcome.to_json());
    } else if let Some(failure) = &outcome.failure {
        eprintln!("spill: {failure}");
    } else {
        println!("{}", outcome.text);
    }
    std::process::exit(outcome.exit_code());
}

/// How often the interface redraws while a turn is running.
///
/// Fast enough that the spinner reads as motion rather than as a sequence of
/// stills, slow enough that a redraw is nothing next to the model it is waiting
/// on.
const TICK: std::time::Duration = std::time::Duration::from_millis(90);

async fn run(mut config: Config) -> io::Result<()> {
    // Nothing configured. Look for a model that is already running before
    // asking anyone anything, and only interrupt with setup if there is nothing
    // to find.
    let mut first_run: Vec<String> = Vec::new();
    if config.tiers.is_empty() {
        let library = crate::preset::Library::embedded();
        let found = crate::setup::probe::find_local(&library.local).await;

        match found.first() {
            Some(server) => {
                config.tiers.push(crate::setup::tier_for_local(server));
                first_run.push(format!(
                    "Nothing is configured yet, so this is the {} found on this machine ({}). \
                     Run `spill setup` to choose your tiers and write a config.",
                    server.name, server.base_url
                ));
            }
            None => {
                println!("No config yet, and no local model is running.");
                println!("Let's set one up.\n");

                let path = config_path(None)?;
                match run_wizard(path.clone()).await? {
                    Some(written) => match Config::load(Some(&written)) {
                        Ok(loaded) => config = loaded,
                        Err(error) => {
                            eprintln!("spill: the config just written could not be read: {error}");
                            return Ok(());
                        }
                    },
                    None => {
                        println!("Nothing was written. Run `spill setup` when you are ready.");
                        return Ok(());
                    }
                }
            }
        }
    }

    let mut app = App::new(config.clone());
    for note in first_run {
        app.messages.push(Message::system(note));
    }

    // Bring the agent up before entering the alternate screen, so a setup
    // failure reads as ordinary terminal output rather than a flash of UI.
    let mut agent_events = None;
    let mut approvals = None;
    match start_agent(&config).await {
        AgentStart::Ready(channels) => {
            app.attach(
                channels.commands,
                channels.cancel,
                &channels.tier_labels,
                channels.warning,
            );
            agent_events = Some(channels.events);
            approvals = Some(channels.approvals);
        }
        AgentStart::Unavailable(message) => {
            app.messages.push(Message::system(message));
        }
    }

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut input_rx) = mpsc::unbounded_channel::<InputEvent>();
    event::spawn_input_thread(tx);

    let mut ticker = tokio::time::interval(TICK);
    // A redraw missed because the loop was busy should be skipped rather than
    // replayed in a burst to catch up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;
            input = input_rx.recv() => match input {
                Some(InputEvent::Key(key)) => app.handle_key(key),
                Some(InputEvent::Paste(text)) => app.paste(&text),
                // Resizing via the backend, not `Terminal::clear`, which can hang
                // waiting on a terminal size report.
                Some(InputEvent::Resize { cols, rows }) => {
                    terminal.resize(Rect::new(0, 0, cols, rows))?;
                }
                None => break,
            },
            request = next_or_pending(&mut approvals) => match request {
                Some(request) => app.set_approval(request),
                // The agent has gone; stop listening rather than spinning on a
                // closed channel.
                None => approvals = None,
            },
            // The spinner and the streaming caret are the only things here that
            // move without input, so the timer runs only while a turn is in
            // flight. An idle app redraws no more often than it ever did.
            //
            // It is polled ahead of the agent's events on purpose: while a model
            // is streaming, those events can arrive faster than a frame, and
            // `biased` would let them win every time and freeze the caret that
            // exists to show the stream is still alive. A tick that is not due
            // yet costs nothing and falls through to the next branch.
            _ = ticker.tick(), if app.busy => {
                app.tick = app.tick.wrapping_add(1);
            },
            event = next_or_pending(&mut agent_events) => match event {
                Some(event) => app.handle_agent_event(event),
                None => {
                    agent_events = None;
                    app.messages.push(Message::system("the agent stopped running"));
                }
            },
        }
    }

    Ok(())
}

/// Await the next item from a channel that may have been put away.
///
/// `None` disables the branch entirely. Without this a closed channel would
/// keep resolving with `None` and win every `select!`, starving the others —
/// which is exactly how the wizard's checks failed to appear.
async fn next_or_pending<T>(slot: &mut Option<UnboundedReceiver<T>>) -> Option<T> {
    match slot {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

struct AgentChannels {
    commands: UnboundedSender<crate::agent::Command>,
    events: UnboundedReceiver<AgentEvent>,
    approvals: UnboundedReceiver<ApprovalRequest>,
    /// How the interface stops a turn that is already running.
    cancel: crate::agent::Canceller,
    /// The chain, in order, under the labels the agent reports tiers by, so the
    /// header rail can tell which one is answering.
    tier_labels: Vec<String>,
    /// Anything the user should know about tiers that were left out.
    warning: Option<String>,
}

enum AgentStart {
    Ready(Box<AgentChannels>),
    /// Why no tier could be started, phrased for the user.
    Unavailable(String),
}

/// Build the chain and start the agent.
///
/// Every tier kind is supported and nothing is dropped quietly: a tier that
/// cannot be built stops startup with the reason, rather than shortening the
/// chain into a fallback that never happens.
async fn start_agent(config: &Config) -> AgentStart {
    let workspace = config.general.workspace_path();
    let library = crate::preset::Library::embedded();

    let tiers = match crate::tiers::build(&library, config, &workspace).await {
        Ok(tiers) => tiers,
        Err(error) => return AgentStart::Unavailable(error),
    };

    let notes = crate::tiers::notes(&library, config);
    let warning = (!notes.is_empty()).then(|| notes.join("\n"));

    let chain = match crate::tiers::chain(config, tiers) {
        Some(chain) => chain,
        None => return AgentStart::Unavailable("no usable tiers".to_string()),
    };
    let tier_labels = chain.labels();

    let (approval_tx, approval_rx) = mpsc::unbounded_channel();
    // Created here and shared, so the interface can stop a turn the agent is in
    // the middle of.
    let canceller = crate::agent::Canceller::default();
    let (commands, events) = crate::agent::spawn(
        AgentConfig {
            workspace,
            max_steps: crate::agent::DEFAULT_MAX_STEPS,
            cancel: canceller.clone(),
        },
        chain,
        Arc::new(Registry::with_default_tools()),
        Arc::new(UiApprover::new(approval_tx)),
    );

    AgentStart::Ready(Box::new(AgentChannels {
        commands,
        events,
        approvals: approval_rx,
        cancel: canceller,
        tier_labels,
        warning,
    }))
}

/// Where the config lives, given what was passed on the command line.
fn config_path(explicit: Option<&std::path::Path>) -> io::Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => crate::config::default_path()
            .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error.to_string())),
    }
}

/// Run the setup wizard, returning the path it wrote if it wrote one.
async fn run_wizard(path: PathBuf) -> io::Result<Option<PathBuf>> {
    let library = crate::preset::Library::embedded();
    let mut wizard = Wizard::new(&library);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<InputEvent>();
    event::spawn_input_thread(input_tx);

    // Look for local servers while the first screen is already up, rather than
    // making the user watch a blank one.
    let (found_tx, found_rx) = mpsc::unbounded_channel();
    let mut found_rx = Some(found_rx);
    let candidates = library.local.clone();
    tokio::spawn(async move {
        let _ = found_tx.send(crate::setup::probe::find_local(&candidates).await);
    });

    // Checks start when the review screen appears, and are keyed on how many
    // tiers there are so that going back and changing them re-runs them.
    let (check_tx, check_rx) = mpsc::unbounded_channel::<(usize, Readiness)>();
    let mut check_rx = Some(check_rx);

    // The models an endpoint advertises, fetched when one is chosen.
    let (models_tx, models_rx) = mpsc::unbounded_channel::<Result<Vec<String>, String>>();
    let mut models_rx = Some(models_rx);
    // The endpoint whose models are being fetched, so a stale answer for a
    // different endpoint is not mistaken for this one's.
    let mut fetching_for: Option<String> = None;
    let mut checked: Option<usize> = None;
    let mut confirm_replace = false;
    let mut written = None;

    loop {
        terminal.draw(|frame| {
            crate::setup::ui::render(frame, &wizard);
            if confirm_replace {
                crate::setup::ui::confirm_overwrite(
                    frame,
                    &path.display().to_string(),
                    &crate::ui::theme::Theme::default(),
                );
            }
        })?;

        if wizard.quit {
            break;
        }
        if let Some(done) = wizard.written.take() {
            written = Some(done);
            break;
        }

        let tier_count = wizard.tiers().len();
        if wizard.step == Step::Review && checked != Some(tier_count) {
            checked = Some(tier_count);
            wizard.clear_checks();
            let library = library.clone();
            let tiers = wizard.tiers();
            let tx = check_tx.clone();
            // Reported one at a time, so a slow tier cannot hide the others.
            tokio::spawn(async move {
                for (index, tier) in tiers.into_iter().enumerate() {
                    let readiness = crate::setup::probe::check(&library, &tier).await;
                    if tx.send((index, readiness)).is_err() {
                        break;
                    }
                }
            });
        }

        // Ask the endpoint which models it offers, once, when the model step
        // opens. Re-entering the step for the same endpoint reuses the answer.
        if wizard.step != Step::ChooseModel {
            fetching_for = None;
        } else if fetching_for.is_none() {
            if let Some((id, base_url, key_env)) = wizard.pending_endpoint() {
                fetching_for = Some(id);
                let tx = models_tx.clone();
                tokio::spawn(async move {
                    let key = key_env.as_deref().and_then(|name| std::env::var(name).ok());
                    let result =
                        crate::provider::openai::list_models(&base_url, key.as_deref()).await;
                    let _ = tx.send(result);
                });
            }
        }

        tokio::select! {
            biased;
            input = input_rx.recv() => {
                let Some(input) = input else { break };
                match input {
                    InputEvent::Resize { cols, rows } => {
                        terminal.resize(Rect::new(0, 0, cols, rows))?;
                    }
                    // The wizard's text steps take typing, not a paste, so a
                    // pasted block is ignored rather than inserted unbidden.
                    InputEvent::Paste(_) => {}
                    InputEvent::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if confirm_replace {
                            confirm_replace = false;
                            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                                write_and_finish(&mut wizard, &path);
                            }
                            continue;
                        }

                        match wizard.step {
                            // While text is being typed, the letters belong to
                            // it. The step itself decides, so a newly added text
                            // step cannot be left out of this routing and have
                            // its input swallowed by the shortcut keys.
                            step if step.accepts_text() => match key.code {
                                KeyCode::Enter => wizard.commit_text(),
                                KeyCode::Backspace => wizard.pop_char(),
                                KeyCode::Esc => wizard.cancel_step(),
                                KeyCode::Char(ch) => wizard.push_char(ch),
                                _ => {}
                            },
                            _ => match key.code {
                                KeyCode::Char('q') => wizard.quit = true,
                                KeyCode::Up => wizard.move_cursor(-1),
                                KeyCode::Down => wizard.move_cursor(1),
                                KeyCode::Enter => wizard.confirm(),
                                KeyCode::Char('s') => wizard.skip_online(),
                                KeyCode::Char('b') => match wizard.step {
                                    Step::ChooseOnline => wizard.step = Step::ChooseLocal,
                                    Step::Review => {
                                        wizard.step = Step::ChooseOnline;
                                        wizard.clear_checks();
                                        checked = None;
                                    }
                                    _ => {}
                                },
                                KeyCode::Char('w') => {
                                    if path.exists() {
                                        confirm_replace = true;
                                    } else {
                                        write_and_finish(&mut wizard, &path);
                                    }
                                }
                                KeyCode::Esc => wizard.quit = true,
                                _ => {}
                            },
                        }
                    }
                }
            }
            found = next_or_pending(&mut found_rx) => match found {
                Some(found) => {
                    wizard.local_probed(found);
                    // One probe is all there is.
                    found_rx = None;
                }
                None => found_rx = None,
            },
            check = next_or_pending(&mut check_rx) => match check {
                Some((index, readiness)) => wizard.note_check(index, readiness),
                None => check_rx = None,
            },
            models = next_or_pending(&mut models_rx) => match models {
                Some(result) => {
                    fetching_for = None;
                    wizard.models_arrived(result);
                }
                None => models_rx = None,
            }
        }
    }

    Ok(written)
}

/// Write the config and mark the wizard finished.
///
/// A failure is reported in the wizard rather than by dropping out of the
/// terminal, so the user keeps their choices and can try somewhere else.
fn write_and_finish(wizard: &mut Wizard, path: &std::path::Path) {
    match crate::setup::write_config(path, &wizard.to_toml()) {
        Ok(_) => wizard.written = Some(path.to_path_buf()),
        Err(error) => {
            wizard.problem = Some(format!("could not write {}: {error}", path.display()));
        }
    }
}

/// Owns raw mode and the alternate screen, and always gives them back.
///
/// Restoring in `Drop` means a panic or an early return still leaves the user
/// with a usable shell rather than a wedged terminal.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        // Raw mode fails with a bare OS error when there is no terminal, which
        // says nothing useful; check first and say what to do instead.
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "spill needs a terminal for its interface. To run without one, use \
                 `spill -p \"your prompt\"`, or `spill doctor` to check your setup.",
            ));
        }

        enable_raw_mode()?;

        let mut stdout = io::stdout();
        // Bracketed paste is what makes a paste arrive as one block of text
        // rather than as a burst of keystrokes, which in raw mode is unreliable
        // for anything long and loses the line breaks in a multi-line paste.
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        stdout.flush()?;

        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            cursor::Show
        );
        let _ = disable_raw_mode();
        let _ = stdout.flush();
    }
}
