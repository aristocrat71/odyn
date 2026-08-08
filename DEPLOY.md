# Deploying Odyn

Releases are built by GitHub Actions and published to **GitHub Releases**.
Installed copies then **auto-update** from there, so cutting a release is the only
distribution step.

The pipeline is **tag-triggered**: pushing a tag matching `v*` builds an Apple
Silicon macOS app, publishes a Release, and writes the `latest.json` the in-app
updater reads. Nothing else (branch pushes, PRs, the Actions UI) can start it.

**macOS on arm64 is the only target, and Intel is not a matrix entry away.**
`ort-sys` — the onnxruntime binding `fastembed` uses for the brain's bundled
embedder — publishes no prebuilt binary for `x86_64-apple-darwin`, so both an
Intel build and a universal one fail at link time with `no prebuilt binaries
available for target x86_64-apple-darwin`. Shipping Intel means building
onnxruntime from source or dropping the bundled embedder; neither is a workflow
change. Linux and Windows _are_ just a matrix entry (plus the apt packages from
the README on the Linux runner) and the matching branches in `install.sh`.

---

## One-time setup (before the first release)

### The signing key

The updater signs each build so clients can verify that an update really came
from you. This is **update-integrity signing, not Apple code-signing** — it has
nothing to do with notarization or Gatekeeper.

The keypair already exists at `~/.tauri/odyn-updater.key`. It has **no password**
— nothing in this pipeline ever asks you for one. It was generated with:

```sh
bun tauri signer generate -w ~/.tauri/odyn-updater.key --ci
```

| File                            | What                                                                       |
| ------------------------------- | -------------------------------------------------------------------------- |
| `~/.tauri/odyn-updater.key.pub` | public key — safe to commit                                                |
| `~/.tauri/odyn-updater.key`     | **private key — never commit, never paste into a chat, never print in CI** |

