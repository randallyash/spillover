#!/bin/sh
# Check that a release tarball installs and runs.
#
# Run this against a published release before announcing it:
#
#   ./packaging/verify-install.sh 0.1.0
#
# It downloads the tarball for this machine's architecture, checks the binary
# runs and reports its version, and confirms the archive carried its licence.
# Deliberately does not install anything to the system: it works in a temporary
# directory so it is safe to run anywhere.

set -eu

version="${1:-}"
if [ -z "$version" ]; then
  echo "usage: $0 <version>    for example: $0 0.1.0" >&2
  exit 2
fi

case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
  *)
    echo "unsupported architecture: $(uname -m)" >&2
    exit 2
    ;;
esac

case "$(uname -s)" in
  Darwin) target="${target%-unknown-linux-gnu}-apple-darwin" ;;
  Linux) ;;
  *)
    echo "unsupported system: $(uname -s)" >&2
    exit 2
    ;;
esac

url="https://github.com/randallyash/spillover/releases/download/v${version}/spill-${target}.tar.xz"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Downloading $url"
if ! curl -fsSL "$url" -o "$work/spill.tar.xz"; then
  echo "FAIL: could not download the release tarball" >&2
  exit 1
fi

tar -xJf "$work/spill.tar.xz" -C "$work"

# cargo-dist wraps the contents in a directory named after the target, but the
# search does not assume that: it accepts either layout.
binary="$(find "$work" -name spill -type f | head -n 1)"
if [ -z "$binary" ]; then
  echo "FAIL: the tarball contains no spill binary" >&2
  exit 1
fi

chmod +x "$binary"
echo "Running the binary"
reported="$("$binary" --version)"
echo "  $reported"

case "$reported" in
  *"$version"*) ;;
  *)
    echo "FAIL: expected version $version, got: $reported" >&2
    exit 1
    ;;
esac

# The subcommands are what the README tells people to run, so they must exist.
for subcommand in presets doctor setup; do
  if ! "$binary" "$subcommand" --help >/dev/null 2>&1; then
    echo "FAIL: '$subcommand' is missing from the release binary" >&2
    exit 1
  fi
done

if [ -z "$(find "$work" -name LICENSE -type f)" ]; then
  echo "FAIL: the tarball contains no LICENSE" >&2
  exit 1
fi

# `spill doctor` must work without a terminal, since that is how it is used in a
# bug report. A non-zero exit is correct with nothing configured, so only the
# output is checked; in a pipeline the status comes from grep, not from doctor.
printf '' >"$work/empty.toml"
if ! "$binary" doctor --config "$work/empty.toml" 2>&1 | grep -q "doctor"; then
  echo "FAIL: 'spill doctor' did not run" >&2
  exit 1
fi

echo "OK: spill $version installs and runs on $target"
