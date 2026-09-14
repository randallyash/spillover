# Packaging

Release artifacts are produced by [cargo-dist](https://opensource.axo.dev/cargo-dist/),
configured in `dist-workspace.toml`.

## Generated, not hand-written

Do not edit these; change the source and let cargo-dist regenerate them, or
`dist generate --check` will fail in CI:

| File | Generated from |
| --- | --- |
| `.github/workflows/release.yml` | `dist-workspace.toml` |
| `wix/main.wxs` | `Cargo.toml` (the `authors` field is the MSI publisher) |

Run `dist generate` after changing either source. Everything else in this
directory is written by hand.

## Cutting a release

1. Bump `version` in `Cargo.toml` and move the changelog's `Unreleased` entries
   under the new version.
2. Commit, then tag and push:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. `.github/workflows/release.yml` runs on the tag. It builds every target,
   generates the installers, and attaches everything to a GitHub release.

To see what a release *would* contain without tagging anything:

```sh
dist plan
```

To check that the workflow matches `dist-workspace.toml` after changing one of
them:

```sh
dist generate --check
```

## What gets published

| Artifact | For |
| --- | --- |
| `spill-x86_64-unknown-linux-gnu.tar.xz` | Linux, x86-64 |
| `spill-aarch64-unknown-linux-gnu.tar.xz` | Linux, ARM64 |
| `spill-x86_64-apple-darwin.tar.xz` | macOS, Intel |
| `spill-aarch64-apple-darwin.tar.xz` | macOS, Apple silicon |
| `spill-x86_64-pc-windows-msvc.zip` | Windows, portable |
| `spill-x86_64-pc-windows-msvc.msi` | Windows, installer |
| `spill-installer.sh` | the `curl \| sh` installer |
| `spill-installer.ps1` | the PowerShell installer |
| `spill.rb` | the Homebrew formula |
| `sha256.sum` | checksums for all of the above |

Each archive carries the binary plus `LICENSE`, `README.md` and `CHANGELOG.md`.

## Homebrew tap

The formula is published to
[randallyash/homebrew-spillover](https://github.com/randallyash/homebrew-spillover),
which is what makes `brew install randallyash/spillover/spill` work.

The tap is set up and working: v0.1.2's formula is published, with checksums that
match the release archives. `packaging.yml` installs from it on a clean macOS
runner, which is the check that keeps that true rather than a claim about it. The
steps below are what it took, kept for renewal when the token expires.

1. Create a fine-grained personal access token at
   <https://github.com/settings/personal-access-tokens/new>:
   - **Repository access** → *Only select repositories* → `homebrew-spillover`
   - **Permissions** → *Repository permissions* → **Contents: Read and write**
   - **Expiration** → one year

   One year is the maximum for a fine-grained token, and releases are
   infrequent, so anything shorter tends to lapse between them and fails a
   release for a reason nobody remembers. GitHub emails before it expires.

   Keep the scope as narrow as above. This token can write to a repository whose
   contents people `brew install`, so a leaked one could publish a malicious
   formula. That is also why it should not be set to never expire.

2. Store it as a secret on the main repository:

   ```sh
   gh secret set HOMEBREW_TAP_TOKEN --repo randallyash/spillover
   ```

   `gh` prompts for the value, so it never lands in your shell history.

When the token expires, renew it by repeating both steps. Nothing in the
repository changes, because the secret is referenced by name only.

Until it is set, the `publish-homebrew-formula` job fails, and because the
`announce` job waits on it the run ends red. The release artifacts still
publish — `host` runs first — so the installers keep working, but it looks
broken.

To re-run just that job after setting the secret, without rebuilding anything:

```sh
gh run rerun <run-id> --failed --repo randallyash/spillover
```

To turn the tap off entirely, drop the last two lines of `dist-workspace.toml`
and run `dist generate`.

## Not yet supported

Everything here comes **after 0.1.1, not instead of it**. A release with fewer
installers is still a release, and a package manager is only worth submitting once
the thing it installs has settled — so another version ships first, and these
follow it. None of them is a reason to hold up a release.

- **winget** and **scoop** are how most Windows users install CLI tools, but
  neither is something cargo-dist can generate. Each needs a manifest submitted
  to `microsoft/winget-pkgs` and `ScoopInstaller/Main` respectively, which is a
  pull request per release, so the format is worth getting settled first.
- **The AUR submission** is written, builds, and carries checksums that match the
  0.1.2 release, but it is not sent: registration at the AUR was closed when the
  recipe became ready, and an account is the one thing that cannot be made from
  here. The key exists, `ssh` is pointed at it, and the repository is committed
  and staged, so what is left is a browser step and one push. See [Arch](#arch).
- **Homebrew core** would remove the need for a tap, but needs a project to be
  reasonably established first.
- **`.deb` and `.rpm`** are not generated by cargo-dist. The `curl | sh`
  installer covers Debian and Fedora.

## Arch

`arch/PKGBUILD` installs the released Linux binary. It builds — `makepkg -f` in
`arch/` produces `spill-bin-<ver>-1-x86_64.pkg.tar.zst` — and the checksums in it
are the real published ones. The package is named `spill-bin` because it installs
a prebuilt binary, and `-bin` is the AUR's convention for that; the crate is
`spill`.

Its `pkgver` has to name the release it downloads, which is the one thing here
that goes stale silently: it sat at 0.1.1 through the 0.1.2 release. A test now
holds it to `Cargo.toml`, along with the changelog's newest release, so a bump
that misses this file fails `cargo test` instead of shipping a package that
downloads one version and calls itself another.

Two `options` are set, and both are about shipping the released artifact
unaltered: `!strip` keeps the symbol table cargo-dist published, so panic
backtraces stay useful, and `!debug` stops makepkg creating a debug package for a
recipe that holds no source. Without the second one the package carries an empty
`/usr/src/debug/spill-bin` directory.

**It is not in the AUR yet, so `yay -S spill-bin` does not work.** The recipe,
its checksums and its `.SRCINFO` are current for 0.1.2, and the push is one
command, but registering the account that authorises it was closed when this was
ready — so it is parked until that reopens. Nothing else waits on it: `makepkg`
builds the package here today, and the release is unaffected.

### Submitting, once

1. Register at <https://aur.archlinux.org/register/> and confirm the email.
2. Add a public key under *My Account* → *SSH Public Key*:

   ```sh
   ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_aur -C "aur"
   cat ~/.ssh/id_ed25519_aur.pub
   ```

3. Teach `ssh` to use that key for the AUR. The name is not one of the defaults
   `ssh` tries, so without this it offers the wrong keys and the AUR answers
   `Permission denied (publickey)` even though the key is registered:

   ```
   Host aur.archlinux.org
     HostName aur.archlinux.org
     User aur
     IdentityFile ~/.ssh/id_ed25519_aur
     IdentitiesOnly yes
   ```

   Then confirm the AUR accepts it. When the key is registered this answers with
   a welcome naming your account; when it is not you get `Permission denied
   (publickey)`, which is worth knowing before blaming the push:

   ```sh
   ssh -T aur@aur.archlinux.org
   ```

4. Push the recipe, from a directory **outside this working tree**. `git init`
   inside `arch/` nests a repository inside this one, and every command here then
   warns about an embedded repository it does not know what to do with:

   ```sh
   aur=~/Projects/spillover-aur
   mkdir -p "$aur"
   cp arch/PKGBUILD arch/.SRCINFO arch/.gitignore "$aur/"
   cd "$aur"
   git init -b master
   git add -A
   git commit -m "Initial import: spill-bin <ver>-1"
   git remote add aur ssh://aur@aur.archlinux.org/spill-bin.git
   git push aur master     # master, not main: the AUR serves the former
   ```

That repository holds the recipe only — `PKGBUILD`, `.SRCINFO` and `.gitignore`,
which is what a real `-bin` package there tracks. No tarball and no binary: the
release archive is downloaded from GitHub when a user builds.

Both `spill-bin` and `spill` were unclaimed when this was written, so the name did
not have to change.

### Each release

The AUR version has to keep up or the package goes stale and gets flagged:

```sh
cd arch
# bump pkgver, then:
updpkgsums                        # rewrites sha256sums from the new release
makepkg --printsrcinfo > .SRCINFO
git commit -am "upgpkg: spill-bin <ver>-1"
git push aur master
```

`git push` is what publishes; there is no web form. A pushed checksum is live
immediately, and a wrong one means `yay -S spill-bin` fails to build for everyone
until it is fixed — so build it before pushing.

`namcap PKGBUILD` lints the recipe, and `namcap spill-bin-<ver>-1-x86_64.pkg.tar.zst`
the package. Four warnings are expected and none is a defect: the arch literal it
wants rewritten as `$CARCH` is in the upstream download URL, which a `-bin`
package cannot choose; the binary is deliberately unstripped; and the two
dependency notes are about `gcc-libs` providing `libgcc_s.so.1`, which the binary
needs. Anything else namcap says is new.

One trap worth knowing before editing the recipe: **do not name a local variable
inside `package()` after a makepkg variable.** `makepkg --printsrcinfo` picks up
an assignment to one — a local called `changelog` ends up in `.SRCINFO` as the
`changelog` field, holding whatever path it was given, and the AUR publishes it.

## Verifying a release

`verify-install.sh` downloads the tarball for this machine, checks the binary
runs, reports the right version, and confirms the subcommands and licence are
present:

```sh
./packaging/verify-install.sh 0.1.2
```

It installs nothing system-wide.

`.github/workflows/packaging.yml` runs it after a release, and installs from the
Homebrew tap on a clean macOS runner to check that the installed binary reports
the version it was released as. It is not part of CI — it reaches the network for
a published artifact rather than building a commit — so it is dispatched by hand,
naming the version or taking the latest release:

```sh
gh workflow run packaging.yml -f version=0.1.2
```
