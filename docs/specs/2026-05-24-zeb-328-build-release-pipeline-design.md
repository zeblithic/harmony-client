# ZEB-328 Sub-project A: Build & Release Pipeline — Design

**Status:** Draft (settled in brainstorm 2026-05-24, awaiting user spec review).
**Parent:** [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) — *harmony-client v0.1.0-alpha: distribution + onboarding + validation.*
**Sibling sub-projects (sequenced):** B (validation surface), C (onboarding docs + first-run UX), D (bootstrap Zeblithic community).
**Author:** zeblith + Claude, 2026-05-24.

## 1. Goal

Produce v0.1.0-alpha artifacts that hand-picked external testers can download, install, and **auto-update** on macOS / Windows / Linux desktop. Unblocks Sub-projects B/C/D.

The shape we're aiming for: zeblith bumps a version field, triggers a workflow, and ~25 minutes later there's a GitHub Release with six artifacts plus a fresh update manifest. Testers running an older version see a toast on next launch, click "Restart," and they're on the new version.

## 2. Non-goals

- **Code signing / notarization** (Apple Developer cert, EV Windows cert). Documented as a public-beta concern, not alpha. Recurring cost we're not paying yet.
- **Mobile builds** (iOS / Android). Different toolchain, different signing story, deferred to later phase of broader product story.
- **Public beta / open registration.** Alpha is invite-only; tester onboarding goes through Sub-project D.
- **Crash telemetry / phone-home.** Alpha relies on tester feedback channel (Sub-project D), not in-app telemetry.
- **Multi-region CDN for update manifest.** GitHub Pages is fine for alpha scale. Cutover to `q8.fyi` or similar is a future operational concern.
- **sccache / build-time optimization** beyond Tauri defaults.
- **Reproducible builds in the strict sense** (bit-identical across runners). We pin tool versions (Rust via `rust-toolchain.toml`, Node via `engines`, Tauri CLI via `package.json`); we don't pursue byte-equality.

## 3. Architecture overview

```text
┌─────────────────┐   workflow_dispatch     ┌──────────────────────────────────┐
│  Operator       │ ────────────────────►   │  .github/workflows/release.yml   │
│  (zeblith)      │   version=0.1.0-alpha.N │                                  │
└─────────────────┘                         │  ① ci.yml passing check (gate)  │
                                            │  ② matrix:                       │
                                            │     - macos-14   (ARM build)     │
                                            │     - macos-13   (Intel build)   │
                                            │     - windows-latest (MSI build) │
                                            │     - ubuntu-22.04  (AppImage)   │
                                            │  ③ release job:                  │
                                            │     - create GitHub Release v…   │
                                            │     - attach 4 installers        │
                                            │     - attach Tauri update bundles│
                                            │     - regenerate gh-pages/       │
                                            │       latest.json                │
                                            └──────────────────────────────────┘
                                                          │
                                                          ▼
                                            ┌──────────────────────────────────┐
                                            │  github.com/zeblithic/           │
                                            │  harmony-client (releases)       │
                                            │                                  │
                                            │  + GitHub Pages:                 │
                                            │  https://zeblithic.github.io/    │
                                            │  harmony-client/latest.json      │
                                            └──────────────────────────────────┘
                                                          │
                              ┌───────────────────────────┴───────────────────┐
                              ▼                                               ▼
                  ┌──────────────────────┐                       ┌──────────────────────┐
                  │  Tester (download)   │                       │  Tester (existing    │
                  │  GET .dmg/.msi/      │                       │  install, on launch) │
                  │  .AppImage from      │                       │  GET latest.json     │
                  │  Releases            │                       │  → compare version   │
                  └──────────────────────┘                       │  → toast if newer    │
                                                                 │  → click Restart →   │
                                                                 │    download .sig +   │
                                                                 │    .tar.gz bundle,   │
                                                                 │    verify, apply     │
                                                                 └──────────────────────┘
```

Six logical components, each owned by a numbered section below:

1. **Release artifacts manifest** — what files exist per release and why.
2. **Build infrastructure** — the GitHub Actions workflow file.
3. **Auto-updater** — Tauri plugin + key management + hosted manifest.
4. **Versioning + release notes** — how versions get bumped and how notes get drafted.
5. **Identity portability invariants** — what must NOT change between versions.
6. **Documentation deliverables** — three install docs + one release-process doc.

