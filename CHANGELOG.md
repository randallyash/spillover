# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-09-15

### Added

- **Named session history and a picker.** A workspace keeps every conversation,
  titled from the first prompt (or `/session rename`). `/sessions` or Ctrl-P
  opens a picker: enter to switch, `n` new, `d` delete, `r` rename. `/new` and
  Ctrl-N start a blank session without throwing the last one away. A leftover
  single `{hash}.json` is migrated on first open.

- **Thinking is visible.** Grok `thought` frames, Command Code `thinking_delta`,
  and OpenAI `reasoning_content` stream as a dim "thinking" block above the
  answer, the way the Grok TUI does. They are not kept as the answer and are
  not sent back to a model.

- **`/deescalate`.** Go back to the first tier (`/tier auto` and `/tier 1` do
  the same). Sticky no longer swallows the climb.

### Fixed

- **A dead CLI session is retried fresh, not consulted about.** Command Code
  rejects `--session` when its on-disk transcript is empty. That used to look
  like a stuck model, so Grok was asked about a flag. Spill now drops the id
  and resends the conversation it already holds.

- **`/escalate` moves the rail.** The command used to change the chain without
  telling the header, so the chip stayed on the cheap tier.

## [0.3.0] - 2026-09-15

### Changed

- **File tools cannot leave the workspace.** `read_file`, `write_file`, `edit_file`,
  `list_dir`, `grep` and `glob` refuse a path whose canonical location is outside it —
  a `..` walk, an absolute path, a `~/…` that is not the workspace, a symlink that
  points elsewhere — and tell the model so, rather than quietly reading `~/.ssh`.
  Absolute paths that actually land inside the workspace are still accepted.

- **A turn may take 32 tool steps, not 12, and the number is in the config.** Twelve
  was enough to make a modest refactor look like a stall. `max_steps` lives under
  `[general]`; omit it for 32. Zero is refused.

- **`run_shell` waits five minutes, not two, and a call can ask for longer.** 120s
  was enough to kill `cargo test` on this repo. `[general] shell_timeout_secs`
  defaults to 300; a call may pass `timeout_secs` of its own, capped at 1800, and a
  zero or a number above the cap is refused rather than killing immediately.

- **OpenAI-compatible streams are asked for usage.** The request now sends
  `stream_options.include_usage`. Several providers only attach token counts if asked,
  so the sidebar could sit at zero and look like a bug.

- **IO failures are classified from `ErrorKind`, not from the English in the
  message.** A missing file in German, or a permission error wrapped in extra
  words, still counts as the same wall. The message is still what the model sees;
  the kind is what the stall detector counts. Argument checks this crate writes
  are still matched on the text, because that text is ours.

- **The repetition detector no longer treats real code as a loop.** A run of
  closing braces is not collapse, and a twelve-token signature that happens to
  appear a few times in a long file is not either. A span only trips when it
  makes up a quarter of what has been written, and spans with no words in them
  are ignored.

- **The Windows installer publishes as Randall Yash.** That string is the MSI
  manufacturer, taken from `Cargo.toml` authors and the WiX definition.

- **The shipped example is DeepSeek first, Grok when it stalls.** Command Code's
  DeepSeek V4 Flash is the cheap tier; Grok is the expensive spillover. A first
  run that can pick a CLI now prefers `command-code` over `grok` for the same
  reason.

- **The README is a product page, not a design diary.** Setup, uniqueness, and
  the DeepSeek→Grok pictures lead. `/why`, the spill log and `doctor` moved into
  a reference fold so the front of the page can be scanned. The screenshots are
  the real interface again, now with Grok as the tier that answers.

- **Per-function doc essays were cut.** Module docs stay; a function keeps its
  first paragraph and loses the rest. The thinking is still in the module, the
  source is shorter to read.

### Fixed

- **Glob (and grep) results stay relative on macOS.** Canonicalising the workspace
  for the confine check made matches print as `/private/var/...`, because `/tmp`
  is a symlink there.

