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

## Install

**macOS and Linux:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/randallyash/spillover/releases/latest/download/spill-installer.sh | sh
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
| Prebuilt archives, five platforms | on the [releases page](https://github.com/randallyash/spillover/releases) |
| Homebrew | formula is published with each release, but not yet pushed to a tap, so `brew install` does not work |
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

Two things worth knowing:

- The abandoned tier's **conversation is discarded**, so the next tier never inherits
  a half-finished answer.
- Its **side effects are not**. If the tier that stalled already wrote a file or ran
  a command, that happened. Those results stay in the history so the next tier can
  see what was already done.

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

## Keyboard

`Enter` send · `Esc` or `Ctrl-C` quit · `PageUp` / `PageDown` scroll the transcript

Before anything that can change your files, a prompt appears showing exactly what it
will do. `y` runs it, `n` skips it — and the model is told it was declined, so it can
try another way rather than repeating itself.

## Status

Working today: configuration and validation, the terminal UI, OpenAI-compatible
streaming with model auto-discovery, the agent loop with seven tools and approval,
stuck detection and tier escalation, delegated CLI tiers, the preset library, the
setup wizard, zero-config first run, `doctor`, and `-p`.

Not yet:

- **Homebrew** needs a tap token before the formula can be published, so
  `brew install` is not available yet. The formula itself ships with each release.
- **The AUR package** is written but not submitted, so `yay -S spill-bin` does not
  work yet.
- Windows `winget` and `scoop` manifests.
- Per-tier CLI session resumption: a CLI fallback gets the whole conversation each
  turn rather than continuing its own session.
- `spill setup` offers hosted endpoints but does not yet validate a model id that was
  typed by hand rather than picked from the list.

## License

MIT — see [LICENSE](LICENSE).
