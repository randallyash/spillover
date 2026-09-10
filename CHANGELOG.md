# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-10

First release. Everything below is new.

### Added

- Configuration loading from `~/.config/spill/config.toml`, with validation that
  reports the offending tier and field rather than failing opaquely. `schema`,
  `[general]`, and repeated `[[tier]]` tables are supported, and both `openai` and
  `cli` tier kinds are accepted.
- An example configuration (`config.example.toml`) that is parsed by the test suite,
  so it cannot silently rot.
- Terminal UI foundation: alternate-screen and raw-mode handling via an RAII guard
  that restores the terminal on panic or early exit, a dedicated input thread feeding
  the event loop, a transcript pane with display-width-aware wrapping, a prompt
  editor with a caret, a tier-chain header, and scrollback.
- `--config <PATH>` to point at a configuration file other than the default.
- OpenAI-compatible streaming over SSE, covering LM Studio, Ollama, llama.cpp,
  vLLM, OpenRouter and other hosted APIs. Tool calls are reassembled correctly
  from the fragments servers split them across, and a bare 404 suggests the
  common cause: a `base_url` missing its `/v1` segment.
- Model auto-discovery: leaving `model` empty takes the first model the endpoint
  advertises, so a local server needs no model id typed in.
- An agent loop with seven tools — `read_file`, `list_dir`, `glob`, `grep`,
  `write_file`, `edit_file` and `run_shell` — which feeds tool results and errors
  back to the model and stops after a bounded number of steps.
- An approval prompt for anything that can write: the exact command or diff is
  shown before it runs, and a refusal is reported to the model rather than
  silently dropped. Read-only tools never prompt.
- Transcript reporting of tool calls, their results, token usage, and a warning
  when the model was cut off by its output limit.
- Stuck detection: a repeated line or recurring token span, the same tool call with
  identical arguments, a run of failed calls, silence past the tier's allowance, and
  the step budget running out. Each tier carries its own thresholds.
- Tier escalation: an ordered chain, stepped down one tier at a time and reported in
  the transcript with the reason. Escalation discards the failed tier's conversation —
  so the next tier never inherits a looping answer — while keeping the results of
  any tool that already ran. A sticky chain stays where it fell to; a non-sticky one
  gives the top tier another chance on the next turn.
- A streaming turn is cancelled when it is abandoned, rather than left generating
  into a channel nobody reads.
- Delegated CLI tiers: an agent CLI can serve as a tier, spawned with an argument
  vector (never a shell string, so a prompt full of quotes and semicolons is just
  text) and streamed back line by line. Three output dialects are understood —
  plain text, Command Code's NDJSON, and Grok Build's `streaming-json` — with plain
  text covering the default print mode of nearly every agent CLI.
- A preset library compiled into the binary: local servers, hosted endpoints, and
  agent CLIs, each with an invocation shape checked against that tool's own
  documentation or `--help`. `spill presets` lists them. A tier can name a preset
  instead of spelling out a URL or an argument vector, and its own settings always
  win over the preset's.
- Startup checks that report what would otherwise fail later: an `approve_all` tier
  that runs unattended, a key variable that is not set, an agent CLI that is not on
  `PATH`, and a `base_url` missing its `/v1`.
- Unattended CLI runs are opt-in. `approve_all` defaults to off, so a CLI tier keeps
  its own permission prompts unless you ask otherwise.
- CI running `cargo fmt --check`, `clippy -D warnings` and the test suite on Linux,
  macOS and Windows, plus a gitleaks job so a credential can never be committed.
- `spill setup`: an in-terminal wizard that follows the shape of the tool — one
  local model first, then up to two online fallbacks in the order you choose. It
  probes for local servers, offers the shipped agent CLIs, asks a hosted endpoint
  for the model id it should use, and checks every tier before writing anything.
  An existing config is copied to `config.toml.bak` and confirmed before it is
  replaced, and the new file is written atomically with mode `0600`.
- Zero-config first run: with no config at all, spill probes the usual local ports
  and starts on whatever is running, saying which server it adopted. Setup is only
  opened when there is nothing to find.
- `spill presets` lists every endpoint and agent CLI a tier can name.
- Choosing a hosted endpoint asks it which models it offers and presents them, so
  a model id is picked rather than typed. An endpoint that cannot list its models
  — no key yet, or a server that does not advertise them — falls back to typing,
  with the reason shown.
- A last row in the endpoint list takes the address of anything not in the shipped
  presets, such as a company gateway, and reads its models the same way.
- `spill doctor`: every configured tier checked in one report, with the model each
  would use, how long it took, and why anything failed. `--json` for scripts, a
  non-zero exit when nothing is usable, and key *names* rather than key values, so
  the output can be pasted into an issue.
- `spill -p "…"`: answer one prompt and exit, with the same tiers and the same
  spill-over. Writes and shell commands are refused when there is nobody to
  approve them, unless `--yolo` is passed. `--output-format json` reports the
  answer, the tier that gave it, and any tiers the run spilled through.
- A clearer message when the interface is started without a terminal, pointing at
  `-p` and `doctor` instead of a bare OS error.
- Release artifacts via cargo-dist: archives for five platforms, shell and
  PowerShell installers, a Windows `.msi`, a Homebrew formula published to a tap,
  and checksums, with an AUR `PKGBUILD` and a script that verifies a published
  tarball installs and runs.
- On Windows, a path written in double quotes is rejected with an explanation.
  `workspace = "C:\Users\me"` is not valid TOML, and the worse case —
  `"C:\x86"` — is a *valid* escape that parses into a path holding a control
  character, so nothing works for reasons that stay invisible. Write it in single
  quotes instead. `config.example.toml` says so too.
- The test suite runs on Linux, macOS and Windows, and the platform-specific
  parts of it use the shell and separators of whichever platform is running
  rather than assuming a POSIX one.
- Homebrew: the formula is published to the
  [`randallyash/spillover`](https://github.com/randallyash/homebrew-spillover)
  tap, so `brew install randallyash/spillover/spill` works.

[0.1.0]: https://github.com/randallyash/spillover/releases/tag/v0.1.0