## [0.2.0] - 2026-09-13

### Added

- **Read-only shell commands run without being asked about.** Every `run_shell`
  command used to stop and show a modal, including `ls` and `git status` — which are
  the shell spelling of `list_dir` and `read_file`, both of which spill already runs
  unprompted. A list of them (listing, reading, searching, and `git`'s read-only
  subcommands) now runs straight through, and everything else still asks. What counts
  as a rule is a **prefix of words matched against a bare word list**: a command
  holding `;`, `&`, `|`, a redirection, a substitution, a glob or a quote always asks,
  however it starts, so `"git status"` cannot be talked around by
  `git status; rm -rf ~`. The commands whose flags can write or execute — `find`,
  `sort`, `sed`, `xargs`, `awk`, `make`, `cargo`, `npm`, `python` — are deliberately
  left out, because a word-prefix rule cannot see a flag. `/allow <words>` sticks one
  for the session, `/allow save <words>` writes the list into `[general] allow_shell`,
  `/allow clear` drops the session's, and `allow_shell = []` asks about everything
  again. `/allow save` edits the file surgically — one line, every comment intact —
  and refuses rather than guessing when there is no `[general]` table, no file, or a
  file it cannot parse. When a rule covers a command the transcript still shows the
  command and names the rule that let it through, because a rule is invisible by
  design and that is exactly what makes it worth saying.

- **`/undo` reaches back ten writes instead of one.** Each entry keeps its own path
  and its own fingerprint of what the write left behind, so an entry may still only be
  put back while its file is untouched — the refusal is per entry, exactly as it was
  with one. The stack is bounded twice: ten writes, or 8 MiB of remembered contents,
  whichever comes first, oldest dropped. A snapshot is a copy of a file, so the byte
  budget is the bound that usually decides; the newest entry is never the one dropped,
  because it is the one the command is for. Every undo reports how much is still behind
  it, and a refusal says that the entries behind it are blocked until the changed file
  is dealt with — undoing out of order would be worse than being told why.

- **A patch in a stalled tier's text is never applied, and now it cannot be.** Worth
  writing down because the text of an abandoned turn can look exactly like an action:
  `Here is the fix:` and a diff, or a line that reads like a command. It is only ever
  text. The only thing that can change a workspace is a structured tool call that was
  approved and ran, there is one place in the code where a call can become an action,
  and the tests now pin it from both sides — a tier that says it patched a file and
  then fails changes nothing and is not even shown to the next tier, while a tool call
  that completed before the stall stands and can be undone. One caveat stated rather
  than implied: a delegated `cli` tier runs its own tools in its own process, which
  spill neither parses nor can withhold.

- **`spill doctor` answers "it does not work on my machine" before anyone has to
  ask.** It now prints the configuration file in force and how it was chosen — an
  explicit `--config`, the default path, or *not there yet* with the built-in defaults
  in force, which is the difference between the file you edited and the file being
  read. A `cli` tier shows where its binary actually is, not merely that something of
  that name is on `PATH` (a second copy earlier on `PATH` is the usual reason a tier
  behaves differently than in your shell), plus the flags it will run with: whether it
  continues a session between turns or is handed the whole transcript each time, and
  whether it has a read-only mode that makes it consultable at all. Every model
  credential spill removes from a delegated CLI's environment is listed, with the ones
  set here marked — the whole list rather than the summary, because the question is
  "is my key the one being taken away?". And a machine block reports the terminal size
  and colour depth, and that nothing need be installed for spill itself: no Rust
  toolchain, no runtime. All of it is in `--json` too.

