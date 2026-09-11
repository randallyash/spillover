# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Token accounting counted only the last request of a turn.** A turn is not one
  request: it makes one per tool call, and every one of them is billed, so a turn
  that read four files made five requests and reported one. The transcript's
  `tokens:` line, `spill -p --output-format json`, `/cost` and the session panel
  were all low by the cost of every step but the last — and low in proportion to
  how much work a turn did, which is the worst direction for a cost figure to be
  wrong in.

  Usage is now accumulated across a turn's requests and reported through a single
  `Spent` event, which is also what an *abandoned* attempt costs: a tier that
  looped and was thrown away was billed for the work it did, and dropping that
  made a spill — the most interesting cost event there is — the least visible
  one. The cost graph still gets one bar per turn rather than per request, so
  spilling does not make one prompt look like two.

  Usage that arrives before a stream ends is now kept too. It used to be read
  only off a *finished* response, so an attempt that was abandoned — a stall, a
  repetition loop, a cancel — threw away a figure the tier had already sent, and
  the cost of a failing tier, which is exactly what someone is trying to measure,
  was invisible. Providers now hand out usage as each frame arrives, and Command
  Code's `model_request_end` and `turn_end` frames are read at all: only its
  final result line was being looked at, so a run killed before the end reported
  nothing despite having said what it spent several frames earlier.

  What cannot be closed remains: a provider reports usage with its response, so
  a stream killed *before any usage frame arrives* has nothing to count, and only
  a provider that billed in arrears could say otherwise.

- `Ctrl-C` quits from anywhere again. The approval prompt and the help overlay
  both swallow every key they do not use, and the check for `Ctrl-C` sat below
  them, so the universal way out was dead in exactly the two places someone might
  most want it: a modal asking about a change they do not understand, and an
  overlay they opened by accident. `Esc` still answers the modal rather than
  quitting; the two keys now mean two different things.
- `/retry` no longer wedges the program. It marked the app busy whether or not
  the command had anywhere to go, so with no agent running every later prompt was
  refused with "still working" and there was no turn left to finish or cancel — it
  never recovered. `/sticky` had the same shape of bug: it recorded a policy the
  chain had never been told about, so the session panel could report one that was
  not in effect. `/clear` cleared the transcript regardless, which wiped the very
  message explaining that nothing had been cleared. All three now commit their
  local state only once the agent has taken the command.
- A delegated CLI no longer inherits spill's model credentials. A `cli` tier hands
  the turn to a harness that signs in for itself, and several of those prefer an
  API key from the environment over the login they hold — so a key exported for
  one of spill's own endpoint tiers would quietly take precedence and bill a
  different account. The `xai` endpoint preset reads `XAI_API_KEY`, so a user with
  that configured beside a `grok` tier had exported exactly the variable that
  would redirect the CLI away from SuperGrok. Those variables are now removed from
  a delegated CLI's environment, everything else is inherited untouched,
  `run_shell` is deliberately left alone, and startup names the variables it
  removed so the removal is never the invisible cause of a CLI that cannot find
  its key.
- `Esc` stops a turn instead of quitting the program. It used to kill the whole
  session, which meant a model forty seconds into a bad answer left no way out
  but losing the conversation. Stopping is deliberately not the same as spilling
  over: the tier stays the one you chose, the work already in the conversation
  stays there, and the half-generated answer — which was never sent to the model
  — is dropped from the transcript rather than left looking like part of it. A
  tool in flight is stopped too, because both the shell and the CLI tiers spawn
  their child with `kill_on_drop`, so a build is killed rather than waited out.
  A call caught mid-flight is recorded as cancelled rather than left unanswered,
  since a provider rejects an assistant message whose tool calls have no results.
  `Esc` still quits when nothing is running, and still cancels a half-typed
  command first.
- The approval preview scrolls. It used to be truncated at the box edge with an
  ellipsis, so a long diff could only be approved blind — the one thing that
  prompt must not ask. `↑`/`↓` and `PageUp`/`PageDown` move through it, the title
  shows the position (`edit a file? — 97/121`), the answer keys stay pinned below
  the window, and the scroll hint appears only when there is something to scroll
  to. The offset is clamped against the same geometry the renderer uses, so
  holding `↓` at the end does not strand it past the content.
- Bracketed paste is enabled, so a multi-line paste arrives as one block with its
  line breaks intact instead of a burst of keystrokes. Control characters are
  stripped; newlines are kept, because flattening a pasted snippet would silently
  change what the model is asked. A paste is ignored while a modal or the help
  overlay has the keys, and cut short past 100,000 characters with a note, since
  a stray clipboard can hold a whole file.

### Added

