# spill

[![CI](https://github.com/randallyash/spillover/actions/workflows/ci.yml/badge.svg)](https://github.com/randallyash/spillover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/randallyash/spillover)](https://github.com/randallyash/spillover/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Local until it isn't.**

An agentic coding assistant for the terminal that runs on your own machine first, for
free and in private, and reaches for a hosted model only when your local one actually
misbehaves. No retyping, no cancelling, no watching it loop.

![spill: a local model stalls, and the turn moves on](assets/hero.png)

Your prompt goes to the local model. When that model repeats itself, stops making
progress, goes quiet, or fails outright, spill abandons it and re-runs the same turn on
the next model in your list. The abandoned answer is thrown away rather than inherited,
so the model that takes over starts from your question and not from someone else's
half-finished loop. You keep using the free model and the paid one stays where it
belongs: unspent, until the moment it is needed.

```
tier 1  local model       free, private, no API key — tried first, every turn
tier 2  first fallback    reached only when tier 1 misbehaves
tier 3  last resort
```

## Why this is useful

Local models are free, private, and good enough for a lot of work, but they occasionally
hang or degenerate into repeating themselves. Most tools leave you to notice that,
cancel, and start over somewhere else. spill treats it as expected:

- **You keep using the free local model.** It is tier 1, so that is where your prompts
  go by default — and it is where they keep going, turn after turn, as long as it
  behaves. The paid models below it are reached only when the local one actually fails.
- **A stuck model costs you nothing.** No watching for a loop, no cancel-and-retype. The
  turn is re-issued on the next tier by itself, in the same conversation, with the
  reason written into the transcript.
- **One interface, any model.** Tiers are generic: any OpenAI-compatible endpoint
  (LM Studio, Ollama, llama.cpp, vLLM, OpenRouter, xAI) or any agent CLI you already
  have installed and logged in (`cmd`, `grok`, `claude`, `codex`, `gemini`, and
  others). An existing CLI subscription works as a fallback with no separate API key.
- **It is an agent, not just a chat window.** It reads, searches, edits files and runs
  commands, asks before anything that writes, and renders the answer as markdown
  instead of showing you its asterisks.

## What is underneath

The fallback is the headline, but it is not the only thing here. These are the parts
worth knowing about once the basics make sense.

| | |
| --- | --- |
| **Consult a tier instead of handing over** | Escalating changes which model you use for the rest of the session. `on_stuck = "consult"` keeps the cheap model driving and spends the big one on one narrow question, built from the raw tool error rather than from the stuck model's own account of the problem — and `/on-stuck consult` turns it on for the session without editing a file. See [Consulting instead of handing over](#consulting-instead-of-handing-over). |
| **Plan mode** | `Shift+Tab` makes a turn read-only. Not asked for in a prompt — the write tools are withheld from the model, and a call for one is refused before it reaches your disk. See [Modes](#modes). |
| **Commands** | `/tier`, `/escalate`, `/consult`, `/on-stuck`, `/retry`, `/drop`, `/sticky`, `/cost`, `/context`, `/compact`, `/clear`, `/undo`. Type `/` and they appear, with a description beside each. |
| **Cost you can see** | Tokens *and* cache reads, attributed per tier, with a sparkline of per-turn spend. A cross-tier fallback is a cache miss no design can avoid; this is where you find out what it cost. Counted per **request**, which is what a turn is made of — one per tool call, plus any spill or consult — so a turn that read four files is billed as five requests and reported as five. Cost is counted as the tier reports it rather than only off a finished response, so a tier that *failed* — stalled, looped, or stopped by you — still shows what it spent. |
| **Session continuity** | A CLI tier keeps its own conversation across turns and receives only the new message, instead of the whole transcript being flattened into every prompt. |
| **It remembers** | Quit and come back in the same directory and nothing is lost: the conversation, the tier you had settled on, whether one was pinned, the sticky choice, the stuck policy, and the mode. One session per workspace. `-p` neither reads nor writes one, so scripts stay stateless. See [It remembers](#it-remembers). |
| **Undo the last write** | `/undo` puts back the file the last approved write changed, using the very bytes it showed you in the diff before it happened. It refuses when the file has moved on since, so it can never discard work done after — and it says why rather than guessing. See [Undoing a write](#undoing-a-write). |
| **A handoff you can follow** | The reason a tier is being abandoned is announced *before* the move, with the tier's mark flashing in the rail, rather than appearing at the same instant as the next model's answer. Silence during a failover is how you end up distrusting it. |
| **Judged on where it runs** | Timeouts are per class, not per tier: a model on your LAN gets 120s to load its weights and then only 30s of silence once it is streaming, where a hosted tier gets 30s and 60s. The *reason* for a stall narrows too — the same missing path three times is a stall, three different files being read is work. |
| **Stopping a turn** | `Esc` stops it where it stands — including a tool mid-flight, so a build is killed rather than waited out — without changing which model you chose. |
| **Compaction** | Earlier turns fold into a short ledger when a tier is abandoned, so the incoming model's cold read is small. The ledger keeps what the tools *did*, because a discarded conversation does not undo a written file. |
| **Bracketed paste, scrolling diffs** | A multi-line paste arrives intact. A diff too long for the approval box scrolls, with its position shown, so you are never asked to approve something you cannot read. |

## Requirements

- A terminal. Linux, macOS, or Windows.
- To build from source: Rust 1.85 or newer.
- For a local tier: a model server already running. spill looks for one on the usual
  ports and uses it without configuration.
- A terminal of at least 46 columns by 12 rows. Below that it says so rather than
  drawing something garbled.
- Color is optional. `NO_COLOR=1` turns it off, and nothing is lost: every state is
  carried by a glyph, a border, or a weight as well as a hue.

## Install

**macOS and Linux:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/randallyash/spillover/releases/latest/download/spill-installer.sh | sh
```

**macOS, with Homebrew:**

```sh
brew install randallyash/spillover/spill
```

**Windows, with the installer:**

```powershell
powershell -c "irm https://github.com/randallyash/spillover/releases/latest/download/spill-installer.ps1 | iex"
```

There is also a `.msi` on the [releases page](https://github.com/randallyash/spillover/releases)
if you would rather install from a package.

**From source**, with Rust 1.85 or newer:

```sh
cargo install --git https://github.com/randallyash/spillover
```

| Route | Status |
| --- | --- |
| Installer script (macOS, Linux) | works |
| Installer script (Windows), `.msi` | published |
| `cargo install --git` | works |
| Homebrew | `brew install randallyash/spillover/spill` |
| Prebuilt archives, five platforms | on the [releases page](https://github.com/randallyash/spillover/releases) |
| Arch (AUR) | `PKGBUILD` is in [`packaging/arch`](packaging/arch) with verified checksums, not yet submitted to the AUR |

Everything is signed off with `sha256.sum` on each release.

## Quick start

Just run it:

```sh
spill
```

With no configuration yet, spill looks for a local model on the usual ports
(LM Studio, Ollama, llama.cpp, vLLM) and starts on whatever it finds, telling you
which one it picked. For the common case there is nothing to configure.

If nothing is running, it opens the setup wizard instead of leaving you at a prompt
that cannot answer. You can also start it directly:

```sh
spill setup
```

Three steps, and every choice is checked before anything is written:

1. **The local model** — what it found running, an option to type an address for a
   server on another machine, or "no local model".
2. **Online fallbacks** — up to two, in the order you want them tried. Agent CLIs
   first, since they need no API key: `cmd`, `grok`, `claude`, `gemini`, `copilot`,
   `cursor-agent`, `codex`, `opencode`, `crush`. Then hosted endpoints, and a last
   row for **anything not in that list** — type its address and spill reads the
   models it serves, so you pick one rather than guessing at an id. If you would
   rather type an id than scroll to it, it is checked against that list, and a
   near miss is named rather than accepted.
3. **Review** — each tier is checked and its resolved model named, so a wrong URL or
   a missing CLI is caught here rather than mid-conversation. `w` is refused while
   any tier cannot answer, because a config that cannot work is worse than no
   config. Press `w` again to insist, for a server you have not started yet:

   ```
   1. LM Studio  (tried first, openai)
   http://localhost:1234/v1
     ✓ http://localhost:1234/v1 would use qwen3-coder-30b
   2. Command Code  (fallback, cli)
   command-code
     ✓ cmd is installed
   ```

   `w` writes `~/.config/spill/config.toml`. An existing file is copied to
   `config.toml.bak` and confirmed before being replaced, and the new file is mode
   `0600`.

`spill presets` lists every endpoint and CLI a tier can name.

## Tiers

A tier is one model that can answer a turn. There are two kinds and you can mix
them in any order.

**`openai`** — anything speaking the OpenAI `/chat/completions` shape.

```toml
[[tier]]
id = "local"
kind = "openai"
preset = "lmstudio"      # or base_url = "http://192.168.1.50:1234/v1"
model = ""               # empty: use whatever the server has loaded
```

**`cli`** — an agent CLI used as a tier. It brings its own agent loop and its own
tools, so spill hands it the conversation and streams back what it produces. This
is what lets an existing subscription be a fallback with no API key.

```toml
[[tier]]
id = "deepseek"
kind = "cli"
preset = "command-code"  # uses your existing CLI login
model = "deepseek/deepseek-v4-flash"
```

Anything not in the preset list still works: give a `cli` tier a `bin` and `args`,
and it is driven through plain text output, which is what nearly every agent CLI
prints by default. Full configuration is in
[`config.example.toml`](config.example.toml).

A `cli` tier signs in for itself, so spill **removes the model credentials it knows
about** (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `GEMINI_API_KEY` and
their siblings) from the CLI's environment. Otherwise a key exported for one of your
own endpoint tiers would quietly take precedence over the CLI's login and bill a
different account — which matters most for exactly the presets that promise a login,
like `grok` reading your SuperGrok subscription. Nothing else about the environment is
touched, and `run_shell` is left alone, since that runs your command and should see
what your shell would. If any of those variables are set, spill says so at startup
rather than letting the removal be invisible.

A `cli` tier can also **keep its own conversation between turns**. The shipped
`grok` and `command-code` presets do this already: the first turn opens a session,
and every turn after that continues it and sends only the new message — rather
than re-sending the whole transcript inside the prompt each time, which a long
session pays for over and over. For any other CLI, set `session_args` and
`resume_args` (both take `{session}`); a CLI that picks its own session id needs
only `resume_args`, and spill reads the id out of its output. Leave them off and
the tier behaves exactly as before.

## When it spills over

A turn is abandoned, and retried on the next tier, when the active tier:

- **repeats itself** — the same line, or the same twelve-token span, often enough to
  be a loop rather than an answer;
- **stops making progress** — the same tool call with identical arguments over and
  over, a run of tool calls that all fail, or the *same kind* of failure from the same
  tool three times. Reading three different files is work; reading the *same* missing
  path three times is not, and looking for a `.env`, a `Makefile` and a
  `pyproject.toml` that are not there is investigation rather than a stall — so a
  file-shaped failure only counts when the path repeats, while a malformed call counts
  across targets, because the target was never what was wrong;
- **goes quiet** — no response at all within `first_token_timeout_ms`, or silence
  past `idle_timeout_ms` once it has started. A slow tool call is not a stall: any
  real response resets that clock;
- **fails** — unreachable, an HTTP error, a broken stream;
- **uses up its step budget** without reaching an answer.

Timeouts are per tier, and the defaults depend on where the model actually is, because
one profile cannot fit both ends:

| | first token | idle | |
| --- | --- | --- | --- |
| **local** — loopback, LAN, `.local` | 120s | 30s | time to load weights, then less patience once it is streaming |
| **hosted** — anything over the network | 30s | 60s | silence means something is broken; a gap mid-answer does not |

A `cli` tier is judged as hosted even though the process is local: its first line of
output includes the harness starting up, which is slow for reasons a model server is
not. Every value can be overridden per tier — and an omitted one now keeps its class's
default rather than falling back to a global number:

```toml
[tier.limits]
idle_timeout_ms = 45000   # this one only; the first-token budget stays local
```

`spill doctor` shows the timeouts a local tier actually got, and carries the class and
both budgets for every tier in its JSON.

In the transcript you see why a tier was given up on, and it is said *before* the move
rather than with it — the tier's mark flashes in the rail for a beat as the reason
appears, so the handoff is narrated rather than sudden:

```
you     explain the parser
·       ✗ Looping Local (http://127.0.0.1:8732/loop/v1) repeated the same output 4 times
          — spilling over to Healthy Fallback (http://127.0.0.1:8732/healthy/v1)
spill   Recovered on the second tier.
```

### Why it gave up, in numbers

Handing a turn to another model is the one decision spill makes on its own, so it is
the one that has to be arguable. `/why` prints every counter the verdict was made
from — including the ones that never fired, which are usually the interesting ones:

```
> /why
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

Note the third line: the *same* failure three times tripped a budget of three while
the tier's own allowance was four. That kind of thing is invisible without the numbers,
and it is exactly what gets tuned.

A turn that **stayed** can still have been close, and a near miss nobody is told about
teaches nothing about whether a threshold is right, so a one-line warning says so:

```
·       nearly spilled · Local repeated the same line 3 of 4 times · /why
```

**Every spill is appended to a log**, because one stall is answered by `/why` and a
pattern of them is answered by the file. On Linux that is
`~/.local/state/spill/spills.jsonl`, one JSON record per line. This is a real record,
not a tidied-up one:

```json
{"at":"2026-09-12T10:00:00Z","calls":{"allowed":4,"class":"no such file","failures":3,"identical":1,"same_error":3},"from":"local","policy":"escalate","reason":"read_file failed 3 times with the same error (no such file)","repeats":{"allowed":4,"lines":1,"span":1},"steps":{"allowed":12,"used":6},"to":"frontier","trigger":"same_tool_error","turn":7,"wait":{"allowance_ms":30000,"first_token_ms":120000,"idle_ms":30000,"phase":"idle","worst_ms":300}}
```

`trigger` is a stable token rather than the prose summary, so the file can be grouped
and counted by it — matching on prose is how a log quietly stops working the first
time a message is reworded. `policy` says whether a handover or a consult actually
ran, or `"ended"` for a turn that stalled with no tier below it. That last case is
logged deliberately: a single-tier setup can only ever end that way, and those are the
thresholds most worth tuning.

Every counter is in there, including the allowances they were measured against, which
is the point — a count means nothing without the number it was supposed to stay under.
Since it is JSON Lines, `jq` does the reading:

```sh
# every spill in this log, newest last, one line each
jq -r '"\(.at) \(.from) -> \(.to // "nowhere") [\(.policy)] \(.trigger): \(.reason)"' \
  ~/.local/state/spill/spills.jsonl

# how often each trigger is what abandoned a turn
jq -r .trigger ~/.local/state/spill/spills.jsonl | sort | uniq -c | sort -rn
```

A record is written in one piece, so several `spill` processes spilling at the same
moment cannot splice their lines together — an append is atomic against other appends,
but only for a single write.

If the log cannot be written, spill says so once and carries on: thresholds being
tuned from a file that has quietly stopped growing is worse than having no file.

Three things worth knowing:

- The abandoned tier's **conversation is discarded**, so the next tier never inherits
  a half-finished answer.
- Its **side effects are not**. If the tier that stalled already wrote a file or ran
  a command, that happened. Those results stay in the history so the next tier can
  see what was already done.
- A `cli` tier's **session is dropped** too, so it starts a fresh conversation next
  time rather than resuming one that holds the answer that was thrown away.

### Consulting instead of handing over

Escalating changes which model you are using for the rest of the session: the cheap
one is abandoned, and the expensive one pays for the whole conversation again. A
**consult** is the alternative — keep the cheap model driving, and spend the bigger
one on one narrow question.

The default is `escalate`, and it stays the default: consulting is newer, and the
tiers where it pays off are the ones you would want to be sure of first.

```toml
[[tier]]
id = "local"
kind = "openai"
preset = "lmstudio"
on_stuck = "consult"      # default is "escalate"
consults_per_turn = 2     # the default
```

You do not have to decide in a file, though. `/on-stuck consult` switches every tier
for the rest of the session, and `/consult` asks for **one** consult on the next stall
without changing anything — which is the way to find out whether it helps before
committing to it.

Because a session choice flattens the whole chain to one answer, `/on-stuck auto` hands
the choice back to each tier's own, which is the only way to return to a configuration
where two tiers differ:

```
> /on-stuck auto
back to the configured policies — Local consult, Frontier escalate
```

The session panel says which policy is live, and marks a policy you chose rather than
one from your configuration:

```
fallback   sticky
on stuck * consult
```

The `*` means the session chose it; without it, the policy is the answering tier's own
from your configuration, which can differ per tier. Restarting returns to the config
either way.

When that tier gets stuck, the next tier in the chain is asked a question built from
evidence spill already holds: your own words, the reason the tier was judged stuck, and
the raw tool call with its raw error — or, when it was repeating itself, the output that
repeated. Never a summary written by the model that got stuck, because the model that
got stuck is the one that does not understand the problem. The answer comes back as
advice, and the same tier carries on with it.

An endpoint consultant is given **no tools**, so it cannot act — it has to answer, and the
call is one round trip rather than an agent loop. That is what makes the answer prose, and
it is why consult is the cheaper move when the answer is something the driver can act on.

It is not always the right move, so it falls back rather than insisting:

- when the budget for the turn is spent, the turn **escalates** as it always did;
- if the consult fails, is stopped, or comes back empty, the turn **escalates** too;
- with no tier below, there is nobody to ask, so it **escalates**;
- and a consultant that cannot be held read-only is not asked at all, so it **escalates**.

A stuck turn is therefore never stranded, and consult can never cost more than the
escalation it replaced.

A `cli` consultant runs its own harness and brings its own tools, which spill cannot
withhold the way it can for an `openai` endpoint. So it is held read-only instead: it is
launched with whatever read-only mode that CLI has — `--permission-mode plan`, `--sandbox
read-only`, a Q&A mode, and so on — and a hard instruction not to act leads the question.
The shipped presets set that flag wherever the CLI offers one. A CLI with no read-only
mode is **never asked as a consultant**: the turn escalates instead, with the reason said
out loud, rather than asking the model to behave when nothing enforces it. A `cli`
consult is still a full agent run, though, so it is not the cheap call that consulting an
endpoint is.

## One-shot use

For scripts and pipelines, `-p` answers one prompt and exits:

```sh
spill -p "summarise this project"
spill -p "what does the parser do?" --output-format json
```

It runs the same tiers with the same spill-over. There is nobody to approve a tool,
so writes and shell commands are **refused** unless you pass `--yolo`:

```sh
spill -p "add a doc comment to main.rs" --yolo
```

The JSON form is for scripts — it reports the answer, which tier gave it, and any
tiers the run spilled through on the way:

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
Started without a terminal and without `-p`, spill says so and points at these two
instead of failing with an OS error.

## Diagnosing a problem

```sh
spill doctor
```

```
spill 0.1.0 — doctor

ok    local    openai  http://localhost:1234/v1
               would use qwen3-coder-30b  (5 ms)
               on stuck: consult
FAIL  offline  openai  http://127.0.0.1:9/v1
               could not reach http://127.0.0.1:9/v1/models: could not connect  (0 ms)
ok    grok     cli     grok
               is installed  (0 ms)

2 of 3 tier(s) usable. spill will start on the first one that answers.
```

Every configured tier is checked, with what each would actually use, how long it
took, and why anything failed. A tier whose stuck policy is not the default says so,
because consult is a choice worth being able to audit — and the plain-text report shows
it only when it is not `escalate`, so an ordinary configuration stays as short as it
was. The JSON always carries `onStuck` for every tier, so a script never has to infer
it from a missing line. `--json` produces the same report for a script, and the exit
code is non-zero when no tier is usable.

**API keys are never printed** — only the *name* of the environment variable one
would come from. That is deliberate, so a doctor report is safe to paste into an
issue.

## Configuration

`spill setup` writes `~/.config/spill/config.toml` for you, which is the easier
route. To write it yourself, copy [`config.example.toml`](config.example.toml).

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

**On Windows**, write paths in single quotes — `workspace = 'C:\Users\me\project'`.
Inside double quotes a backslash starts an escape sequence, so `"C:\Users"` is not
valid TOML at all, and `"C:\x86"` quietly parses into something that is not the
path you meant. spill rejects both cases with that explanation rather than leaving
you to work it out.

spill also warns at startup about the things that would otherwise fail silently: a
key variable that is not set, or an agent CLI that is not on `PATH`.

## Interface

The top line is the tier chain, and it is the part worth watching: the answering tier is
drawn as a filled block, any tier that was spilled past is marked with a cross, and the
state word at the far right spins while a turn is in flight. When the chain does not fit,
the rail shortens the tier names, and then falls back to just the answering tier and its
position ("2/3") rather than clipping the chain.

That line also says how fast the model is going: `~42 tok/s` while text arrives, and
`ready` when nothing is. The tilde is the honest part. A tier reports tokens only if it
chooses to and only once it has finished, so a figure that moves has to be inferred from
the characters coming back, divided by a ratio learned from any turn that did report real
usage. The clock starts at the first character rather than at your prompt, because a local
model spends its first seconds loading and reading — and counting that as writing is what
makes a fast model look slow.

The number keeps a slot of its own on the rail rather than taking whatever the chain
happens to leave over. That is deliberate: the chain re-measures itself as the terminal
changes size and steps up to a longer name the moment one fits, so a number riding on the
leftover would keep being squeezed out — and then reappear as the terminal grew, which
reads as the number not existing rather than as a narrow window. On a terminal too narrow
to hold the chain, the state word and the number all at once, the chain and the word stay
and the number is not shown.

Below that is the conversation. The model's markdown is rendered rather than shown raw:
headings take weight, inline code is styled, a fenced block becomes a framed box with its
language on the top edge, bullets become dots, and a quote gets a bar down its side. A bar
down the left marks everything you wrote, so your own turns are findable in a long
session. Tool calls carry a glyph that says how they went — a spinner while one is
running, `✓` when it worked, `✗` when it did not, `!` for something worth noticing — and
the color agrees with the glyph rather than replacing it.

On a terminal 104 columns or wider, a panel on the right shows the chain as a row per tier
with its state, the fallback policy and the stuck policy in force, the working directory,
the session's token and cache totals, and a sparkline of per-turn spend, which is the only
place the real cost of a fallback is visible. On a narrower terminal the conversation takes that space and the active tier
moves down to the footer instead.

The prompt leads with `❯`, so the line you type on reads as a command line rather than as
text that happens to sit in a box.

## Keyboard

`Enter` send · `Shift+Tab` switch mode · `Esc` stop or quit · `Ctrl-C` quit · `PageUp` / `PageDown` scroll

`Esc` does the nearest thing first: it stops a turn that is running, and only quits
when there is nothing to stop. Stopping is not spilling over — the model you chose
stays the model you chose, the work done so far stays in the conversation, and nothing
half-written reaches the transcript. It also stops a tool that is running: a build or a
test is killed rather than waited out. `Ctrl-C` always quits, from anywhere, including
the approval prompt and the help overlay.

Before anything that can change your files, a prompt appears showing exactly what it
will do. `y` runs it, `n` skips it — and the model is told it was declined, so it can
try another way rather than repeating itself. When the preview is longer than the box,
`↑`/`↓` and `PageUp`/`PageDown` read the rest of it, with the position shown in the
title, so you are never asked to approve a change you cannot see.

![the approval prompt, reading a diff too long for one screen](assets/approval.png)

Pasting is bracketed, so a multi-line paste arrives as one block with its line breaks
intact rather than as a stream of keystrokes.

## Modes

`Shift+Tab` switches between two ways to work. `Tab` does the same whenever you are not
part-way through typing a command.

**build** is the default: it reads, writes files and runs commands, asking first.

**plan** is read-only. Ask it to look into something and you get a plan — what it would
change, file by file, in what order, and what it needs decided first — instead of an
edit. While plan mode is on, the prompt box is drawn in the warning colour and says
`plan`, so the mode you are in is never a guess.

The guarantee is not a request in a prompt. In plan mode the write tools are not offered
to the model at all, and if one is named anyway — a hallucinated tool, or a habit carried
over from build mode — the call is refused before it reaches the disk. The model is told
why, so it can get on with the plan rather than retrying.

![spill in plan mode: the prompt box says so, and the answer is a plan](assets/plan.png)

## It remembers

Quit and come back in the same directory, and spill picks up where it left off: the
conversation, the tier it had settled on, whether a tier was pinned, the sticky choice,
the stuck policy, and the mode. A CLI tier's own conversation is kept too, so the next
turn continues it rather than sending the whole transcript again.

```
> spill
resumed this session — 42 messages, last saved 3h ago
```

Sessions are **per workspace directory**, so two projects keep two conversations. They
live in your state directory — `~/.local/state/spill/sessions/` on Linux — one file per
workspace, written `0600`. Deleting the file is a clean first run.

This is why the transcript is stored rather than a pointer to it. A local model behind an
`openai` tier has no server-side conversation at all: the whole history is re-sent on
every turn, so the file on disk is the only record that exists. Resuming a local session
would otherwise remember nothing.

What is saved is the conversation the models saw, not the interface: tool spinners,
notices, and escalation markers are not replayed, so a resumed transcript shows what was
said and what was run without replaying the frames that produced it. Token and cost
totals start fresh, since they describe a session rather than a workspace.

`spill -p` never resumes and never writes — a script that picked up yesterday's
conversation because it ran in the same directory would be a trap. `spill doctor` and
`spill presets` do not touch the file either.

`/clear` is still how you deliberately start over, and it clears what is on disk as well
as what is on screen. Otherwise resuming would bring back the conversation you just
dropped, which would make `/clear` a lie.

## Commands

Type `/` to open the menu of everything below. `?` shows them alongside every key.

![the command menu, opening as you type a slash](assets/commands.png)

The ones worth knowing are the ones about the chain, because they are what a
single-model agent cannot offer:

| Command | What it does |
| --- | --- |
| `/tier` | Show the chain, with each tier's number and state |
| `/tier <name\|number>` | Answer from that tier until told otherwise |
| `/tier auto` | Go back to the configured order |
| `/escalate` | Spill to the next tier now, without waiting for a stall |
| `/consult` | Ask the tier below one question about the next stall, and keep driving |
| `/on-stuck <escalate\|consult\|auto>` | What a stuck tier does for the rest of the session; `auto` goes back to each tier's own |
| `/retry [tier]` | Send the last turn again, here or somewhere else |
| `/drop` | Discard the active tier's own conversation and start it fresh |
| `/sticky <on\|off>` | Whether a spill keeps the lower tier for the session |
| `/why` | Why the last tier was abandoned, counter by counter |
| `/cost` | Tokens and cache reads so far, tier by tier |
| `/context` | What actually gets sent on each turn, and how large it has grown |
| `/compact` | Fold earlier turns into a short ledger to shrink what is sent |
| `/clear` | Start a new conversation, keeping the tiers as they are |
| `/undo` | Put back the last file an approved write changed |
| `/help`, `/quit` | The obvious two |

Two details that matter:

- A slash only means a command at the **start of a line**, and only for a name we
  know. So "what is in `/usr/local/bin`" is a question, not an instruction. Write
  `//` to start a message with a literal slash.
- `/compact` is deterministic rather than a model summary: it keeps the last few
  turns and replaces the rest with a one-line-per-action ledger — including which
  tools ran, because a file that was written stays written. It costs nothing, it
  works while the active tier is misbehaving, and it declines to act if the ledger
  would be larger than what it replaces. Because it changes the shape of the
  history, it also drops every tier's own session, so nothing continues a
  conversation that no longer exists.

Running low on context is not something you have to notice: when a tier is
abandoned and the session has grown past a few turns, the history is compacted as
part of spilling over, so the next tier starts from something small.

### Undoing a write

`/undo` puts back the file that the last approved write changed, using the bytes
that were already read to show you the diff before it happened. It reaches **one
write** and no further: after it, a second `/undo` says there is nothing to undo
rather than working backwards through the session.

```
> /undo
· restored src/a.rs (412 bytes, as it was before the write_file)

> /undo                        # the write had created the file
· removed src/a.rs and the 2 directories it created (created by the write_file)

> /undo                        # somebody edited it in the meantime
· src/a.rs has changed since that write — leaving it alone. Nothing was changed.
```

It refuses rather than guessing: if the file is not still exactly what the write
left — because you edited it, or another tool did, or it was deleted — it says so
and changes nothing. That is the whole safety story, and it is why `/undo` does not
ask for approval first: you typed it, and the thing it protects you from is not
yourself.

It pairs with spilling over, which is why it is here at all. A tier that wrote junk
and then looped is *abandoned* but its side effects are not undone — the next tier
inherits a workspace containing them. `/undo` is how you take that back:

```
you     refactor the parser
·       ✗ Looping Local repeated the same output 4 times — spilling over to DeepSeek
spill   Recovered on the second tier.
> /undo
· restored src/parser.rs (2,104 bytes, as it was before the write_file)
```

Two limits worth knowing: it reaches only the **last** write, so a model that
scattered junk across several files needs several rounds of spilling and undoing;
and it covers the file tools only. A `run_shell` command can do anything, so there
is no honest way to reverse one and `/undo` will not pretend otherwise.

## Status

Working today: configuration and validation, the terminal UI, OpenAI-compatible
streaming with model auto-discovery, the agent loop with seven tools and approval,
stuck detection and tier escalation, consulting a tier instead of escalating,
build and plan modes, slash commands, session continuity for CLI tiers, sessions
that survive a restart, `/undo`, per-class timeouts, cost reporting per tier,
delegated CLI tiers, the preset library, the setup wizard (which checks every tier
before it will write), zero-config first run, `doctor`, and `-p`.

The awkward paths are pinned by fixtures that drive the real provider, detector and
tools against a local endpoint instead of a live model — a model looping on the same
line, a consult that fails, a model stuck on a missing file, and a hunt for files
that are not there. A change that quietly breaks failover fails `cargo test` instead
of reaching you.

The tool set is deliberately closed at seven. No MCP client, no browser, no image
generation in 0.1.x: a tool has to earn its place, and every tool added is another
way for a small local model to get stuck.

Not yet:

- **The AUR package** is written but not submitted, so `yay -S spill-bin` does not
  work yet.
- Windows `winget` and `scoop` manifests.

## License

MIT — see [LICENSE](LICENSE).