- **A first run is now a complete setup rather than half of one.** With nothing
  configured, spill picks the local server it found as tier 1 — and then adds one
  fallback, the first agent CLI on PATH, so "local until it isn't" has an "isn't" from
  the very first turn. A one-tier chain has the headline behaviour switched off, and
  the moment to notice that is not after the first stalled turn. The order it looks in
  is `claude`, `codex`, `gemini`, `copilot`, `cursor-agent`, `grok`, `opencode`,
  `crush`, `command-code`: a preference order among what is installed, not a ranking,
  with `command-code` last because it is the harness you may be running spill inside
  and choosing it would spend a plan you are already using. No agent CLI at all is a
  normal answer — you get an all-local chain and are told that a stalled turn will end
  rather than spill.

- **A `cli` tier that is not installed is left out of the chain instead of carried.**
  A harness that is not on PATH cannot answer, so building it just put a fallback in
  the rail that never happens. It is dropped, `notes` says so in as many words, and
  the tier that *is* usable still starts. A tier that is merely unreachable is
  deliberately *not* dropped: pre-flighting the local server would mean silently
  starting on the paid tier, which is the opposite of what this program is for.

- **Every refusal names the next command.** `spill presets` to see what a tier can
  be, `spill setup` to choose tiers and write a config, and `spill doctor` when the
  tiers exist but nothing is reachable. A refusal that does not say what to do next
  is where a first run turns into an uninstall; the zero-tier message named
  `spill setup` and stopped there, and a chain that ran out of tiers named nothing at
  all.

- **`/why`: the stall call, made inspectable.** Handing a turn to another model is the
  one decision spill makes on its own, so it is the one that has to be arguable — a
  user who cannot tell why their turn was taken away will turn the fallback off. The
  reason was already in the transcript; what was missing was everything behind it. The
  detectors kept their counts privately and threw them away at the threshold, so the
  verdict existed and the evidence did not.

  Now every attempt carries a verdict, and `/why` prints the counters it was decided
  from — including the ones that never fired, which are usually the interesting ones.
  A classic case it makes visible: the *same* tool failure three times trips a budget
  of three while the tier's own allowance is four, and nothing on screen would ever
  have said so.

- **A one-line warning when a turn nearly spilled instead of answering.** A near miss
  is the only evidence that a threshold is right — too tight costs a turn, too loose
  never catches anything — and a silent one teaches nothing. Any counted signal one
  short of its allowance fires it, as does a wait that used four fifths of its own
  budget. The report says "3 of 4" rather than explaining what would have happened,
  because it lands mid-transcript and has to stay one line.

- **A spill log.** One stall is answered by `/why`; a pattern of them is answered by
  `~/.local/state/spill/spills.jsonl`, one JSON record per line with the trigger, both
  tier ids, whether a consult or a handover ran, and every counter including the
  allowances they were measured against. `trigger` is a stable token rather than a
  prose summary, so the file can be grouped by it — matching on wording that exists to
  read well is how a log stops working the first time a message is reworded. A turn
  that stalled with no tier below is logged as `"ended"` rather than skipped, because
  that is the only ending a single-tier setup can have and its thresholds are the ones
  most worth tuning. A log that cannot be written is reported once and does not stop
  the turn: thresholds tuned from a file that has quietly stopped growing are worse
  than no file.

- **The watchdog now knows how close it came, not just that it waited.** A gap is
  judged against the allowance that applied *to it* rather than the other one, because
  eight seconds is unremarkable against a local tier's two-minute first-token budget
  and nearly fatal against its thirty-second idle one. The gap between one request's
  last frame and the next one's first is deliberately not counted: it contains the tool
  call that ran in between, which can be minutes long and says nothing about the model.

- Both samples in the README — the `/why` report and the log record — are pinned by
  tests against the real output, so the documentation cannot drift from the program.

- **`packaging.yml` installs from the Homebrew tap on a clean macOS runner.** The
  tap was already publishing a formula whose checksums matched the release, and
  nobody had ever run it end to end on the platform it exists for. The workflow
  installs `randallyash/spillover/spill` the way a user does, then checks that the
  installed binary reports the version it was released as, and that it starts. The
  same run verifies the Linux tarball through `verify-install.sh`. It is dispatched
  by hand rather than run in CI, because it reaches the network for a published
  artifact instead of building a commit.