## 4. Release artifacts manifest

Per release, the workflow produces these files and attaches them to a GitHub Release tagged `v{version}`:

| Artifact | Platform | Purpose |
|---|---|---|
| `Harmony_{version}_aarch64.dmg` | macOS Apple Silicon | First-time install |
| `Harmony_{version}_x64.dmg` | macOS Intel | First-time install |
| `Harmony_{version}_x64_en-US.msi` | Windows x64 | First-time install |
| `harmony_{version}_amd64.AppImage` | Linux x64 | First-time install (chmod +x and run) |
| `Harmony.app.tar.gz` + `Harmony.app.tar.gz.sig` | macOS ARM + Intel (one of each) | Tauri updater bundle |
| `Harmony_{version}_x64-setup.nsis.zip` + `.sig` | Windows | Tauri updater bundle |
| `harmony_{version}_amd64.AppImage.tar.gz` + `.sig` | Linux | Tauri updater bundle |

**Naming follows Tauri 2 defaults** so we don't have to override the bundler.

**Why two macOS .dmgs (not Universal):** Universal binaries double download size; Apple Silicon split is well-understood by users and Tauri's tooling produces them cleanly when you build on the matching runner. We build ARM on `macos-14` and Intel on `macos-13`. (`macos-13` is the last GA Intel runner — when GitHub deprecates it we'll need to revisit; tracking as a risk.)

**Linux: AppImage only for v0.1.0.** No `.deb` / `.rpm` / `.flatpak` — adds packaging complexity, and AppImage works everywhere. We can add platform-specific packages later if a tester strongly prefers their distro's native format.

**All installer artifacts are OS-unsigned** (no Apple notarization, no Windows code-signing cert) — per-OS install docs (§9) cover the Gatekeeper / SmartScreen workarounds. **The Tauri updater bundles ARE signed** via Tauri's internal ed25519 mechanism (see §6.3) — the `.sig` file alongside each `.tar.gz` is that signature, verified by installed clients before auto-applying. The two signing layers are independent: OS signing establishes initial-install trust; Tauri signing protects the update channel post-install. Total artifact set: ~10 files, well within GitHub Release's 2GB-per-asset limit (per-asset is ~50-100MB).

## 5. Build infrastructure

New file: `.github/workflows/release.yml`. Single-trigger, matrix-built, fan-in-released.

### 5.1 Trigger

`workflow_dispatch` only. One input:

```yaml
inputs:
  version:
    description: 'SemVer + pre-release (e.g., 0.1.0-alpha.1)'
    required: true
    type: string
```

No tag-driven trigger — keeps the surface area minimal and prevents accidental releases from random pushes. The release workflow creates the tag itself as part of the Release.

### 5.2 Precondition gate

Before any build, a `precheck` job:

1. Verifies the input version matches the `version` field in `tauri.conf.json` (fail if mismatch — forces operator to bump and commit before triggering).
2. Runs all five quality gates inline (per CLAUDE.md): `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`. Fails the workflow if any gate fails. **We deliberately do NOT re-enable the standalone `ci.yml` workflow** — per the project's `feedback_ci_disabled` HARD RULE, PR-time CI stays off (AI bot reviews cover that surface). Gates run here only at release-trigger time, so the no-CI PR experience is preserved while releases are still gated on green.
3. Verifies `gh release view v{version}` returns 404 (fail if a release with that tag already exists — prevents accidental clobber).

### 5.3 Matrix builds

```yaml
strategy:
  fail-fast: false
  matrix:
    include:
      - os: macos-14
        target: aarch64-apple-darwin
        artifact: Harmony_{version}_aarch64.dmg
      - os: macos-13
        target: x86_64-apple-darwin
        artifact: Harmony_{version}_x64.dmg
      - os: windows-latest
        target: x86_64-pc-windows-msvc
        artifact: Harmony_{version}_x64_en-US.msi
      - os: ubuntu-22.04
        target: x86_64-unknown-linux-gnu
        artifact: harmony_{version}_amd64.AppImage
```

