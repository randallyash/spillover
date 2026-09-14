# spill

[![CI](https://github.com/randallyash/spillover/actions/workflows/ci.yml/badge.svg)](https://github.com/randallyash/spillover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/randallyash/spillover)](https://github.com/randallyash/spillover/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Local until it isn't.**

An agentic coding assistant for the terminal that runs on your own machine first — free,
private, no API key — and reaches for a hosted model only when the local one actually
misbehaves. No retyping, no cancelling, no watching it loop.

![spill: a local model stalls, and the turn moves on](assets/hero.png)

```
tier 1  your local model    free, private, no key — tried first, every turn
tier 2  a fallback          reached only when tier 1 misbehaves
tier 3  a last resort
```

Your prompt goes to the local model. When that model repeats itself, stops making progress,
goes quiet, or fails outright, spill abandons it and re-runs the same turn on the next model
in your list. The abandoned answer is thrown away rather than inherited, so the model that
takes over starts from your question and not from someone else's half-finished loop. You
keep using the free model, and the paid one stays where it belongs: unspent, until the
moment it is needed.

## Why you would want this

Local models are free, private, and good enough for a lot of work — and then they
occasionally hang, or collapse into repeating themselves. Most tools leave you to notice
that, cancel, and start over somewhere else. spill treats it as expected:

- **You keep using the free model.** It is tier 1, so that is where your prompts go by
  default — and where they keep going, turn after turn, as long as it behaves. The paid
  tiers below are reached only when the local one actually fails.
- **A stuck model costs you nothing.** No loop to watch for, no cancel-and-retype. The turn
  is re-issued on the next tier by itself, in the same conversation, with the reason written
  into the transcript.
- **The wasted answer is thrown away; the work is not.** The next model starts from your
  question rather than from someone else's half-finished loop. Side effects are kept,
  because a file that was written is written — and `/undo` takes those back.
- **Any model, one interface.** Any OpenAI-compatible endpoint — LM Studio, Ollama,
  llama.cpp, vLLM, OpenRouter, xAI — or any agent CLI you already have installed and logged
  in. A subscription you already pay for becomes a fallback with no separate API key.
- **It is an agent, not a chat window.** It reads, searches, edits files and runs commands,
  asks before anything that writes, and shows you answers as markdown.

## What makes it more than a fallback

| | |
| --- | --- |
| **Consult instead of handing over** | Escalating changes which model answers for the rest of the session and makes the expensive one pay for the whole conversation again. A **consult** keeps the cheap model driving and spends the big one on one narrow question. It is the default whenever a local tier has a paid one below it. See [Consult instead of escalating](#consult-instead-of-escalating). |
| **Plan mode** | `Shift+Tab` makes a turn read-only. Not a request in a prompt — the write tools are withheld from the model, and a call for one is refused before it reaches your disk. See [Modes](#modes). |
| **A stall call you can argue with** | `/why` prints every counter the verdict came from, including the ones that never fired — usually the interesting ones. Every spill is also appended to a log you can `jq`. See [Why it gave up, in numbers](#why-it-gave-up-in-numbers). |
| **Cost you can see** | Tokens *and* cache reads, per tier, with a sparkline of per-turn spend. A cross-tier fallback is a cache miss no design can avoid; this is where you find out what it cost. A tier that *failed* still shows what it spent. |
| **Reading runs without asking** | `ls`, `cat`, `git status`, `git diff` and the rest of the read-only set run unprompted, because they are the shell spelling of tools spill already runs unprompted. Everything else asks. `/allow` sticks a rule of your own. |
| **Undo, ten writes deep** | `/undo` puts back the last approved write using the bytes it showed you in the diff, then reaches the one before it — and refuses when the file has moved on since, so it can never discard work done after. |
| **A handoff you can follow** | The reason a tier is abandoned is announced *before* the move, with the tier's mark flashing in the rail, rather than arriving at the same instant as the next model's answer. Silence during a failover is how you end up distrusting it. |
| **Judged on where it runs** | Timeouts are per class, not per tier: a model on your LAN gets 120s to load its weights and then 30s of silence once it is streaming, where a hosted tier gets 30s and 60s. |
| **Sessions that survive a restart** | Quit and come back in the same directory and the conversation, the tier you settled on, the sticky choice, the stuck policy and the mode are all still there. One session per workspace. |

## Install

```sh
# macOS and Linux
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/randallyash/spillover/releases/latest/download/spill-installer.sh | sh

# macOS, with Homebrew
brew install randallyash/spillover/spill

# from source, with Rust 1.85 or newer
cargo install --git https://github.com/randallyash/spillover
```

```powershell
# Windows
powershell -c "irm https://github.com/randallyash/spillover/releases/latest/download/spill-installer.ps1 | iex"
```

| Route | Status |
| --- | --- |
| Installer script (macOS, Linux, Windows), `.msi` | published |
| Homebrew | `brew install randallyash/spillover/spill` |
| `cargo install --git` | works |
| Prebuilt archives, five platforms | on the [releases page](https://github.com/randallyash/spillover/releases) |
| Arch (AUR) | `PKGBUILD` in [`packaging/arch`](packaging/arch), verified — not submitted yet |

Every release is signed off with a `sha256.sum`.

Nothing needs to be installed for spill itself: no Rust toolchain and no runtime. You need
a terminal — 46 columns by 12 rows minimum, and it says so rather than drawing something
garbled below that — and for a local tier, a model server already running. Colour is
optional: `NO_COLOR=1` turns it off and nothing is lost, because every state is carried by
a glyph, a border or a weight as well as a hue.

## Quick start

Just run it:

```sh
spill
```

With no configuration, spill looks for a local model on the usual ports (LM Studio, Ollama,
llama.cpp, vLLM), starts on whatever it finds, and tells you which one it picked. Then it
adds a fallback for you: the first agent CLI on your `PATH`, so that "local until it isn't"
has somewhere to spill to on the very first turn. One tier is not this product — a chain of
one has the headline behaviour switched off, so it will not leave you with one.

The CLIs it looks for, in order: `claude`, `codex`, `gemini`, `copilot`, `cursor-agent`,
`grok`, `opencode`, `crush`, `command-code`. That is a preference order among the ones
actually installed, not a ranking — the first found wins and the rest are never considered.
`command-code` is last deliberately: it is the harness you may be running spill inside, and
picking it for you would spend a plan you are already using. With no agent CLI installed you
get an all-local chain and are told what that means — a stalled turn will end rather than
spill. That is a supported way to run, not a broken setup.

If nothing is running locally, it opens the setup wizard instead of leaving you at a prompt
that cannot answer. Every choice is checked before anything is written:

```
1. LM Studio  (tried first, openai)
http://localhost:1234/v1
  ✓ http://localhost:1234/v1 would use qwen3-coder-30b
2. Command Code  (fallback, cli)
command-code
  ✓ cmd is installed
```

`w` writes `~/.config/spill/config.toml` — refusing while any tier cannot answer, because a
config that cannot work is worse than no config. Press `w` again to insist, for a server you
have not started yet. An existing file is backed up to `config.toml.bak` and confirmed
before it is replaced, and the new file is mode `0600`.

`spill setup` opens the same wizard later. `spill presets` lists every endpoint and CLI a
tier can name.

A configuration that cannot answer at all is refused at startup rather than failing on the
first turn, and it says which tier went and what to do:

```
spill: none of the configured tiers can be used (tier "grok" runs "grok", which is
not on PATH), so no prompt could be answered. Run `spill presets` to see what a
tier can be, or `spill setup` to choose tiers and write a config.
```

A tier that is merely *down* is not dropped — it stays in the chain and is tried, because
pre-flighting the local tier would mean silently starting on the paid one. Only a `cli` tier
that is not installed at all is left out, since it can never answer and carrying it would
put a fallback in the rail that never happens.

## What counts as stuck

A turn is abandoned, and retried on the next tier, when the active tier:

- **repeats itself** — the same line, or the same twelve-token span, often enough to be a
  loop rather than an answer;
- **stops making progress** — the same tool call with the same arguments over and over, a
  run of tool calls that all fail, or the *same kind* of failure from the same tool three
  times. Arguments are compared as parsed JSON rather than as the text that arrived, so a
  call re-spelled — a space moved, keys reordered — is still the same call, and a model
  cannot rewrite its way out of being caught. Reading three different files is work; reading
  the *same* missing path three times is not, and hunting for a `.env`, a `Makefile` and a
  `pyproject.toml` that are not there is investigation rather than a stall;
- **goes quiet** — nothing within `first_token_timeout_ms`, or silence past
  `idle_timeout_ms` once it has started. A slow tool call is not a stall: any real response
  resets the clock;
- **fails** — unreachable, an HTTP error, a broken stream;
- **runs out of steps** without reaching an answer.

Timeouts depend on where the model actually is, because one profile cannot fit both ends:

| | first token | idle | |
| --- | --- | --- | --- |
| **local** — loopback, LAN, `.local` | 120s | 30s | time to load weights, then less patience once it is streaming |
| **hosted** — anything over the network | 30s | 60s | silence means something is broken; a gap mid-answer does not |

A `cli` tier is judged as hosted even though the process is local: its first line of output
includes the harness starting up, which is slow for reasons a model server is not. Every
value can be overridden per tier, and an omitted one keeps its class's default:

```toml
[tier.limits]
idle_timeout_ms = 45000   # this one only; the first-token budget stays local
```

In the transcript you see why a tier was given up on, said *before* the move rather than
with it, so the handoff is narrated rather than sudden:

```
you     explain the parser
·       ✗ Looping Local (http://127.0.0.1:8732/loop/v1) repeated the same output 4 times
          — spilling over to Healthy Fallback (http://127.0.0.1:8732/healthy/v1)
spill   Recovered on the second tier.
```

### Why it gave up, in numbers

Handing a turn to another model is the one decision spill makes on its own, so it is the one
that has to be arguable. `/why` prints every counter the verdict was made from — including
the ones that never fired:

```
Looping Local was abandoned: read_file failed 3 times with the same error (no such file)

decided from:
  steps       6 of 12 used
  repeated    1 identical line(s) in a row, allowed 4
  spans       worst recurring span seen 1 time(s), allowed 4
  calls       1 identical call(s) in a row, allowed 4
  failures    3 in a row, allowed 4
  same error  no such file x3 (budget 3)
  silence     worst 0.3s of a 30.0s idle allowance
  budgets     120.0s first token, 30.0s idle, rate limited at 4 repeats
```

Note the `same error` line: the *same* failure three times tripped a budget of three while
the tier's own allowance was four. That kind of thing is invisible without the numbers, and
it is exactly what gets tuned.

A turn that **stayed** can still have been close, and a near miss nobody is told about
teaches nothing about whether a threshold is right, so a one-line warning says so:

```
·       nearly spilled · Local repeated the same line 3 of 4 times · /why
```

### The record of every spill

One stall is answered by `/why`; a pattern of them is answered by the file. Every spill is
appended to `~/.local/state/spill/spills.jsonl` (Linux), one JSON record per line, written
in a single piece so two processes spilling at once cannot splice their lines together:

```json
{"at":"2026-09-12T10:00:00Z","calls":{"allowed":4,"class":"no such file","failures":3,"identical":1,"same_error":3},"from":"local","policy":"escalate","reason":"read_file failed 3 times with the same error (no such file)","repeats":{"allowed":4,"lines":1,"span":1},"steps":{"allowed":12,"used":6},"to":"frontier","trigger":"same_tool_error","turn":7,"wait":{"allowance_ms":30000,"first_token_ms":120000,"idle_ms":30000,"phase":"idle","worst_ms":300}}
```

`trigger` is a stable token rather than the prose summary, so the file can be grouped and
counted by it — matching on prose is how a log quietly stops working the first time a
message is reworded. `policy` says whether a handover or a consult actually ran, or
`"ended"` for a turn that stalled with no tier below it. Every counter is in there with the
allowance it was measured against, because a count means nothing without the number it was
supposed to stay under:

```sh
# every spill in this log, newest last, one line each
jq -r '"\(.at) \(.from) -> \(.to // "nowhere") [\(.policy)] \(.trigger): \(.reason)"' \
  ~/.local/state/spill/spills.jsonl

# how often each trigger is what abandoned a turn
jq -r .trigger ~/.local/state/spill/spills.jsonl | sort | uniq -c | sort -rn
```

If the log cannot be written, spill says so once and carries on. Thresholds being tuned from
a file that has quietly stopped growing is worse than having no file.

## Consult instead of escalating

Escalating changes which model you are using for the rest of the session: the cheap one is
abandoned, and the expensive one pays for the whole conversation again. A **consult** is the
alternative — keep the cheap model driving, and spend the bigger one on one narrow question:

```toml
[[tier]]
id = "local"
kind = "openai"
preset = "lmstudio"
on_stuck = "consult"      # what you get by default
consults_per_turn = 2     # the default
```

The next tier is asked a question built from evidence spill already holds: your own words,
the reason the tier was judged stuck, and the raw tool call with its raw error — or, when it
was repeating itself, the output that repeated. Never a summary written by the model that
got stuck, because the model that got stuck is the one that does not understand the problem.
The answer comes back as advice, and the same tier carries on with it.

An endpoint consultant is given **no tools**, so it cannot act — it has to answer, and the
call is one round trip rather than an agent loop. That is what makes the answer prose, and
why consult is the cheaper move.

A `cli` consultant runs its own harness and brings its own tools, which spill cannot
withhold. So it is held read-only instead, launched with whatever read-only mode that CLI
has (`--permission-mode plan`, `--sandbox read-only`, a Q&A mode), and a hard instruction
not to act leads the question. A CLI with no read-only mode is **never asked as a
consultant**: the turn escalates instead, rather than asking a model to behave when nothing
enforces it. A `cli` consult is still a full agent run, so it is not the cheap call that
consulting an endpoint is.

It is not always the right move, so it falls back rather than insisting: with no tier below,
with the turn's consult budget spent, or if the consult fails, is stopped or comes back
empty, the turn **escalates** instead. A stuck turn is therefore never stranded, and consult
can never cost more than the escalation it replaced.

**You do not have to decide in a file.** `/on-stuck escalate|consult|auto` switches the
whole chain for the session, and `/consult` asks for **one** consult on the next stall
without changing anything — the way to find out whether it helps before committing to it.
The session panel says which policy is live, and marks a policy you chose rather than one
from your configuration:

```
fallback   sticky
on stuck * escalate
```

The `*` means the session chose it; without it, the policy is the answering tier's own,
which can differ per tier. Restarting returns to the config either way, and `/on-stuck auto`
hands the choice back to each tier immediately — the only way to return to a configuration
where two tiers differ.

## Tiers

A tier is one model that can answer a turn. Two kinds, mixable in any order.

**`openai`** — anything speaking the OpenAI `/chat/completions` shape.

```toml
[[tier]]
id = "local"
kind = "openai"
preset = "lmstudio"      # or base_url = "http://192.168.1.50:1234/v1"
model = ""               # empty: use whatever the server has loaded
```

**`cli`** — an agent CLI used as a tier. It brings its own agent loop and tools, so spill
hands it the conversation and streams back what it produces. This is what lets an existing
subscription be a fallback with no API key.

```toml
[[tier]]
id = "deepseek"
kind = "cli"
preset = "command-code"  # uses your existing CLI login
model = "deepseek/deepseek-v4-flash"
```

Anything else works too: give a `cli` tier a `bin` and `args` and it is driven through plain
text output, which is what nearly every agent CLI prints by default. A `cli` tier can also
**keep its own conversation between turns** — the shipped `grok` and `command-code` presets
do — by setting `session_args` and `resume_args`, so a long session is not paying to re-send
the whole transcript every turn. Full configuration is in
[`config.example.toml`](config.example.toml).

A `cli` tier signs in for itself, so spill **removes the model credentials it knows about**
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `GEMINI_API_KEY` and their siblings)
from that CLI's environment. Otherwise a key exported for one of your endpoint tiers would
quietly take precedence over the CLI's login and bill a different account. Nothing else
about the environment is touched, and `run_shell` is left alone, since that runs your
command and should see what your shell would. If any of those variables are set, spill says
so at startup rather than letting the removal be invisible.

## The interface

The top line is the tier chain, and it is the part worth watching: the answering tier is a
filled block, any tier spilled past is marked with a cross, and the state word at the far
right spins while a turn is in flight. That line also says how fast the model is going
(`~42 tok/s`), inferred from the characters coming back rather than trusted from a figure
that only arrives once the turn is over — and counted from the first character, not from
your prompt, because a local model spends its first seconds loading and counting that as
writing is what makes a fast model look slow.

Below that is the conversation, with the markdown rendered rather than shown raw: headings
take weight, inline code is styled, a fenced block becomes a framed box with its language on
the top edge, bullets become dots. Tool calls carry a glyph that says how they went — a
spinner while one is running, `✓` when it worked, `✗` when it did not, `!` for something
worth noticing — and the colour agrees with the glyph rather than replacing it.

On a terminal 104 columns or wider, a panel on the right shows a row per tier with its
state, the fallback and stuck policies, the working directory, the session's token and cache
totals, and a sparkline of per-turn spend — the only place the real cost of a fallback is
visible. On a narrower terminal the conversation takes that space and the active tier moves
down to the footer.

![the command menu, opening as you type a slash](assets/commands.png)

## Keys

`Enter` send · `Tab` or `Shift+Tab` switch mode · `Esc` stop, or quit · `Ctrl-C` quit ·
`PageUp` / `PageDown` scroll · `?` help

`Esc` does the nearest thing first: it stops a turn that is running, and only quits when
there is nothing to stop. Stopping is not spilling over — the model you chose stays the
model you chose, the work done so far stays in the conversation, and nothing half-written
reaches the transcript. A tool that is running is killed rather than waited out. `Ctrl-C`
always quits, from anywhere, including the approval prompt and the help overlay. Pasting is
bracketed, so a multi-line paste arrives as one block with its line breaks intact.

## Modes

**build** is the default: it reads, writes files and runs commands, asking first.

**plan** is read-only. Ask it to look into something and you get a plan — what it would
change, file by file, in what order, and what it needs decided first — instead of an edit.
The guarantee is not a request in a prompt: the write tools are not offered to the model at
all, and if one is named anyway — a hallucinated tool, or a habit carried over from build
mode — the call is refused before it reaches the disk, and the model is told why so it can
get on with the plan rather than retrying. While plan mode is on the prompt box is drawn in
the warning colour and says `plan`, so the mode you are in is never a guess.

![spill in plan mode: the prompt box says so, and the answer is a plan](assets/plan.png)

## Commands

Type `/` to open the menu of everything below. `?` shows them alongside every key.

The ones about the chain are what a single-model agent cannot offer:

| Command | What it does |
| --- | --- |
| `/tier`, `/tier <name\|number>`, `/tier auto` | Show the chain, answer from one tier, or go back to the configured order |
| `/escalate` | Spill to the next tier now, without waiting for a stall |
| `/consult` | Ask the tier below one question about the next stall, and keep driving |
| `/on-stuck <escalate\|consult\|auto>` | What a stuck tier does for the session; `auto` goes back to each tier's own |
| `/why` | Why the last tier was abandoned, counter by counter |
| `/retry [tier]` | Send the last turn again, here or somewhere else |
| `/drop` | Discard the active tier's own conversation and start it fresh |
| `/undo` | Put back the newest approved write, and reach back from there |
| `/cost`, `/context`, `/compact` | What it has spent, what gets sent, and folding earlier turns into a ledger |
| `/allow`, `/sticky`, `/clear`, `/help`, `/quit` | Rules that skip a prompt, the session's spill policy, a fresh start, and the obvious two |

Two details that matter:

- A slash only means a command at the **start of a line**, and only for a name we know. So
  "what is in `/usr/local/bin`" is a question, not an instruction. Write `//` to start a
  message with a literal slash.
- `/compact` is deterministic rather than a model summary: it keeps the last few turns and
  replaces the rest with a one-line-per-action ledger, including which tools ran, because a
  file that was written stays written. It costs nothing, works while the active tier is
  misbehaving, and declines if the ledger would be larger than what it replaces. Running
  low on context is not something you have to notice either: when a tier is abandoned and
  the session has grown past a few turns, the history is compacted as part of spilling over.

## Nothing runs without asking

Every tool that can change something stops and shows you what it is about to do. `y` runs
it, `n` skips it, and the model is told it was declined so it can try another way rather
than repeating itself. When the preview is longer than the box, `↑`/`↓` and `PageUp` /
`PageDown` read the rest of it with the position shown in the title, so you are never asked
to approve a change you cannot see.

![the approval prompt, reading a diff too long for one screen](assets/approval.png)

`run_shell` handed a whole command line is the one that asks most often, so read-only
commands do not:

```
ls  pwd  cat  head  tail  wc  file  stat  which  du  df  tree  grep  rg
git status  git diff  git log  git show  git branch  git blame
git grep  git rev-parse  git describe  git shortlog
```

Each is read-only by construction, and each is the shell spelling of something spill already
does without asking — `read_file`, `list_dir`, `glob` and `grep` run unprompted — so none of
them adds reach the agent did not have. What they remove is a modal. When a rule covers a
command, the transcript still shows the command *and* says why nobody was asked:

```
· run_shell runs without asking (rule: git status)
→ run_shell  run in ~/code:
             git status --short
```

Everything else asks, including the commands whose *flags* can write or execute — `find`
(`-delete`, `-exec`), `sort` (`-o`), `sed` (`-i`), `xargs`, `awk`, `make`, `cargo`, `npm`,
`python`. Leaving them out costs a question; putting them in would be a hole shaped like a
flag.

**A rule is a prefix of words, never a pattern over the line.** `"git status"` is a prefix
of `"git status; rm -rf ~"`, so a rule only ever matches a command that is a *bare word
list*: if the line contains `;`, `&`, `|`, `<`, `>`, a backtick, `$`, `(`, `)`, `{`, `}`,
`[`, `]`, `*`, `?`, `~`, a quote or a backslash, no rule can cover it and it always asks —
even when the character is harmless inside quotes. `ls; rm -rf ~` asks. So does `FOO=bar ls`,
because the first word is not `ls`.

Yours are one command away:

```
> /allow git push              # this session only
> /allow save cargo test       # and write it into config.toml
> /allow                       # what is in force, and what is stuck for now
> /allow clear                 # forget this session's rules
```

`/allow save` writes *everything in force* rather than just the new rule, because the key
**is** the list: writing one rule alone would drop the read-only defaults. The edit is
surgical — one line in `[general]` — so every comment and every other setting in that file
is left exactly as you wrote it. `allow_shell = []` is how you go back to being asked about
everything.

### Undoing a write

`/undo` restores the file the last approved write changed, using the bytes already read to
show you the diff before it happened — and then reaches back to the write before that. The
stack holds **ten writes, or 8 MiB of remembered contents**, whichever comes first, so the
newest is always reachable.

```
> /undo
· restored src/a.rs (412 bytes, as it was before the write_file) — 2 more writes can still be put back

> /undo                        # the write before it had created the file
· removed src/b.rs and the 2 directories it created (created by the write_file) — that was the last write on the stack

> /undo                        # somebody edited src/a.rs in the meantime
· src/a.rs has changed since that write — leaving it alone. Nothing was changed. (2 more writes are behind it)
```

It refuses rather than guessing: if the file is not still exactly what the write left — you
edited it, another tool did, it was deleted — it says so and changes nothing. That is why
`/undo` does not ask for approval first: you typed it, and the thing it protects you from is
not yourself. An entry it refuses stays on the stack, so the ones behind it are blocked
until you put the file back by hand and retry, because undoing *out of order* would be worse
than being told why. It covers the file tools only: a `run_shell` command can do anything,
so there is no honest way to reverse one.

It pairs with spilling over, which is why it is here at all. A tier that wrote junk and then
looped is *abandoned*, but its side effects are not undone — the next tier inherits a
workspace containing them:

```
you     refactor the parser
·       ✗ Looping Local repeated the same output 4 times — spilling over to DeepSeek
spill   Recovered on the second tier.
> /undo
· restored src/parser.rs (2,104 bytes, as it was before the write_file)
```

### Text is never a side effect

A tier that stalls has its text thrown away: the turn is rolled back to the checkpoint
before the next model reads anything, and the CLI's own session is dropped so it does not
resume the conversation that held the thrown-away answer. That matters more than it sounds,
because a model's prose can look exactly like an action — `Here is the fix:` followed by a
diff, or a line that reads like a command. None of it is ever applied. The only thing that
can change your workspace is a **structured tool call** that was approved and ran, and there
is exactly one place in the code where one of those can become an action.

The inverse is also true, and worth knowing: a tool call that *did* complete before the
stall is not undone by the handover. Its result is a real record, and `/undo` is how you
take it back.

One honest caveat: a delegated `cli` tier runs its own harness and its own tools in its own
process. Spill neither parses nor replays that, and cannot withhold tools from a program it
does not run — so if the CLI acted before the turn was abandoned, that is the CLI acting,
not spill applying prose.

## It remembers

Quit and come back in the same directory, and spill picks up where it left off: the
conversation, the tier it had settled on, whether a tier was pinned, the sticky choice, the
stuck policy, and the mode.

```
> spill
resumed this session — 42 messages, last saved 3h ago
```

Sessions are **per workspace directory**, so two projects keep two conversations. They live
in your state directory (`~/.local/state/spill/sessions/` on Linux), one file per workspace,
written `0600`; deleting the file is a clean first run. What is saved is the conversation
the models saw, not the interface — tool spinners, notices and escalation markers are not
replayed. Token and cost totals start fresh, since they describe a session rather than a
workspace. `/clear` empties what is on disk as well as what is on screen, because otherwise
resuming would bring back the conversation you just dropped.

## Scripts and pipelines

`-p` answers one prompt and exits, running the same tiers with the same spill-over:

```sh
spill -p "summarise this project"
spill -p "what does the parser do?" --output-format json
```

There is nobody to approve a tool, so writes and shell commands are **refused** unless you
pass `--yolo`:

```sh
spill -p "add a doc comment to main.rs" --yolo
```

`-p` never resumes and never writes a session — a script that picked up yesterday's
conversation because it ran in the same directory would be a trap. The JSON form reports the
answer, which tier gave it, and any tiers the run spilled through on the way:

```json
{
  "text": "the second tier answered",
  "answeredBy": "Backup Agent",
  "escalations": [
    "Box That Is Off (http://127.0.0.1:9/v1) could not reach ... → Backup Agent"
  ],
  "stopReason": "end_turn",
  "usage": { "inputTokens": 15754, "outputTokens": 3 },
  "error": null
}
```

The exit code is `0` on success and `1` on failure, so `if ! spill -p ...; then` works.
Started without a terminal and without `-p`, spill says so and points at these two instead
of failing with an OS error.

## When it does not work: doctor

```sh
spill doctor
```

```
spill 0.2.0 — doctor

config    /home/you/.config/spill/config.toml
          found at the default path · workspace ~ · sticky fallback on

ok    local    openai  http://localhost:1234/v1
               would use qwen3-coder-30b  (5 ms)
FAIL  offline  openai  http://127.0.0.1:9/v1
               could not reach http://127.0.0.1:9/v1/models: could not connect  (0 ms)
ok    grok     cli     grok
               is installed  (0 ms)
               binary: /home/you/.local/share/mise/installs/node/bin/grok
               sessions: on · opens with -s {session} · resumes with -r {session}
               consult: available read-only · --permission-mode plan

2 of 3 tier(s) usable. spill will start on the first one that answers.

delegated CLIs sign in for themselves, so spill removes these from their
environment before they start — a key exported for another tier cannot change
whose account is billed:
  ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN OPENAI_API_KEY XAI_API_KEY
  GROK_API_KEY GEMINI_API_KEY GOOGLE_API_KEY GOOGLE_GENERATIVE_AI_API_KEY
  OPENROUTER_API_KEY DEEPSEEK_API_KEY
  * set here right now, so this is the removal doing something: XAI_API_KEY

this machine
  terminal  110x40
  colours   truecolor
  needs     nothing installed for spill itself: no Rust toolchain and no runtime
```

It answers "it does not work on my machine" before anyone has to ask: which file is in
force, every tier with what it would actually use and how long that took, where each CLI
really is and what flags it will run with, which variables are taken out of its environment,
and what the terminal is giving the interface.

Two lines are worth reading first. The **config path**: a path you did not expect means you
are editing the other file, and *not there yet* means the built-in defaults are in force.
And **`binary:`**, because "grok is installed" cannot say *which* grok, and a second copy
earlier on `PATH` is the usual reason a tier behaves differently here than in your shell.
A tier whose stuck policy is not the default says so, since handing a turn over is a choice
worth being able to audit.

**API keys are never printed**, only the *name* of the variable one would come from, so a
doctor report is safe to paste into an issue. `--json` produces the same report for a
script, carrying every field the prose leaves out, and exits non-zero when no tier is
usable.

## Configuration

`spill setup` writes `~/.config/spill/config.toml` for you, which is the easier route. To
write it yourself, copy [`config.example.toml`](config.example.toml) — a working two-tier
config rather than a template with everything switched on.

API keys are referenced by environment variable name and never stored in the file:

```toml
[[tier]]
id = "hosted"
kind = "openai"
preset = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
model = "deepseek/deepseek-v4-flash"
```

A bad config names the tier and field that caused it, rather than failing later:

```
spill: tier "local" is kind = "openai" but has neither a base_url nor a preset;
       set preset = "lmstudio", or give the API root such as http://localhost:1234/v1
```

spill also warns at startup about the things that would otherwise fail silently: a key
variable that is not set, or an agent CLI that is not on `PATH`.

**On Windows**, write paths in single quotes — `workspace = 'C:\Users\me\project'`. Inside
double quotes a backslash starts an escape sequence, so `"C:\Users"` is not valid TOML at
all, and `"C:\x86"` quietly parses into something that is not the path you meant. spill
rejects both with that explanation rather than leaving you to work it out.

## Status

Working today: configuration and validation, the terminal UI, OpenAI-compatible streaming
with model auto-discovery, the agent loop with seven tools and approval, stuck detection and
tier escalation, consulting a tier instead of escalating, build and plan modes, slash
commands, the read-only shell rule list with `/allow`, `/undo` ten writes deep, session
continuity for CLI tiers, sessions that survive a restart, per-class timeouts, cost reporting
per tier, the setup wizard (which checks every tier before it will write), zero-config first
run, `doctor`, and `-p`.

The awkward paths are pinned by fixtures that drive the real provider, detector and tools
against a local endpoint instead of a live model — a model looping on the same line, a
collapse arriving in the ragged frames a server actually sends (a sentence split mid-word,
two lines in one frame), a model re-asking for the same file in new words every turn, a tier
that goes quiet *after* a tool, a consult that fails, and a hunt for files that are not
there. A change that quietly breaks failover fails `cargo test` instead of reaching you.
Each detector is also tested on its own, fed the counters it is judged by, so a refactor
that leaves the loop working by accident still has to keep the detectors working on purpose.

The tool set is deliberately closed at seven. No MCP client, no browser, no image generation
in 0.x: a tool has to earn its place, and every tool added is another way for a small local
model to get stuck.

Not yet: the **AUR package** is written and verified but not submitted, because new account
registration at the AUR is closed — so `yay -S spill-bin` does not work yet. Windows `winget`
and `scoop` manifests are also still to come.

## License

MIT — see [LICENSE](LICENSE).