### Changed

- **Consult is now the default when a local tier has a paid one below it.** Escalating
  is the expensive mistake: the stalled turn goes to the tier below whole, and because a
  spilled session stays spilled, the cheap model is then gone for every turn after it —
  one bad answer bought at the price of the rest of the session. A consult spends one
  question on the tier below and the local model carries on driving.
  `/on-stuck escalate` switches a session over, and `on_stuck = "escalate"` does it for a
  single tier; both still hand the turn over whole, and a chain of one behaves exactly as
  before, since there is nothing below it to ask. The fallbacks are unchanged: when the
  advice is no use, when the consult fails or comes back empty, or when the per-turn
  budget is spent, the turn escalates anyway, so consulting can never strand a turn or
  cost more than the escalation it replaced. What moved with the default is the rest of
  the reporting — `spill doctor` prints a tier's policy when it is *not* the default,
  which now means `escalate`, and the session rail highlights `escalate` for the same
  reason.

- **`config.example.toml` is a working config now, not a template with everything
  commented out.** Two tiers, in order, with the reasons written down the way a person
  writes them: the model left empty because LM Studio is already holding Qwen3 Coder
  and naming it here would mean a 404 the day you switch; `on_stuck = "consult"` on the
  local tier because escalating costs the frontier the whole conversation and then
  every remaining turn of the session; one `[tier.limits]` override that leaves the
  class's own first-token budget alone, because that inheritance *is* the mechanism and
  writing the defaults in would have hidden it.

### Fixed

- **The AUR recipe was a release behind the release it downloads.** `pkgver` sat at
  0.1.1 while 0.1.2 had shipped, so `yay -S spill-bin` would have fetched the older
  archive and presented it as the newer version — a package that looks abandoned
  and installs the wrong thing. Nothing failed, because a version kept in more than
  one place is only checked in the place you are looking, so a test now holds the
  recipe's `pkgver` and the changelog's newest release to `Cargo.toml`. Two more
  defects came out of actually building it: `!strip` without `!debug` left an empty
  `/usr/src/debug/spill-bin` directory in the package, and the block that installed
  `config.example.toml` could never fire, since the release archive does not carry
  one — `CHANGELOG.md` is installed in its place, which the archive does carry.

