# Security

## Reporting a vulnerability

Open a [private security advisory](https://github.com/randallyash/spillover/security/advisories/new)
rather than a public issue. Please include what you did, what happened, and what you
expected.

## What this program does, and why it matters

`spill` is an agent harness, so it has a wider blast radius than a typical CLI. Worth
knowing before you run it:

- **It can modify your files and run commands.** File writes and shell commands are
  confirmed in the UI by default. Turning that off, or letting a tier run with
  `approve_all = true`, means the model acts without a prompt.
- **Delegated CLI tiers run under their own permission system.** When a `cli` tier is
  configured with `approve_all = true`, `spill` passes the tool's own
  skip-permissions flag (`--yolo`, `--always-approve`). From that point the child
  process decides what to do, not `spill`'s approval prompt. That is a
  deliberate trade: it is the only way those tiers can edit files at all.
- **Your prompts and files go to whichever tier answers.** A local tier keeps
  everything on your machine; a hosted tier does not. Fallback means a prompt you
  pointed at a local model may be re-sent to a hosted provider.
- **It reads credentials it does not own.** For delegated CLI tiers it relies on
  the CLI's existing login. `spill` does not read, copy, or refresh those tokens
  itself.
- **Configuration never contains secrets.** API keys are referenced by the name of an
  environment variable. A malformed or malicious config file cannot exfiltrate a key
  by reading it, because the key is never written to disk.

## Supported versions

Pre-release. Fixes land on `main`; there are no maintained release branches yet.
