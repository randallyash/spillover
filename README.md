# spill

[![CI](https://github.com/randallyash/spillover/actions/workflows/ci.yml/badge.svg)](https://github.com/randallyash/spillover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/randallyash/spillover)](https://github.com/randallyash/spillover/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run your prompt on a local model first. When that model stalls, repeats itself, or
gets stuck in a loop, spill abandons it and re-runs the same turn on the next model
you listed — automatically, without you retyping anything.

```
tier 1  local model       free, private, no API key — tried first
tier 2  first fallback    reached only when tier 1 misbehaves
tier 3  last resort
```

## Why this is useful

Local models are free, private, and good enough for a lot of work, but they
occasionally hang or degenerate into repeating themselves. Most tools leave you to
notice that, cancel, and start over somewhere else. spill treats it as expected:

- **You keep using the free local model.** It is tier 1, so that is where your
  prompts go by default. The paid models below it are only reached when the local
  one actually fails.
- **A stuck model costs you nothing.** No watching for a loop, no cancel-and-retype.
  The turn is re-issued on the next tier by itself.
- **One interface, any model.** Tiers are generic: any OpenAI-compatible endpoint
  (LM Studio, Ollama, llama.cpp, vLLM, OpenRouter, xAI) or any agent CLI you already
  have installed and logged in (`cmd`, `grok`, `claude`, `codex`, `gemini`, and
  others). An existing CLI subscription works as a fallback with no separate API key.
- **It is an agent, not just a chat window.** It reads, searches, edits files and
  runs commands, and asks before anything that writes.

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
   models it serves, so you pick one rather than guessing at an id.
3. **Review** — each tier is checked and its resolved model named, so a wrong URL or
   a missing CLI is caught here rather than mid-conversation:

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
  over, or a run of tool calls that all fail;
- **goes quiet** — no response at all within `first_token_timeout_ms`, or silence
  past `idle_timeout_ms` once it has started. A slow tool call is not a stall: any
  real response resets that clock;
- **fails** — unreachable, an HTTP error, a broken stream;
- **uses up its step budget** without reaching an answer.

Thresholds are per tier, so an unhurried local model and a hosted one can be judged
differently. In the transcript you see exactly what happened and where it moved to:

```
you     explain the parser
·       ✗ Looping Local (http://127.0.0.1:8732/loop/v1) repeated the same output 4 times
          — spilling over to Healthy Fallback (http://127.0.0.1:8732/healthy/v1)
spill   Recovered on the second tier.
```

Three things worth knowing:

- The abandoned tier's **conversation is discarded**, so the next tier never inherits
  a half-finished answer.
- Its **side effects are not**. If the tier that stalled already wrote a file or ran
  a command, that happened. Those results stay in the history so the next tier can
  see what was already done.
- A `cli` tier's **session is dropped** too, so it starts a fresh conversation next
  time rather than resuming one that holds the answer that was thrown away.

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
FAIL  offline  openai  http://127.0.0.1:9/v1
               could not reach http://127.0.0.1:9/v1/models: could not connect  (0 ms)
ok    grok     cli     grok
               is installed  (0 ms)

2 of 3 tier(s) usable. spill will start on the first one that answers.
```

Every configured tier is checked, with what each would actually use, how long it
took, and why anything failed. `--json` produces the same report for a script, and
the exit code is non-zero when no tier is usable.

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

Below that is the conversation. The model's markdown is rendered rather than shown raw:
headings take weight, inline code is styled, a fenced block becomes a framed box with its
language on the top edge, bullets become dots, and a quote gets a bar down its side. A bar
down the left marks everything you wrote, so your own turns are findable in a long
session. Tool calls carry a glyph that says how they went — a spinner while one is
running, `✓` when it worked, `✗` when it did not, `!` for something worth noticing — and
the color agrees with the glyph rather than replacing it.

On a terminal 104 columns or wider, a panel on the right shows the chain as a row per tier
with its state, the working directory, the session's token and cache totals, and a
sparkline of per-turn spend, which is the only place the real cost of a fallback is
visible. On a narrower terminal the conversation takes that space and the active tier
moves down to the footer instead.

The prompt leads with `❯`, so the line you type on reads as a command line rather than as
text that happens to sit in a box.

## Keyboard

`Enter` send · `Shift+Tab` switch mode · `Esc` or `Ctrl-C` quit · `PageUp` / `PageDown` scroll

Before anything that can change your files, a prompt appears showing exactly what it
will do. `y` runs it, `n` skips it — and the model is told it was declined, so it can
try another way rather than repeating itself.

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

## Commands

Type `/` to open the menu of everything below. `?` shows them alongside every key.

The ones worth knowing are the ones about the chain, because they are what a
single-model agent cannot offer:

| Command | What it does |
| --- | --- |
| `/tier` | Show the chain, with each tier's number and state |
| `/tier <name\|number>` | Answer from that tier until told otherwise |
| `/tier auto` | Go back to the configured order |
| `/escalate` | Spill to the next tier now, without waiting for a stall |
| `/retry [tier]` | Send the last turn again, here or somewhere else |
| `/drop` | Discard the active tier's own conversation and start it fresh |
| `/sticky <on\|off>` | Whether a spill keeps the lower tier for the session |
| `/cost` | Tokens and cache reads so far, tier by tier |
| `/context` | What actually gets sent on each turn, and how large it has grown |
| `/compact` | Fold earlier turns into a short ledger to shrink what is sent |
| `/clear` | Start a new conversation, keeping the tiers as they are |
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

## Status

Working today: configuration and validation, the terminal UI, OpenAI-compatible
streaming with model auto-discovery, the agent loop with seven tools and approval,
stuck detection and tier escalation, delegated CLI tiers, the preset library, the
setup wizard, zero-config first run, `doctor`, and `-p`.

Not yet:

- **The AUR package** is written but not submitted, so `yay -S spill-bin` does not
  work yet.
- Windows `winget` and `scoop` manifests.
- `spill setup` offers hosted endpoints but does not yet validate a model id that was
  typed by hand rather than picked from the list.

## License

MIT — see [LICENSE](LICENSE).