- **The README's doctor sample showed a version two releases old.** `spill 0.1.0 —
  doctor`, in the section a reader goes to when something is wrong, is the kind of
  detail that says more about how maintained a project is than any wording around
  it. It is pinned now, along with the sample's shape, so it cannot age again.

- **A loop spelled differently was not a loop.** The "same tool with the same
  arguments" detector compared the arguments as the *text* that arrived, so a model
  that re-sampled the same call with the space on the other side of the colon — or
  with its keys in another order — looked like a model doing something new. It is the
  failure the detector exists for, and it was the one way out of it: the model only had
  to rewrite itself slightly to keep looping. Arguments are now compared as parsed
  JSON, so meaning decides and spelling does not, while a changed value or a different
  key is still a different call. The same held for the tighter budget on repeated
  failures: one missing file reported two ways was not accumulating, so a model grinding
  against a single obstacle kept the tier's full allowance. Found by writing the
  detector's tests against the shapes a real loop arrives in rather than the shapes the
  detector happened to be handed.

- **A spill record was written in many small pieces, so two processes spilling at
  once spliced their records together.** The append is atomic, but only for a single
  write, and `Display` for a JSON value emits in pieces — each of which became its own
  `write` on a shared file. The record is now whole before it reaches the file, so
  every line is one JSON object however many `spill` processes are running. Found in
  this project's own log, which had two records interleaved character by character.

- **The test suite wrote to the real spill log of whoever ran it.** A test drives the
  real one-shot path, and that path resolved the log from the platform's state
  directory — so `cargo test` appended to `~/.local/state/spill/spills.jsonl`, from
  several threads at once. The path is now injectable, and the test helper points at a
  temporary directory. The failure was the worst kind: a green suite quietly filling a
  file the user is meant to be tuning thresholds from.

- **The command menu's row ceiling was a hard-coded 14.** Derived from the catalogue
  instead: the two drifting apart is exactly how a command becomes unreachable, which
  is what a hard-coded 12 did to `/quit` once already. The menu scrolls, so nothing was
  actually lost — but adding a command should not be what puts another out of sight.

## [0.1.2] - 2026-09-12

### Added

- **A live `~42 tok/s` in the rail while a tier writes.** How fast a local model
  actually goes is the number that decides whether it is worth keeping, and it belongs
  on screen while the answer arrives rather than in a summary afterwards. It sits at the
  right of the header beside the state word, and goes when the model stops.

  It is inferred rather than counted, and the `~` says so. Tiers report tokens only when
  they choose to, and at the end of a reply, so a figure that moves while the text
  arrives has to come from the characters instead — divided by a ratio learned from any
  turn that *did* report usage, so it settles toward the model in use rather than
  assuming four characters to a token forever. Three things keep that learning honest: a
  turn that called a tool is skipped, because tool arguments arrive as structure and
  never stream as text, so its completion tokens would weigh more than the characters
  counted and every rate after it would read high; a consult is skipped, because its
  answer goes to a discard channel and the characters to hand are the driver's; and the
  learned ratio is clamped, so one strange turn cannot spoil the session.

  Timed from the first character of the message rather than from the request, so a local
  model's prefill — seconds of loading and reading a long prompt before a single token —
  is not charged to writing. That is the difference between a figure that describes
  generation and one that makes a fast model look slow. The clock is the redraw tick,
  which is an estimate as well: a tick the interface was too busy to take is skipped
  rather than replayed, so a starved interface reads a little fast. Nothing is shown
  until there is enough of a stream to divide by, because a number that swings is worse
  than no number at all.

  It holds a slot of its own on the rail, reserved whether or not a number is showing at
  that moment, so the chain never re-flows to make space for it as the text starts and
  stops — which is several times a turn. An earlier cut had it ride on whatever padding
  the chain left over, which looked harmless and was not: the chain re-measures itself
  against the width and steps up to a longer form the moment one fits, so at some widths
  — 69 to 78 cells with a typical local label — the leftover shrank below the number and
  the rate vanished, then reappeared as the terminal grew further. A figure that comes
  and goes with the size of the terminal reads as a figure that does not exist, and the
  rail is the only place it appears. Below 56 cells the rail is chain-first by design:
  the number is given up rather than the chain shortened.

### Fixed

- **A model grinding against the same missing file is spilled on Windows too.** The
  error classifier knew a missing file only by Unix wording, so on Windows — where the
  same absent path reads "The system cannot find the file specified. (os error 2)" — the
  identical failure was filed as an unknown one. Nothing accumulated, and the tighter
  budget that spills a model walking into one obstacle rather than discovering several
  never fired, leaving the guarantee quietly absent on that platform. It recognises that
  wording now, and matches the error numbers parenthesised: a bare `os error 2` also
  matches `os error 20`, which is ENOTDIR — a directory where a file was expected, not an
  absence — so filing it as one would spill a model that is reading a directory listing.

  The fixture that covers this asserted on Unix prose, which is why it passed here and
  failed there; it asserts on the error number now. A test pinning the Windows wording
  runs on every platform, so the next hole of this kind is caught before a push rather
  than after it.
- **The AUR `PKGBUILD` builds.** It never had: `conflicts=('spill', 'spill-bin-git')`
  reads like a list with a comma between its items, but in bash the quote and the comma
  are one token, so the first element is `spill,` and `makepkg` refuses the whole file
  with `conflicts contains invalid characters: ','` before it does anything.
  `updpkgsums` fails on the same validation, so the documented way to refresh the
  checksums could not be run either. The file now carries the published 0.1.1 sums, and
  the recipe has the `.SRCINFO` the AUR actually parses and a `.gitignore` so a build
  inside the clone does not offer to commit the tarball it just downloaded. The package
  is still unsubmitted, so `yay -S spill-bin` does not work yet.

## [0.1.1] - 2026-09-11

### Added

- **`/undo` — put back the last approved write.** Spill stops and shows a diff
  before every write, so the bytes it was about to replace were already in hand;
  keeping them makes the one capability a model-initiated write cannot offer you.
  It pairs with spilling over: a tier that wrote junk and then looped is abandoned,
  but its side effects are not, and the next tier inherits a workspace containing
  them.

  It reaches **one write** — a second `/undo` says there is nothing to undo rather
  than working backwards — and it refuses rather than guessing: if the file is not
  still exactly what the write left, because you edited it or it was deleted, it
  says so and changes nothing. That refusal is the whole safety story, and it is
  why `/undo` does not ask for approval first: you typed it, and the thing it
  protects you from is not yourself.

  A created file is removed again, along with the directories the write created —
  but only while they are empty, so a directory that acquired something else is
  left alone. A write too large to keep a copy of says so rather than quietly
  offering to restore an *older* write. `run_shell` is never reversible and is
  never offered. Nothing is persisted, so it does not survive a restart.

- **A tool-error budget by error class.** A run of tool failures used to be counted
  without regard to *what* failed, so a model guessing at a path that does not exist
  and one having an unlucky run looked the same. Failures are now classified —
  missing file, permission denied, malformed call, timeout, or unrecognised — and the
  same kind of failure from the same tool three times is a stall. Three different
  files read *successfully* is not, and never was; that is now pinned by a test.

  **A repeat only counts when it is really a repeat.** A missing file, a refusal and
  a timeout are facts about the *world*, so those only accumulate when the call's
  target repeats too: looking for a `.env`, a `Makefile` and a `pyproject.toml` that
  are not there is three facts about the filesystem, not one wall. A malformed call is
  the model's own output being wrong, so those count across targets — the target was
  never what was wrong. Probing that never stops is still caught, by the general
  run-of-failures rule at whatever the tier asked for.

  The budget is a minimum against the tier's own `max_repeat_run`, so it can only make
  detection tighter and can never loosen a tier that configured itself strictly.
  Transient and unrecognised failures have no budget of their own, because the next
  try may well work, so only the tier's general allowance applies to those. The
  verdict names the class:
  `read_file failed 3 times with the same error (no such file)`.

- **The handoff is narrated before it happens.** A new `Spilling` event is sent the
  moment the verdict lands and *before* the attempt is discarded, so the transcript
  says why a tier is being given up on as it happens rather than after. The rail
  meanwhile draws that tier's mark reversed in the warning colour for a beat
  (~360ms), then settles it into the spent `✗`.

  This is presentation only: the tier is already stopped when the verdict fires, so
  the beat costs the retry nothing — the next tier starts work immediately and the
  narration is held on screen over it.

- **The session survives the process.** Quitting used to lose everything: the
  conversation, the tier it had settled on, whether one was pinned, the sticky
  choice, the stuck policy, the mode, and every CLI tier's conversation handle.
  `spill` with no arguments now resumes the session for the directory it is run
  in, and says so — `resumed this session — 42 messages, last saved 3h ago`.

  One file per workspace under the state directory (`~/.local/state/spill/sessions/`
  on Linux), written atomically and `0600`, keyed by a hash of the canonical
  workspace path with the path itself stored and checked on load so a collision
  cannot resume somebody else's conversation. The tier is named by its configured
  `id` rather than its position, so reordering `config.toml` between runs does not
  quietly move you onto a different model; a tier that is gone is dropped and
  falls back to the first.

  **The transcript is stored inline, and that was the whole argument.** A pointer
  to a provider-side conversation would be smaller, but an `openai` tier is
  stateless by design — the entire history is re-sent on every turn — so a
  pointer-only session would remember *nothing* for a local model, which is the
  configuration this program is built around. The file is therefore the only
  record that exists for those tiers.

  The system prompt is not stored, only the conversation: it is rebuilt from the
  restored mode on load, so a session saved in plan mode cannot be resumed
  carrying instructions to be read-only while in build mode. What comes back is
  the conversation the models saw, so tool spinners and notices are not replayed.
  Token and cost totals are not persisted — they describe a session, and `/cost`
  starts fresh.

  Written by the agent, which is the only thing that can see the session, the
  chain and each provider's conversation at once, at one call site that every
  command passes through — so a completed turn, a tier change and `/clear` are
  all saved by the same code. A failed write is reported and the turn carries on.

  `-p`, `doctor` and `presets` are untouched: a script that resumed yesterday's
  conversation because it ran in the same directory would be a trap. `/clear`
  clears what is on disk as well as what is on screen.

### Changed

- **Timeout defaults depend on where the model is.** One profile was wrong in both
  directions. A local tier — loopback, the private ranges, link-local, `.local`,
  `host.docker.internal` — now gets **120s** before its first token, because a 30B
  loading into a 5090's VRAM can easily need more than the old 30s, and **30s** of
  silence once it is streaming, because a local server that has gone quiet has
  wedged. Hosted tiers keep 30s / 60s, and a `cli` tier is judged as hosted: its first
  line includes its own harness starting up, which is slow for reasons a model server
  is not.

  A tier's `[tier.limits]` block is now *overrides*, and each field falls through to
  its class default independently — so a local tier that sets only `idle_timeout_ms`
  keeps the first-token patience its locality implies instead of dropping back to a
  global 30s, which was the false spill this was meant to prevent.

  `spill doctor` reports the effective timeouts for a local tier, and carries the
  class and both budgets for every tier in its JSON.

- **The rail's flash moved from after a spill to around it.** It used to reverse the
  dead tier's mark for ~900ms once the move had already happened. It now flashes the
  tier being abandoned, starting at the announcement and settling when the beat
  expires, so the mark and the reason belong to the move rather than to its aftermath.

### Fixed

- The command menu shows every command again. It capped at twelve rows and the
  catalogue is thirteen, so `/quit` had scrolled out of reach — and the cap is
  now what fits above the prompt rather than a fixed number, so the menu can
  never cover the prompt it belongs to.
- **`spill setup` accepts a model id the endpoint does not serve.** Three gaps
  stacked into one bad first impression: the text step took any non-empty string,
  the review check reported a configured id as *ready* without looking for it in
  the advertised list — naming the typo as the model that "would be used" — and
  `w` wrote the config anyway, because a failed check was only ever drawn. A
  hand-typed OpenRouter or xAI id that was one letter off therefore reached the
  config and 404'd on the first turn of the first conversation. That is the first
  impression this wizard exists to prevent, and the README already claimed
  "every choice is checked before anything is written", which was not true.

  A typed id is now checked against the list the endpoint already gave us — the
  same list the picker offers, so it costs no second request — and a near miss is
  named: `"deepseek/deepseek-v4-falsh" is not among the 2 models this endpoint
  lists — did you mean "deepseek/deepseek-v4-flash"?`. The same question is
  answered in one place, so the text step, the review screen and `spill doctor`
  cannot disagree about it. An endpoint that lists nothing still refuses nothing:
  that path exists for a gateway which serves completions without advertising
  them, and refusing there would block a working setup on a guess.

  `w` is now refused while any tier cannot answer, which is what makes the check
  a refusal rather than a note — but only the first time. Pressing it again
  writes the config anyway, because a server you have not started yet is a real
  reason to save a tier that does not answer right now, and a wizard that cannot
  be finished is its own bad first impression.

- **The model list was re-requested on every answer.** Arriving cleared the
  in-flight marker while the model step was still open, so the next loop asked
  again — a request per round trip against the endpoint for as long as that screen
  was up, and a cursor reset to the top of the list on every arrival, which put
  "type it myself" out of reach behind a list that kept re-selecting its first
  row. Asking is now gated on the wizard's own idea of whether an answer is still
  wanted.

- The wizard waits at most ten seconds for an endpoint to list its models, like
  every other probe it makes. Unbounded, a server that accepted the connection
  and then said nothing left it asking forever with no way forward.

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

- **Consult is a command, not just a config key.** It was the interesting new
  mechanic and it was buried in TOML: the only way to try it was to edit a file
  and restart. `/on-stuck escalate|consult` now switches every tier for the rest
  of the session, and `/consult` asks for a single consult on the next stall
  without changing the policy — the way to find out whether it helps before
  committing to it.

  The one-shot is taken rather than read when a stall is handled, so a single
  request cannot quietly become the policy for the session; a request that
  survived would be a policy change wearing a one-shot's clothes. Asking when
  there is no tier below is refused while the user is looking at it, and
  `/on-stuck consult` on a chain one tier deep says the policy cannot take
  effect rather than silently doing nothing.

- `/on-stuck auto` hands the choice back to each tier's own. A session policy
  flattens the whole chain to one answer, so without this a single `/on-stuck`
  would be a one-way door: the arrangement its configuration described — one tier
  consulting, the next not — would be unreachable until a restart. The notice says
  what going back actually means, naming each tier's policy, because "back to the
  configuration" is not one policy.

- `spill doctor` reports each tier's stuck policy. Consult is opt-in and its whole
  argument is that a choice was made, and until now nothing outside the session
  panel could show which tiers consult — not `doctor`, and not for a tier other
  than the one answering. The plain-text report prints the policy only where it is
  not the default, so an ordinary configuration stays as short as it was, and the
  JSON carries `onStuck` for every tier so a script never has to infer it from a
  missing line.

- The session panel says which stuck policy is live. It is drawn from the
  answering tier's own configuration when the session has not chosen one, because
  tiers differ on purpose — consulting a hesitant local model is the point, while
  a frontier tier has nothing better to ask — and marked with `*` when the
  session chose it rather than the config, so an override cannot outlive its
  experiment unnoticed. Consult is drawn in the warning colour, with the word
  itself carrying the meaning either way.

  The default stays `escalate`, in config and on screen.
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

  An `openai` consultant is given no tools, which is what makes "the answer is prose"
  true for it rather than hoped for: it has nothing to call, so it must answer, and the
  call is one round trip rather than an agent loop. A `cli` consultant runs its own
  harness and cannot be stripped of its tools this way, so it is launched read-only
  instead — a plan mode, a sandbox policy, a Q&A mode, whichever that CLI offers — with a
  hard instruction not to act leading the question, and never with the unattended flag
  that would hand the powers straight back. A CLI whose preset names no read-only mode is
  never consulted at all: the turn escalates and says why, rather than asking the model
  to behave when nothing enforces it. The shipped presets carry the flag wherever the CLI
  has one, checked against each CLI's own `--help`.

  Consult falls back to escalating rather than insisting: when the per-turn budget
  is spent, when the consult fails or is stopped, when the answer comes back empty,
  when the consultant cannot be held read-only, or when there is no tier below to
  ask. A stuck turn is therefore never stranded, and a consult can never cost more
  than the escalation it replaced. The answer is clipped before it enters the
  driver's history, because it is re-read on every later turn of the session. A
  second consult is told what the first one said and that it did not work, since the
  likeliest outcome of asking twice is paying for the same advice again.

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

[0.4.0]: https://github.com/randallyash/spillover/releases/tag/v0.4.0
[0.3.0]: https://github.com/randallyash/spillover/releases/tag/v0.3.0
[0.1.2]: https://github.com/randallyash/spillover/releases/tag/v0.1.2
[0.1.1]: https://github.com/randallyash/spillover/releases/tag/v0.1.1
[0.1.0]: https://github.com/randallyash/spillover/releases/tag/v0.1.0