- The README leads with what spill is actually for — local until it isn't — and
  carries four pictures of the real interface. They are not drawings: a test walks
  the buffer `draw` produced, records each cell's symbol and styling as JSON, and a
  small renderer turns that into PNG. So a screenshot cannot drift from the
  program, because the program is what produced it.

  The renderer maps the theme's *named* colours onto a terminal palette — which is
  how spill works, since it asks for "yellow" and the terminal decides what that
  looks like — and needed one per-glyph font fallback. Caskaydia Mono has no shape
  for U+2717, the cross that marks a spilled-past tier, so it was drawn as an empty
  box in the one picture that exists to show a spill. Comparing against a
  known-unassigned codepoint is what found it; the bounding-box test tried first
  called the box "present" and would have shipped it.

- **Consulting a tier instead of handing the turn over.** Escalating changes which
  model you are using for the rest of the session, and makes the bigger one pay for
  the whole conversation again. `on_stuck = "consult"` on a tier keeps that tier
  driving and spends the next tier on one narrow question, whose answer comes back
  as advice.

  What goes in the question is the whole design. It is built from evidence spill
  already holds — the user's own words, the reason the tier was judged stuck, and
  the raw tool call with its raw error, or the output that was repeating — and never
  from a summary written by the model that got stuck, because that model is the one
  that does not understand the problem. The repeating case had to come from the
  stuck reason rather than the session: a turn that loops is abandoned before any of
  it is recorded, so the detector's own sample is the only copy of that evidence.

  The consultant is given no tools, which is what makes "the answer is prose" true
  for an `openai` endpoint rather than hoped for: it has nothing to call, so it must
  answer, and the call is one round trip rather than an agent loop. A `cli`
  consultant runs its own harness and cannot be stripped of its tools this way, so
  the question asks it plainly not to act and the README says so.

  Consult falls back to escalating rather than insisting: when the per-turn budget
  is spent, when the consult fails or is stopped, when the answer comes back empty,
  or when there is no tier below to ask. A stuck turn is therefore never stranded,
  and a consult can never cost more than the escalation it replaced. The answer is
  clipped before it enters the driver's history, because it is re-read on every
  later turn of the session. A second consult is told what the first one said and
  that it did not work, since the likeliest outcome of asking twice is paying for
  the same advice again.

  The consultant's tokens are attributed to the consultant rather than to the
  driving tier, so `/cost` measures the thing the choice between the two modes
  turns on.

- Build and plan modes, switched with `Shift+Tab` (or `Tab` when no command is
  being typed). Plan mode is read-only, and that is enforced in three places
  rather than requested once: the write tools are never offered to the model, a
  call for one is refused before a preview is even computed, and the system
  prompt says plainly that those tools do not exist for this turn. Withholding
  the offer is a courtesy to a well-behaved model; the refusal is the guarantee,
  and a test asks for a file in plan mode only to check the disk. The system
  prompt is swapped along with the mode, since it is the first message of the
  session and a stale one would leave a read-only turn being told about approval
  prompts. The mode is written on the prompt box, in the warning colour, because
  it is what pressing enter will mean; the footer offers the one key that leaves
  it.
- Session continuity for CLI tiers. `grok` and `command-code` now keep their own
  conversation across turns: the first turn opens a session — grok is handed a
  UUID of ours, `cmd` chooses its own and spill reads it back out of what it
  prints — and every turn after that continues it and sends only the new message.
  Before this, every turn flattened the entire transcript into the prompt and
  re-sent it, so a long session paid for the whole conversation again on each
  turn. Measured against a real `cmd` run, that is also a fixed ~15k tokens of the
  CLI's own harness on top of whatever we send, every single time. Any other CLI
  can be set up the same way with `session_args` and `resume_args`, both taking
  `{session}`; leaving them off is the previous behaviour exactly, and is what
  every other preset still does.
- A CLI tier whose turn was discarded on escalation has its session forgotten, so
  neither the next tier nor that tier on a later turn can resume a conversation
  holding output that was thrown away.
- Cache token counts are kept instead of being parsed past and dropped:
  `cacheReadTokens` / `cacheWriteTokens` (Command Code), the Anthropic pair, and
  OpenAI's nested `prompt_tokens_details.cached_tokens`. They appear in the
  transcript as `tokens: 15360 in, 2 out · 7424 cached`, and in
  `spill -p --output-format json`. Whether those tokens are included in the input
  count differs by provider, so the two are reported as they arrived and never
  added together. A cross-tier fallback is a cache miss that no amount of
  plumbing can avoid — caches are per provider — but the cost of it is now
  visible rather than guessed at.
- `src/provider/fixtures/cmd-stream.jsonl`: a real 30-frame
  `cmd -p --output-format json` run, recorded verbatim and parsed end to end by
  the test suite. It pins the frames no documentation describes — the `run_end`
  state dump that must be passed over, the thinking deltas that must never reach
  the transcript as answer text, the `sessionId` to resume with, and the cache
  counts on the result line.
- The model's prose is rendered as markdown rather than shown with its markers
  intact. Headings lose their hashes and take weight, `**bold**` and `*italic*`
  become weight rather than punctuation, inline code is styled and stripped of
  its backticks, and a fenced block becomes a framed box with its language on the
  top edge and its lines ruled down the side. Bullets become dots with a hanging
  indent, quotes get a bar, and `---` draws a rule. Two rules keep it honest: a
  marker with no closing partner is left as literal text (so a half-streamed
  `**bold` never flips the rest of the line), and an unterminated fence stays
  open to the end rather than flickering between two shapes mid-answer.