**Public key → the config.** Already done: the one-line contents of the `.pub`
file live in `crates/odyn-app/tauri.conf.json` at `plugins.updater.pubkey` (the
release workflow refuses to build if it's ever emptied). Don't change it, or
existing installs won't be able to verify new updates. To re-derive it:

```sh
cat ~/.tauri/odyn-updater.key.pub
```

**Private key → the GitHub secret.** This is the one step still outstanding.
Under **Settings → Secrets and variables → Actions → New repository secret**,
create `TAURI_SIGNING_PRIVATE_KEY` and paste the private key's contents. Pipe it
to the clipboard rather than to the screen:

```sh
pbcopy < ~/.tauri/odyn-updater.key
```

That's the **only** secret you need. Do **not** create
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: GitHub rejects empty secret values, and a
passwordless key doesn't need one. The workflow still references it, but with the
secret absent that reference resolves to an empty string — which is exactly what
this key wants, and why CI never stops to ask.

If the private key is lost, updates break until you generate a new keypair _and_
every user reinstalls. Back it up somewhere you'd trust with a password.

**This changes local release builds.** With `createUpdaterArtifacts` on, bundling
signs the updater artifact, so a bare `bun tauri build` now ends with _"A public
key has been found, but no private key"_ — after writing the `.app` and `.dmg`,
so they're still usable, but the command exits non-zero. Point it at the key
file (this variable takes a path, so the key stays off your command line):

```sh
TAURI_SIGNING_PRIVATE_KEY=~/.tauri/odyn-updater.key \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD= \
  bun tauri build --target aarch64-apple-darwin
```

The second variable is not a password — it's an empty value that says _there is
no password_. Leave it out entirely and the CLI stops after bundling to prompt
for one on the terminal, which is what `incorrect updater private key password:
Device not configured` means when it appears in a script or a CI log.

`bun tauri dev` is unaffected — it doesn't bundle.

That's the whole setup. There are deliberately **no repo rulesets, protected
branches, or deployment environments** to configure — pushing a `v*` tag is all a
release takes.

If this app ever grows past personal use, the one thing worth adding back is a
manual approval before the signing key reaches a runner: put `environment:
release` on the `release` job, then under **Settings → Environments → release**
set **Required reviewers** _and_ set **Deployment branches and tags** to
**Selected branches and tags** with a rule of type **Tag**, pattern `v*`. That
second part is not the default and not optional — a tag is not a branch, so
leaving it on "Protected branches only" fails every release with `Tag 'v0.1.0'
is not allowed to deploy to release due to environment protection rules`.

---

## Cutting a release

### 1. Get the changes onto `main`

The release **must** be built from `main` — a guard job refuses to release a tag
whose commit isn't on it. `main` isn't protected, so a plain `git push` is enough;
the check exists to stop a tag pushed from a feature branch shipping un-merged
code.

### 2. Bump the version and write the changelog

Add a `## [X.Y.Z]` section to `CHANGELOG.md`. **This is required** — the workflow
extracts the section matching the tag and uses it as the release body, and fails
the release outright if there isn't one. That body is also what `tauri-action`
writes into `latest.json` as `notes`, so it's the text every installed copy of
Odyn shows on its next update check. Write it for users, not for the commit log.
The heading's date is never published, so `## [0.2.0] - Unreleased` is fine to
merge and tidy up later.

The updater compares the **semver in `crates/odyn-app/tauri.conf.json`** against
what's installed, so every release needs a higher version. Bump it in **all
three** files — the guard job fails the release if they disagree with each other
or with the tag:

| File                             | Field                |
| -------------------------------- | -------------------- |
| `crates/odyn-app/tauri.conf.json` | `"version": "0.2.0"` |
| `Cargo.toml`                     | `version = "0.2.0"` under `[workspace.package]` — every crate inherits it |
| `package.json`                   | `"version": "0.2.0"` |

There's nothing to bump for the install one-liner. It tracks `main` rather than a
tag, so the published command is identical every release and the script resolves
`releases/latest` at run time.

If you ever change that command, it lives in three places that must stay
byte-identical: `README.md`, the header comment in `install.sh`, and the
release-notes snippet in `.github/workflows/release.yml`.

Then sync `Cargo.lock` (it records each crate's version) and land it:

```sh
cargo check --workspace   # updates Cargo.lock
git add Cargo.toml Cargo.lock crates/odyn-app/tauri.conf.json package.json CHANGELOG.md
git commit -m "release: v0.2.0"
git push origin main
```

### 3. Tag and push

Tag the bump commit **after** it's on `main`, then push the tag. **This is what
triggers the build.**

```sh
git tag v0.2.0
git push origin v0.2.0
```

### 4. Watch the build

Open the repo's **Actions** tab. The run has two stages, and no approval step —
pushing the tag is the last thing you have to do:

1. **guard** — read-only, no secrets, fails in seconds if the tag isn't on
   `main`, the three versions disagree, the updater pubkey is missing, the
   signing-key secret is missing, or `CHANGELOG.md` has no matching section.
2. **release** — the macOS build. Expect 10–20 minutes on a cold cache: the tree
   carries `onnxruntime` (via `fastembed`), plus a bundled SQLite and
   `sqlite-vec`.

When it's green the release page has `odyn_X.Y.Z_aarch64.dmg`, its `.sha256`, the
updater bundle + `.sig`, and `latest.json`.

### 5. Verify

```sh
ODYN_VERSION=v0.2.0 bash <(curl -fsSL --connect-timeout 10 https://raw.githubusercontent.com/aristocrat71/odyn/main/install.sh)
gh attestation verify ~/Downloads/odyn_0.2.0_aarch64.dmg --repo aristocrat71/odyn
```

---

## How the update reaches an installed copy

The tray has one item for this — **Check for updates…** — and it is the whole
interface. Odyn checks once at launch and stays quiet if there is nothing; if
there is, the item relabels itself through downloading and lands on **Restart to
finish updating**. Clicking the item at any other time runs the same check by
hand, and then it always answers, because silence reads as broken.

An installed copy only accepts a bundle whose signature verifies against the
public key baked into it at build time, so a tampered release — or one signed
with a different key — is refused rather than installed.

## How the private key is kept out of the logs

The user-facing requirement is that `TAURI_SIGNING_PRIVATE_KEY` never appears
anywhere in a run's output. What enforces that:

- **One step sees it.** The secret is set in the `env:` of the single
  `tauri-action` step, never at job level. No other step in the job — including
  anything that dumps the environment — can read it.
- **Nothing echoes it.** No step prints the variable, interpolates it into a
  command line (where `ps` on the runner could see it), or runs under `set -x`.
- **The presence check never touches it.** The guard job verifies the secret
  exists by passing `${{ secrets.TAURI_SIGNING_PRIVATE_KEY != '' }}` — the runner
  evaluates the comparison, so all that reaches the shell is the string `true` or
  `false`. The key is never assigned to a variable in that job at all.
- **Only tag pushes reach it.** The workflow has exactly one trigger, `push` on
  `v*`. Nothing on a branch, a PR from a fork, or the Actions UI can start a run
  that reads the secret — and only repo collaborators can push a tag.
- **Actions are pinned to commit SHAs**, not movable tags, so a hijacked or
  repointed tag in a third-party action can't start reading the environment.
- **Least-privilege tokens.** The workflow is `contents: read` by default; only
  the release job widens that.

If you ever need to debug this workflow, **do not enable step debug logging**
(`ACTIONS_STEP_DEBUG`) on it. GitHub masks known secret values in logs, but
masking is a backstop, not the control.

If the key is ever exposed, treat it as burned: generate a new keypair, update
`pubkey` in `crates/odyn-app/tauri.conf.json`, replace the secret, and ship a
release — existing installs will need a manual reinstall to pick up the new
public key.

---

## Gatekeeper and notarization

Odyn isn't signed with an Apple Developer ID or notarized, so a `.dmg` downloaded
by hand shows _"odyn cannot be opened because the developer cannot be verified"_.
`install.sh` handles this: it verifies the download against the published SHA-256
first and then clears the `com.apple.quarantine` flag, which is the flag that
triggers the prompt. Manual installs need a one-time **right-click → Open**, or:

```sh
xattr -dr com.apple.quarantine /Applications/odyn.app
```

Auto-updates are unaffected — the updater downloads and extracts the new bundle
itself, so nothing ever gets quarantined.

Notarizing would remove all of this. It needs a paid Apple Developer account plus
four more secrets (`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`) added
to the same `tauri-action` step.

---

## Redoing a release

If a release is wrong, **cut a new patch version** rather than moving the tag.
Installs that already fetched `latest.json` will have cached the old one, and a
moved tag makes the published `.sha256` and the attestation disagree with what's
actually on the release page. If you must, delete the release and its tag
(`gh release delete v0.2.0 --cleanup-tag`) before anyone installs it, then
re-tag.
