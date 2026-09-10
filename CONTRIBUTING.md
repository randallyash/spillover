# Contributing

## Development setup

Requires Rust 1.85 or newer (edition 2024).

```sh
git clone https://github.com/randallyash/spillover
cd spillover
cargo build
```

If `cargo` is not found after installing rustup, add the shims to your `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## Before opening a pull request

CI runs exactly these, so run them locally first:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

If you touched `dist-workspace.toml` or the release workflow, check they still
agree — cargo-dist generates one from the other:

```sh
dist generate --check
```

## Cutting a release

See [packaging/README.md](packaging/README.md). In short: bump the version, tag
it, and the release workflow does the rest.

## Running the app

```sh
cargo run                     # uses ~/.config/spill/config.toml
cargo run -- --config ./my.toml
```

Run it in a real terminal — it needs a TTY to enter raw mode.

## Conventions

- Every module that can be tested without a terminal should be. Wrap your own
  break/pack logic rather than relying on a widget's internals, and unit-test it.
- Errors must be actionable. Say what is wrong, where, and what to do about it.
- Never write a secret to disk. Configuration refers to keys by environment
  variable name; that rule exists so the repository can never leak one.
- Comments explain *why*, not *what*. If a line is surprising, say why it has to be
  that way.

## Adding a provider preset

Presets live in `src/preset/presets.toml`. A new CLI preset needs:

1. The exact invocation that answers one prompt and exits, verified against the
   tool's own `--help` output.
2. A `dialect` if the tool emits structured events, and a recorded fixture under
   `tests/fixtures/`. If no dialect fits, leave it as `plain` — that guarantees the
   tier still works, just without structured tool events.
3. A note in the README's provider table.

## Reporting bugs

Include your OS, terminal, `spill doctor` output, and the relevant part of your
config with secrets removed.