- A tool that is still running is marked by a spinner in its own line, so the
  transcript says whether the work has finished rather than only that it began.
  The line becomes a history entry with an arrow once the result lands.
- A tier that was just spilled past has its mark reversed for a beat, so the one
  event the whole program exists for is not lost in a wall of grey text. The rail
  also carries a state word at the far right — `working` with a spinner in flight,
  `ready` at rest — which gives the header a right edge instead of trailing off.
- Slash commands, and a menu that lists them as you type. `?` opens the same list
  beside every keybinding, which is the help surface the interface was missing.
  The set is deliberately small and about this program rather than the
  conversation: a command earns its place only if it has no other affordance.
  - The chain: `/tier` shows it, `/tier <name|number>` answers from one tier until
    told otherwise, `/tier auto` hands control back to the fallback policy,
    `/escalate` spills to the next tier without waiting for a stall, `/retry [tier]`
    sends the last turn again, `/drop` discards the active tier's own conversation.
  - The policy and the cost: `/sticky on|off`, `/cost` (tokens and cache reads,
    attributed per tier rather than only totalled), and `/context` (what actually
    goes out on each turn).
  - The conversation: `/compact`, `/clear`, `/help`, `/quit`.
  Choosing a tier by hand outranks the fallback policy, so it survives the next
  message even on a per-turn chain; a tier that then fails releases the choice
  rather than insisting on it. A slash only means a command at the start of a line
  and only for a name we know, so a question about `/usr/local` is still a
  question, and `//` starts a message with a literal slash.
- `/compact`, and compaction as part of spilling over. Earlier turns are replaced
  by a short ledger of what happened — including which tools ran and how they went,
  because the side effects of a discarded conversation are still real. It is
  deterministic rather than a model summary: it costs nothing, it works while the
  active tier is misbehaving, and it cannot degrade into the weak model
  paraphrasing the evidence it was too weak to use. It declines to act when the
  ledger would be larger than what it replaces, so compaction can never grow the
  thing it is compacting. Because the history changes shape, every tier's own
  session is dropped with it, so nothing resumes a conversation that no longer
  exists in that form. When a tier is abandoned and the session has grown past a
  few turns, this happens by itself, so the incoming tier's cold read is small.

### Changed

- The interface is composed for the work rather than boxed off. The tier chain is now a
  status rail across the top, with the answering tier drawn as a filled block, tiers that
  were spilled past marked with a cross, and a state word at the far right that spins
  while a turn is in flight. The rail shortens tier names when the chain does not fit,
  then falls back to the answering tier and its position, so it degrades instead of
  clipping.
- The transcript renders the model's markdown, and its lines are wrapped for the
  scrollbar's column when one is needed: previously the transcript was wrapped for the
  full width and then drawn into one column less, which clipped the last cell of any line
  that reached the edge.
- `NO_COLOR` is honored. Monochrome is not a degraded mode here: every state is carried by
  a glyph, a border, or a weight, and the monochrome theme is tested to keep all six
  tier/outcome states distinguishable without a single color.
- On terminals 104 columns or wider, a session panel shows the chain as a row per tier
  with its own state, the fallback policy, the workspace, and the session's token and
  cache totals, with a sparkline of per-turn spend beneath them so the shape of a
  session's cost is visible and not only its total. On a narrower terminal the
  conversation takes that space and the active tier moves down to the footer instead.
- The prompt box leads with a `❯` mark, so the place to type reads as a command line
  rather than as text that happens to sit in a box. It still grows with what is typed, up
  to five lines, and dims while a model is working.
- A terminal smaller than 46 × 12 gets a message saying what it needs, rather than a
  garbled layout.
- The approval modal is shaped by what the tools actually emit, which is three different
  things: `edit_file` shows a real before/after with `-` and `+` lines in red and green,
  `run_shell` styles the command as a command, and `write_file` shows its one sentence.
  A command dressed up as a diff would misrepresent what is about to happen.
- Tool outcomes in the transcript are colored by their own glyph — `✓` green, `✗` red,
  `!` amber — so the color is a second reading of the same signal rather than the only
  one.
- The welcome message no longer lists the tiers, their models and their keys. The rail
  shows the chain and its state, and the startup notices already report an unset key or a
  missing binary, so the same information was being stated three times on first run.
- `Theme` is a complete set of named roles rather than seven colors reused: identity,
  transcript, structure, states, the chain, markdown, the diff, and the panel's
  sparkline each have their own, and the fields nothing used were deleted rather than
  kept to make the palette look more considered than it is. The accent stays rationed to
  the wordmark, the answering tier, the user's own turn, the caret and the keys; a test
  fails if a heading or a section label starts wearing it.

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