Each matrix job:

1. Checkout
2. Install Rust toolchain (pinned via `rust-toolchain.toml` — to-be-added if not present)
3. Install Node (pinned via `engines.node` in `package.json`)
4. `npm ci`
5. `npm run tauri build -- --target {target}`
6. Upload the installer + the updater bundle (`.tar.gz` + `.sig`) as workflow artifacts

`fail-fast: false` so a Windows failure doesn't abort macOS — we want to see all three results.

**Ubuntu pinned to `22.04`** (not `latest`) for AppImage compatibility. AppImages built on newer glibc don't run on older systems — `22.04`'s glibc is wide-enough for our tester base. Re-evaluate every ~6 months.

### 5.4 Release fan-in

After all matrix builds succeed, a `release` job:

1. Download all platform artifacts
2. Create the GitHub Release: `gh release create v{version} --generate-notes --draft`
3. Attach all artifacts
4. Update `latest.json` and push to `gh-pages` branch (see §6)
5. Publish the draft (`gh release edit --draft=false`)

Total wall-clock target: **~25 minutes**. Matrix builds run in parallel; longest pole is macOS (Rust compile + DMG assembly).

## 6. Auto-updater architecture

### 6.1 Tauri 2 updater plugin

Add `tauri-plugin-updater` (Rust dep) and `@tauri-apps/plugin-updater` (JS dep) — pin both to the latest minor version compatible with the pinned Tauri 2.10.x at implementation time. (Tauri minors can shift the bundle format; use `cargo add tauri-plugin-updater@^2` and the matching `npm install @tauri-apps/plugin-updater@^2` then verify against `https://v2.tauri.app/plugin/updater/` for the exact tested combination.) Configure in `tauri.conf.json`:

```json
{
  "plugins": {
    "updater": {
      "active": true,
      "endpoints": [
        "https://zeblithic.github.io/harmony-client/latest.json"
      ],
      "pubkey": "<ed25519 verifying key — embedded at build time>"
    }
  }
}
```

The `pubkey` is the operator-held verifying half of an ed25519 keypair. The signing half lives only as a GitHub Actions secret + a sealed-text backup.

### 6.2 Update manifest format

GitHub Pages serves `latest.json` (Tauri's expected schema):

```json
{
  "version": "0.1.0-alpha.2",
  "notes": "See https://github.com/zeblithic/harmony-client/releases/tag/v0.1.0-alpha.2",
  "pub_date": "2026-05-25T12:00:00Z",
  "platforms": {
    "darwin-aarch64": {
      "signature": "<base64 ed25519 sig>",
      "url": "https://github.com/zeblithic/harmony-client/releases/download/v0.1.0-alpha.2/Harmony.app.tar.gz"
    },
    "darwin-x86_64": { … },
    "windows-x86_64": { … },
    "linux-x86_64": { … }
  }
}
```

The release workflow's last step regenerates this file from the matrix outputs and force-pushes to `gh-pages`. GitHub Pages auto-publishes within ~30s.

### 6.3 Signing key custody

**Generation:** one-time, via `tauri signer generate -w ~/.tauri/harmony-updater.key`. Output is two files: a private key (`.key`) and a public key (`.key.pub`).

**Custody:**

1. Private key contents stored as a GitHub Actions repository secret named `TAURI_SIGNING_PRIVATE_KEY`.
2. The plain-text passphrase that protects the private key (Tauri uses a passphrase-protected key) stored as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
3. **Two backups** of both:
   - Sealed text in zeblith's primary password manager (1Password vault).
   - Sealed text printout in a physical lockbox or equivalent offline location.

**Threat model:** if the private key leaks, an attacker can sign malicious updates that installed clients will accept and auto-apply. Mitigations:
- Endpoint URL is HTTPS-only (GitHub Pages enforces TLS).
- Tauri verifies the update signature against the embedded `pubkey` BEFORE applying — even a compromised endpoint can't push unsigned updates.
- If a leak is suspected: rotate the keypair, ship a v0.1.0-alpha.N+1 release with the new `pubkey` embedded, and accept that existing installed clients (signed with the old key) need manual re-install to migrate. This is a recovery story but a painful one — treat the key with care.

**If the private key is LOST** (not leaked, just lost — e.g., backups all destroyed): existing installed clients are permanently stranded on their current version, can't auto-update. Recovery requires shipping a new keypair and manual re-install for all testers. Document this as a Severe operational risk in `docs/release-process.md`.

### 6.4 In-app UX

Per the brainstorm decision: **silent check on launch, toast on available, user-initiated apply.**

1. App starts → background async task fetches `latest.json` (1 HTTPS GET, ~5s timeout).
2. If `latest.version > current_version` AND user hasn't dismissed this specific version: show non-blocking toast in the bottom-right: *"Harmony v{N} is available. [Restart to update] [Later] [Skip this version]"*.
3. **[Restart to update]** triggers the Tauri updater plugin: download the bundle, verify the ed25519 signature against the embedded pubkey, apply, restart. (Tauri handles all of this; we just call `update.downloadAndInstall()`.)
4. **[Later]** dismisses the toast for this session only; reappears next launch if still applicable.
5. **[Skip this version]** persists a `dismissed_version` string in app-data; toast stays suppressed until a version *higher* than the dismissed one is available.

**Never auto-restart.** Testers should never lose unsaved state without consent.

**Failure modes** (each surfaces a one-line tracing warning, app continues to function):
- Endpoint 5xx / 4xx / DNS failure → no toast; retry next launch
- Manifest parse error → no toast; log + sentry-style breadcrumb (no-op for alpha; placeholder for future telemetry)
- Signature verification failure during apply → user sees "Update could not be verified" with a "Report" link to the GitHub Issues; toast suppressed for that version

### 6.5 GitHub Pages setup

One-time:

1. Create empty `gh-pages` branch from `git checkout --orphan gh-pages && git rm -rf . && git commit --allow-empty -m "init"`.
2. In repo Settings → Pages: set source = `gh-pages` branch, root.
3. (Optional) Custom CNAME — if we want `updates.harmony.zeblith.net` instead of `zeblithic.github.io/harmony-client/latest.json`, configure a DNS CNAME record + `CNAME` file in `gh-pages`. Defer to v0.2.0; the `zeblithic.github.io` URL is fine for alpha.

The release workflow has push access to `gh-pages` via the default `GITHUB_TOKEN`.

## 7. Versioning + release notes

### 7.1 Versioning scheme

SemVer with pre-release counter. Pattern: `<major>.<minor>.<patch>[-<channel>.<n>]`.

Concrete sequence:

```
0.1.0-alpha.1   ← first cut
0.1.0-alpha.2   ← second cut (bug fixes, small features)
0.1.0-alpha.N   ← Nth iteration
0.1.0-beta.1    ← if/when we decide alpha → beta
0.1.0           ← first stable (probably won't happen for a while)
```

**Tauri auto-updater requires SemVer.** It compares `latest.version` against the running build's `version` using SemVer rules. Per the spec ("Identifiers consisting of only digits are compared numerically"), pre-release counters like `alpha.10` correctly compare as greater than `alpha.2` — no zero-padding needed. (The SemVer spec actively forbids leading zeroes on numeric identifiers, so `alpha.01` would be a spec violation.)

### 7.2 Version bump process

**Manual edit + commit BEFORE triggering the workflow.** The release workflow's `precheck` job (§5.2) verifies the input version equals `tauri.conf.json`'s `version` field — if they don't match, the workflow fails fast.

Operator steps:

```bash
# 1. Bump version in tauri.conf.json
# 2. Commit
git commit -am "release: bump version to 0.1.0-alpha.N"
git push
# 3. Trigger the workflow
gh workflow run release.yml -f version=0.1.0-alpha.N
```

This gives us a real git anchor (the version-bump commit) for "what's in this release." Alternative was workflow-patches-config-and-commits-itself; rejected as harder to reason about.

### 7.3 Release notes

`gh release create --generate-notes` produces a default body listing all PRs merged since the last tag. Format (GitHub native):

```markdown
## What's Changed
* feat(zeb-325): Phase 2c invite handshake by @jenglund in #159
* fix(zeb-323): Hard-coded relay fallback by @jenglund in #160
…

**Full Changelog**: https://github.com/zeblithic/harmony-client/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
```

The workflow creates the Release as **draft** with these auto-generated notes. Operator edits to add narrative framing (one paragraph: "highlights, known issues, what to test"), then publishes via `gh release edit --draft=false`. The published release is what testers see in their update toast.

This is the only manual step inside the release workflow's window — everything else is automated.

## 8. Identity portability invariants

These are NOT changes for v0.1.0 — they're invariants that must be documented and not violated.

### 8.1 OS keychain identifiers

Today's usage (verified in `src-tauri/src/identity.rs` and `iroh_endpoint.rs`):

- `service = "harmony.client"`
- accounts: `"identity.signing_key"`, `"iroh.secret_key"`, possibly others as we ship more features

**Invariant:** these strings freeze in v0.1.0. Any future change requires:
1. A migration plan (read-old, write-new, deprecate-old over N releases) documented in the release notes.
2. A signed-off Linear ticket explicitly authorizing the change.

**Why it matters:** OS keychain entries are independent of the app binary — they persist across reinstalls / upgrades. As long as we read with the same `(service, account)` pair, the user keeps their identity. Change either and the user's identity vanishes on next launch (the app generates a new one, and the original identity is silently orphaned).

### 8.2 Tauri bundle identifier

`tauri.conf.json` → `identifier: "net.zeblith.harmony"`.

**Invariant:** freezes in v0.1.0. Tauri uses this to namespace app-data directories (e.g., `~/Library/Application Support/net.zeblith.harmony/` on macOS). Change it and testers' communities + CRDT state appear to "disappear" on next launch.

### 8.3 App-data directory contents

Path varies per OS but the layout inside is ours:
- `identity/` — encrypted identity state (if not in keychain), pkarr publisher state
- `community-engines/{community-id}/` — per-community CRDT state, channel logs
- `settings.json` — UI preferences

These paths and on-disk formats are governed by their respective modules (Sub-project A doesn't dictate them). What Sub-project A DOES require: any change to the on-disk layout must include a migration helper that runs on first launch of the new version and translates v0.1.0 layouts forward. Out of scope for the initial pipeline build, but called out in `docs/release-process.md`.

## 9. Documentation deliverables

Three new files in `docs/`. Each is small (~1 page); they're tester-facing, written in plain English, no jargon.

### 9.1 `docs/install-macos.md`

Structure:

1. **Download** — link template `https://github.com/zeblithic/harmony-client/releases/latest`, instructions to pick `aarch64` (Apple Silicon) vs `x64` (Intel) — include screenshot of "About this Mac" showing which CPU.
2. **First launch** — double-click `.dmg` → drag to Applications → first launch shows Gatekeeper warning ("'Harmony' can't be opened because Apple cannot check it for malicious software"). **Workaround:** right-click the app in Applications → Open → "Open" in the warning dialog. After once, it's trusted forever.
3. **Quarantine attribute workaround** (if right-click doesn't work, fallback): `xattr -dr com.apple.quarantine /Applications/Harmony.app` in Terminal.
4. **Optional permissions** — first launch may prompt for keychain access, network access. Both required; deny → app won't function correctly.
5. **Updating** — automatic via in-app toast. Manual re-download fallback if updater fails.
6. **Uninstalling** — drag from Applications → Trash. To also remove identity + community state: `rm -rf ~/Library/Application\ Support/net.zeblith.harmony` (warn: destroys identity).

### 9.2 `docs/install-windows.md`

Similar shape, Windows-specific friction:

1. Download `.msi` from Releases.
2. Run installer; SmartScreen blocks ("Windows protected your PC"). **Workaround:** click "More info" → "Run anyway". After once, future runs trust it.
3. Default install location (`%LOCALAPPDATA%\Programs\Harmony` or wherever Tauri puts it).
4. First-launch firewall prompt (Tauri opens its own listening port for iroh) → "Allow access".
5. Updating + uninstalling sections analogous to macOS.

### 9.3 `docs/install-linux.md`

Simpler — Linux users tend to expect AppImage friction:

1. Download `.AppImage` from Releases.
2. `chmod +x harmony_*.AppImage`.
3. Double-click or run from terminal.
4. (Optional) Use [AppImageLauncher](https://github.com/TheAssassin/AppImageLauncher) to integrate with desktop menu — out of scope for our docs, link only.
5. First-launch: requires `libsecret` (gnome-keyring or KWallet) for identity persistence. Documented as a hard dependency; covered in `docs/headless-install.md` for server-mode fallback.
6. Updating + uninstalling.

### 9.4 `docs/release-process.md` (internal)

Operator-only, NOT in tester-visible docs.

1. **Preflight** — check `ci.yml` green on `main`, no draft PRs to merge first, decide version.
2. **Bump + commit** — exact commands.
3. **Trigger workflow** — `gh workflow run release.yml -f version=…`.
4. **Watch the workflow** — what to expect (build times, common failures).
5. **Manual smoke test on each platform** — install the artifact on a real machine, walk through the install doc end-to-end, confirm app launches + auto-updater check round-trips.
6. **Publish** — edit draft notes, publish.
7. **Post-release** — verify GitHub Pages updated (within ~30s of push); confirm an existing-install tester sees the toast on next launch.
8. **Severe-incident playbook** — keypair compromise (rotate + ship new pubkey + accept stranded clients); keypair LOSS (worse, same recovery but with more comms work); release-published-broken (revoke via GitHub Release deletion, ship hotfix N+1).

## 10. Operational concerns

### 10.1 First-release bootstrap (the chicken-and-egg)

Auto-updater can't update a tester from "nothing installed" to v0.1.0-alpha.1 — there's nothing to update. **The first release is download-only;** auto-updater takes over from N → N+1.

Documented in `docs/install-*.md`: download the latest from the Releases page. Reverification of new alphas (when public download URL changes if we rename) requires re-doc.

### 10.2 Workflow secret bootstrap

Before running the workflow for the first time, zeblith must:
1. Generate the Tauri signing keypair locally
2. Add the private key + passphrase as GH Actions secrets (`TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`)
3. Embed the public key in `tauri.conf.json`'s `plugins.updater.pubkey`
4. Commit (the pubkey in git is public-by-design; signing key alone determines authenticity)
5. Save the private key passphrase to 1Password + lockbox per §6.3

These are documented as a "one-time setup" section in `docs/release-process.md`. Subsequent releases just re-use the secrets.

### 10.3 GitHub Actions minutes budget

Per release:
- macos-14: ~10 min (Rust compile dominant)
- macos-13: ~10 min
- windows-latest: ~12 min
- ubuntu-22.04: ~8 min
- release fan-in: ~2 min

Total ~42 minutes of compute per release. GitHub Actions free tier = 2000 minutes/month for private repos, unlimited for public. **Verify `gh repo view zeblithic/harmony-client --json visibility` before relying on the public-repo assumption.** If currently public, this is free; if private, ~47 releases/month before exhausting free tier.

Either way: plenty of headroom for an alpha cadence (~2-4 releases/week).

### 10.4 Tauri version pinning

Tauri 2.10.1 today (`@tauri-apps/cli` in `package.json`, `tauri-build` etc. in `Cargo.toml`). The release workflow uses `npm run tauri build` which honors the package.json version. **Don't bump Tauri minor versions during alpha** unless there's a specific reason — bundle output format can shift between minors, breaking the updater bundle contract that installed clients rely on.

## 11. Test plan

### 11.1 Inline at release-trigger time (precheck job in release.yml, BEFORE matrix builds)

Per §5.2: gates run inline in the release workflow's precheck job, NOT via a standalone `ci.yml`. Gates (per CLAUDE.md):
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `npx tsc --noEmit`
- `npx vitest run`

Operator should still run these locally before bumping the version — release.yml's precheck is the safety net, not the first line of defense.

### 11.2 Workflow-internal (per matrix job, AFTER `npm run tauri build`)

Each matrix job runs a "post-build sanity" step:
1. Verify the installer file exists at the expected path.
2. Verify the updater bundle (`.tar.gz` + `.sig`) exists.
3. Verify the `.sig` parses as a valid Tauri signature (`openssl base64 -d` round-trip).

### 11.3 Manual smoke test (PRE-publish, per release)

Documented in `docs/release-process.md` step 5. For v0.1.0-alpha.1 specifically:
1. Download each platform's artifact onto a real machine (zeblith has macOS; one tester volunteers Linux + Windows for early cuts).
2. Walk through the corresponding install doc literally — note any friction not yet documented.
3. Launch the app, confirm window appears, confirm IdentityPanel renders.
4. Confirm auto-updater check fires on launch (visible in app logs OR by temporarily bumping `latest.json` to a fake higher version on a private branch and seeing the toast).
5. Quit + relaunch — identity persists from prior session.
6. Click a `harmony://invite/...` URL from email or another app — confirm the URL handler launches Harmony and opens the redeem dialog. (This validates the `bundle.deepLinks` config + per-OS protocol registration.)

### 11.4 Regression suite (every release after the first)

Same as 11.3 plus:
- Upgrade-from-prior path: install the previous alpha, run it once to seed identity, then trigger the auto-updater toast, accept, confirm post-upgrade identity is the same (same address hash visible in IdentityPanel).
- Cross-version compatibility: previous alpha communicates with new alpha in a community (validated as part of Sub-project D's playbook).

## 12. Risks + open questions

### 12.1 Identified risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Tauri auto-updater bundle format changes in minor versions | Low | High (breaks update path) | Pin Tauri version; verify update path in every smoke test |
| `macos-13` runner deprecated by GitHub | Medium (12-18 months out) | Medium (lose Intel build) | Track; switch to cross-compile on macos-14 when GH deprecates |
| Private signing key leaked | Low (depends on op security) | Severe (malicious updates possible) | HTTPS-only endpoint; embedded pubkey verification; rotation playbook (§6.3) |
| Private signing key LOST | Medium-Low | Severe (all installed clients stranded) | Two backups (1Password + lockbox); documented as Severe risk |
| GitHub Pages outage during release | Low | Low (testers don't auto-update, manual download still works) | Accept; reattempt on next release |
| Gatekeeper / SmartScreen workaround unreliable across OS versions | Medium | Medium (some testers can't install) | Track per-tester; if a workaround stops working, update install docs + cut hotfix release with notes |
| `harmony://invite/` URL handler doesn't register on some Linux DEs | Medium | Medium (testers can't click-to-join) | Manual paste fallback in Sub-project D's invite distribution doc |
| AppImage glibc compat breaks for some Linux distro versions | Low | Medium | Pin Ubuntu 22.04; document minimum glibc requirement in install-linux.md |

### 12.2 Deferred decisions (for v0.2.0+)

- Custom domain for update manifest (`updates.harmony.zeblith.net`) — DNS + CNAME work. Easy when needed; defer.
- Linux `.deb` / `.rpm` packaging — adds repo hosting concerns; defer.
- Signed builds (macOS notarization + Windows code-signing) — defer to public beta.
- Mobile builds (iOS + Android) — entirely separate sub-project.
- Auto-rollback if a release is bad — manual revoke + hotfix is fine for alpha cadence.

## 13. References

- Parent: [ZEB-327](https://linear.app/zeblith/issue/ZEB-327)
- Sibling sub-projects: B (TBD), C (TBD), D (TBD) — to be filed after Sub-A spec lands
- Tauri 2 updater plugin: <https://v2.tauri.app/plugin/updater/>
- Tauri 2 bundle config: <https://v2.tauri.app/reference/config/#bundleconfig>
- Tauri 2 deep links: <https://v2.tauri.app/plugin/deep-link/>
- GitHub Actions free tier: <https://docs.github.com/en/billing/managing-billing-for-github-actions/about-billing-for-github-actions#about-billing-for-github-actions>
- GitHub Pages: <https://docs.github.com/en/pages>
- Existing CI workflow (disabled): `.github/workflows/ci.yml.disabled` — re-enabling is in this sub-project's scope
- Existing keychain integration: `src-tauri/src/identity.rs` (search for `keyring::Entry::new`)
- macOS XprotectService dev-tools setup: `feedback_xprotectservice_dev_tools` (developer-only, NOT a release-pipeline concern but a tester-pipeline one if testers' macs hang on first-launch)
- `harmony://invite/` URL scheme: `src-tauri/src/community_invite.rs:695` (`URL_PREFIX`)
