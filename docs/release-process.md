# Harmony Release Process (Operator Playbook)

Internal only. Not for testers. All commands assume `zsh` on macOS unless noted.

---

## 1. One-time bootstrap (OP-1 through OP-7)

Run these once before the first release. If you're picking up mid-stream, check which steps are already done via their VERIFICATIONs.

---

### OP-1: Generate Tauri signing keypair

WHY: Tauri's auto-updater rejects bundles that don't match the embedded pubkey. The keypair is permanent unless you rotate it (see §4).

```bash
mkdir -p ~/.tauri
npx @tauri-apps/cli signer generate -w ~/.tauri/harmony-updater.key
# Tauri prompts for a passphrase — choose strong, store in 1Password immediately
```

OUTPUTS: `~/.tauri/harmony-updater.key` (private) + `~/.tauri/harmony-updater.key.pub` (public).

VERIFICATION:
```bash
ls -l ~/.tauri/harmony-updater.key ~/.tauri/harmony-updater.key.pub
# Both files must exist; .key should be ~500 bytes, .key.pub ~100 bytes
```

DONE_WITH_CONCERNS: `npx @tauri-apps/cli signer` is the Tauri 2.x CLI spelling. If this fails, check `package.json` for the installed CLI version and consult `https://v2.tauri.app/reference/cli/`. The underlying subcommand may be `tauri signer generate` via the local binary at `node_modules/.bin/tauri`.

---

### OP-2: Set GitHub Actions secrets

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY \
  --repo zeblithic/harmony-client \
  < ~/.tauri/harmony-updater.key

gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD \
  --repo zeblithic/harmony-client
# gh prompts you to type the passphrase; it will not echo
```

VERIFICATION:
```bash
gh secret list --repo zeblithic/harmony-client
# Must show: TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

---

### OP-3: Embed public key in tauri.conf.json

WHY: The pubkey is baked into every shipped binary. Installed clients verify updates against it. It is public-by-design — committing it to git is correct and intentional.

```bash
PUBKEY=$(cat ~/.tauri/harmony-updater.key.pub)
echo "$PUBKEY"
# Copy this value, then edit src-tauri/tauri.conf.json:
# Replace "PLACEHOLDER_REPLACE_WITH_OP_3_OUTPUT" with the value above
# The key goes under: plugins.updater.pubkey
```

VERIFICATION:
```bash
grep -A2 '"pubkey"' src-tauri/tauri.conf.json
# Must show your actual key, not the placeholder string
```

Commit + push before triggering first release:
```bash
git add src-tauri/tauri.conf.json
git commit -m "chore: embed Tauri updater public key (OP-3)"
git push
```

---

### OP-4: Back up keypair + passphrase

WHY: Loss = all installed clients permanently stranded. Leak = attacker can push malicious auto-updates.

- Create a 1Password vault entry: **"Harmony updater signing key"**
  - Attach `~/.tauri/harmony-updater.key`
  - Attach `~/.tauri/harmony-updater.key.pub`
  - Store the passphrase in the entry's password field
  - Add note: "Lose this = all installed clients stranded forever. Leak this = attacker signs malicious updates. Rotate per docs/release-process.md §4."
- Sealed printout in physical lockbox — print both file contents + passphrase.

VERIFICATION: Open 1Password and confirm both files are attached and the passphrase is readable.

---

### OP-5: Create gh-pages branch

WHY: The release workflow pushes `latest.json` (update manifest) to `gh-pages`. The branch must exist and be orphaned (no source code history).

```bash
cd /path/to/harmony-client
git checkout --orphan gh-pages
git rm -rf .
git commit --allow-empty -m "init gh-pages branch for tauri updater manifest"
git push origin gh-pages
git checkout main
```

VERIFICATION:
```bash
git ls-remote origin gh-pages
# Must print a SHA; empty output means branch doesn't exist
```

---

### OP-6: Enable GitHub Pages

- Repo → Settings → Pages → Source: **Deploy from a branch**
- Branch: `gh-pages` / `/ (root)`
- Save → wait ~30s

VERIFICATION:
```bash
curl -sI https://zeblithic.github.io/harmony-client/ | head -5
# HTTP/2 200 or 404 — either is fine; what matters is the response is from GitHub Pages
# A connection error means Pages is not yet enabled
```

---

### OP-7: Verify repo visibility

WHY: Free-tier GitHub Actions minutes (2000/mo) apply only to private repos. Public repos are unlimited. At ~42 min/release, private-repo budget allows ~47 releases/month.

```bash
gh repo view zeblithic/harmony-client --json visibility -q .visibility
# Expected: "PUBLIC" (unlimited minutes)
# If "PRIVATE": budget carefully — alpha cadence is fine, but be aware
```

No action needed if PUBLIC. If PRIVATE and you're exceeding 2000 min, consider switching to public or buying minutes.

---

### 1.8 Why we don't use SemVer pre-release suffixes

**TL;DR:** Use clean numeric SemVer like `0.1.0`, `0.1.1`, `0.2.0`. Never `0.1.0-alpha.N`, `-beta.N`, or `-rc.N`.

The original binding constraint was Windows MSI (WiX): four-numeric `Major.Minor.Build[.Revision]` UInt16s, no pre-release suffix possible. We ultimately chose to ship **NSIS** for Windows (smaller installer, no admin elevation, what Tauri's own docs recommend for distribution-style apps) — and NSIS itself is not as strict as WiX about version strings — but we kept the numeric-SemVer policy because:

- macOS `Info.plist`'s `CFBundleShortVersionString` requires plain `X.Y.Z` numeric and won't parse pre-release identifiers anyway.
- The Tauri updater's `version` field must match exactly across all bundles + the GitHub release tag + the `latest.json` manifest. One canonical form (numeric SemVer) collapses three opportunities for drift into zero.
- Two-version configurations (one for the SemVer field, one for the bundler) double operator burden — see below.

Historical concrete failure: when `tauri.conf.json` `version` was `0.1.0-alpha.1` (PR #164's first draft), the Windows matrix job failed inside `tauri build` with a WiX validation error before we switched to NSIS. The release workflow's "Verify all platform bundles present" step then fails because the required `*-setup.nsis.zip` glob is missing, the draft release is never created, and you've burned ~15 min of Actions minutes on a dead run.

**Why we don't use Tauri's `bundle.windows.wix.version` override.** Tauri 2.x exposes `bundle.windows.wix.version` to set a Windows-specific 4-numeric version (e.g., `0.1.0.1`) while leaving the SemVer `version` field as `0.1.0-alpha.1`. We don't use it because:

1. **Support is inconsistent across Tauri 2.x point releases** — works in some, silently ignored in others. Pinning to a specific Tauri version to make it work is fragile.
2. **Two version strings doubles operator burden and introduces drift risk** — every bump touches both fields, and the auto-updater manifest may use either, causing version-check mismatches.
3. **Most large Tauri projects converge on plain-numeric SemVer** — matches Tauri's auto-updater documentation examples, the path of least friction.

**How to signal "alpha" status without a version suffix:**

- The `0.x.y` range IS the alpha cycle by convention. `0.1.0`, `0.1.1`, `0.1.2`, ..., `0.2.0`, etc.
- Release notes title: `## Harmony v0.1.0 — alpha` for narrative framing.
- GitHub Release name: `v0.1.0 (alpha)` for visibility on the releases page.
- README + About modal: explicit "alpha" labels visible to users.

**This constraint was caught by Cursor Bugbot during the first-release bootstrap** — the operator playbook originally used `0.1.0-alpha.N+1` examples that would have broken every Windows build. The `release.yml` precheck regex was also tightened to enforce numeric-only at the workflow boundary (catching at "operator passed bad input" instead of at "Windows runner failed 15min into build"). Lesson: validate the version pattern against ALL platform bundlers before committing to a docs convention, and make the enforcement match the policy.

---

## 2. Per-release operator flow

Prerequisites: OP-1 through OP-7 complete.

---

### Step 1: Preflight

```bash
cd /path/to/harmony-client
git checkout main
git pull --ff-only
git status   # must be clean
```

Check: no draft PRs you meant to include.

**Confirm CI is green on the release base.** `.github/workflows/ci.yml` is live (it was
disabled when this playbook was first written; that changed in ZEB-676). It runs
`rust-check` (fmt + clippy), three sharded `rust-test` jobs behind a
`Rust — test (nextest)` roll-up, `msrv`, and `frontend` — ~12 min wall-clock warm.

```bash
gh run list --workflow=ci.yml --repo zeblithic/harmony-client --branch main --limit 1
```

Also run the full local gate — CI green is necessary, not sufficient, because the
release build compiles bundles CI never produces:

```bash
# Rust gates run from src-tauri/ (see CLAUDE.md — the inner .cargo/config.toml
# is discovered from the cwd). The subshell keeps that cd from leaking into the
# frontend commands below; without it a pasted block would cd twice and silently
# skip clippy + nextest.
(
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
)

# from repo root
npx tsc --noEmit && npx vitest run
```

Each command must pass. Note that `&&`-chaining the Rust three would stop at the
first failure — run them all so you see every problem in one pass.

> Do **not** use `scripts/test-select` here. Its rotating-partition selection is for
> iterative dev gates; release validation takes the full sweep (see `CLAUDE.md`).

---

### Step 2: Bump version

Edit `src-tauri/tauri.conf.json`: change `"version": "0.1.X"` to `"0.1.X+1"` (numeric SemVer only — see [§1.8](#18-why-we-dont-use-semver-pre-release-suffixes) for why no `-alpha.N` suffix).

```bash
git diff src-tauri/tauri.conf.json   # confirm one-line change, nothing else
git commit -am "release: bump version to 0.1.X+1"
git push
```

---

### Step 3: Trigger the workflow

```bash
gh workflow run release.yml \
  --repo zeblithic/harmony-client \
  -f version=0.1.X+1
```

---

### Step 4: Watch the workflow

```bash
gh run list --workflow=release.yml --repo zeblithic/harmony-client --limit 5
gh run watch   # streams live; Ctrl-C is safe, run continues
```

Expected wall-clock times (measured from recent real releases, not aspirational — ZEB-764):
- precheck job (gates the matrix — `build` needs it): several minutes; it runs
  without CI's warm sccache/R2 cache, so it is slower here than the ~4 min CI equivalent.
- matrix build jobs run in parallel once precheck passes, so the matrix is only
  as fast as its long pole:
  - `macos-15-intel` (x86_64): **~40 min — the long pole**, ~3× the ARM leg. A
    40-minute Intel build is healthy, **NOT** hung — do not cancel it.
  - `windows-latest`: ~28 min
  - `ubuntu-22.04`: ~14 min
  - `macos-14` (aarch64): ~13 min
- release fan-in (draft assembly): <1 min
- **Total: ~70 min for a clean run**, plus ~10-15 min per failed-leg rerun. The
  two most recent real releases ran **1h19m** and **1h41m** end-to-end (each
  included a leg rerun).

A plain `gh run rerun --failed` recompiles the whole failed leg from scratch. The
macOS legs now retry their DMG **bundling** in place (ZEB-764), so a transient
`hdiutil` flake costs ~1-2 min (a warm-`target` re-bundle) instead of a fresh
~40-min recompile.

Common early failures and fixes:
- `version mismatch`: tauri.conf.json version doesn't match `-f version=` input — re-run with the correct value.
- `TAURI_SIGNING_PRIVATE_KEY not set`: OP-2 incomplete — set secrets and re-run.
- macOS `xcode-select` failure: transient GH runner issue — retry the run.

---

### Step 5: Manual smoke test (pre-publish)

See §3 below. Do this before publishing the draft release.

---

### Step 6: Edit draft release notes (still draft — do NOT un-draft yet)

The workflow creates a draft release with auto-generated notes. Add narrative framing.

Notes are drafted **ahead of the release**, in the PR that bumps the version, and live at
`docs/release-notes/vX.Y.Z.md` (see `docs/release-notes/v0.2.0.md` for the shape). Two reasons:
they get reviewed like anything else, and a large release is far easier to summarize while the
work is fresh than from a long generated list on release day. (v0.2.0 spanned 190 PRs; a
`--generate-notes` wall that size is not something a tester reads.) Publish with:

```bash
gh release edit vX.Y.Z --repo zeblithic/harmony-client \
  --notes-file docs/release-notes/vX.Y.Z.md
```

Search the file for `<!-- OPERATOR` first — that marks any line that could only be filled in
from the smoke-test results, and it must be resolved before un-drafting.

The template below is the original inline form, kept for reference:

```bash
gh release edit v0.1.X+1 \
  --repo zeblithic/harmony-client \
  --notes "$(cat <<'EOF'
## Harmony v0.1.X+1

### Highlights
- <one-paragraph summary of what changed>

### Known issues
- <bulleted list>

### What to test
- <bulleted list of focus areas for testers>

## What's Changed
EOF
)"
```

> ⚠️ Don't un-draft yet. Run Step 6a (manifest gen + push) first, then verify, then un-draft in Step 6b. The workflow deliberately stops at "draft + artifacts uploaded" so a broken bundle never reaches installed clients via the auto-updater channel before you've smoke-tested.

---

### Step 6a: Generate + push the update manifest

Download the signed updater bundles from the draft release, extract signatures, build `latest.json`, push to `gh-pages`. The workflow doesn't do this because GitHub Pages serves the manifest to all installed clients within ~30s — pushing before smoke-test means a broken release auto-updates to everyone immediately.

```bash
set -euo pipefail
VERSION="0.1.X+1"
REPO_URL="https://github.com/zeblithic/harmony-client"
WORKDIR="$(mktemp -d -t harmony-release-XXXXXX)"
cd "$WORKDIR"

# Download all .sig files (we only need the signatures + URL construction)
gh release download "v${VERSION}" --repo zeblithic/harmony-client --pattern '*.sig' -D sigs/

# Fail loudly if any expected signature is missing
required_sigs=(
  "Harmony_aarch64-apple-darwin.app.tar.gz.sig"
  "Harmony_x86_64-apple-darwin.app.tar.gz.sig"
)
missing=()
for s in "${required_sigs[@]}"; do
  if [ ! -f "sigs/$s" ]; then missing+=("$s"); fi
done
# Windows + Linux .sig basenames are version-stamped, glob-check:
for glob in '*-setup.nsis.zip.sig' '*amd64.AppImage.tar.gz.sig'; do
  if ! ls sigs/$glob >/dev/null 2>&1; then missing+=("$glob"); fi
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "MISSING SIGNATURES — refusing to generate manifest:"
  printf '  - %s\n' "${missing[@]}"
  exit 1
fi

PUB_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
WIN_SIG_FILE=$(ls sigs/*-setup.nsis.zip.sig | head -1)
WIN_URL_FILE=$(basename "$WIN_SIG_FILE" .sig)
LIN_SIG_FILE=$(ls sigs/*amd64.AppImage.tar.gz.sig | head -1)
LIN_URL_FILE=$(basename "$LIN_SIG_FILE" .sig)

cat > latest.json <<EOF
{
  "version": "${VERSION}",
  "notes": "See ${REPO_URL}/releases/tag/v${VERSION}",
  "pub_date": "${PUB_DATE}",
  "platforms": {
    "darwin-aarch64": {
      "signature": "$(cat sigs/Harmony_aarch64-apple-darwin.app.tar.gz.sig)",
      "url": "${REPO_URL}/releases/download/v${VERSION}/Harmony_aarch64-apple-darwin.app.tar.gz"
    },
    "darwin-x86_64": {
      "signature": "$(cat sigs/Harmony_x86_64-apple-darwin.app.tar.gz.sig)",
      "url": "${REPO_URL}/releases/download/v${VERSION}/Harmony_x86_64-apple-darwin.app.tar.gz"
    },
    "windows-x86_64": {
      "signature": "$(cat "$WIN_SIG_FILE")",
      "url": "${REPO_URL}/releases/download/v${VERSION}/${WIN_URL_FILE}"
    },
    "linux-x86_64": {
      "signature": "$(cat "$LIN_SIG_FILE")",
      "url": "${REPO_URL}/releases/download/v${VERSION}/${LIN_URL_FILE}"
    }
  }
}
EOF
cat latest.json   # sanity-check it parses + all URLs/sigs present

# Push to gh-pages
cd /tmp
rm -rf harmony-gh-pages
git clone --branch gh-pages --single-branch git@github.com:zeblithic/harmony-client.git harmony-gh-pages
cp -f "${WORKDIR}/latest.json" harmony-gh-pages/latest.json
cd harmony-gh-pages
git add latest.json
git commit -m "release: v${VERSION} manifest"
git push origin gh-pages
cd -

rm -rf "$WORKDIR"
```

After the push, GitHub Pages refreshes within ~30s. Verify before un-drafting:

```bash
sleep 45
curl -s https://zeblithic.github.io/harmony-client/latest.json | jq '.version, .platforms | keys'
# Expected: "0.1.X+1" + all 4 platform keys present
```

---

### Step 6b: Un-draft the release

Only after Step 6a verifies the manifest is live and well-formed:

```bash
gh release edit v0.1.X+1 \
  --repo zeblithic/harmony-client \
  --draft=false
```

---

### Step 7: Post-release verification

```bash
# Manifest appears within ~30s of release publish
curl -s https://zeblithic.github.io/harmony-client/latest.json | jq .version
# Expected: "0.1.X+1"

# Confirm artifacts are attached
gh release view v0.1.X+1 --repo zeblithic/harmony-client
# Should list: .dmg (x2), .exe, .AppImage, .tar.gz (x2), and their .sig files
```

---

## 3. Smoke test playbook (per release, pre-publish)

Run on at least one platform per release. macOS aarch64 is zeblith's machine; Linux + Windows rely on a tester volunteer for early cuts.

1. **Download artifact.** From the draft release page, download the platform-appropriate artifact (e.g., `harmony_0.1.X+1_aarch64.dmg`).

2. **Walk the install doc literally.** Open `docs/install-macos.md` (or `-windows`, `-linux`). Follow every step exactly as written. Note any friction not yet documented.

3. **Launch the app.** Confirm the main window appears. Confirm IdentityPanel renders with your identity address.

4. **Auto-updater check.** Confirm the updater check fires on launch — check app logs or look for the update toast. To force a toast: temporarily push a fake-higher version to `latest.json` on a local branch (or just confirm the log line `Checking for updates…` appears).

5. **Persist across relaunch.** Quit, relaunch. Confirm the identity address shown is the same as before (keychain persists).

6. **Deep-link handler.** Click a `harmony://invite/...` URL from Mail.app or another app. Confirm:
   - macOS/Windows: OS activates Harmony (or launches it if closed)
   - `RedeemInviteDialog` opens with the URL pre-populated
   - This validates `bundle.deepLinks` config + per-OS protocol registration

7. **Diagnostics log file (ZEB-379).** After launch, confirm a rolling log file is being written under the app-data dir — this is the channel testers attach to bug reports:
   - macOS: `~/Library/Application Support/net.zeblith.harmony/logs/harmony.<date>.log`
   - Windows: `%APPDATA%\net.zeblith.harmony\logs\harmony.<date>.log`
   - Linux: `~/.local/share/net.zeblith.harmony/logs/harmony.<date>.log`

   The file should exist and grow as you use the app. Optionally launch once with `RUST_LOG=harmony_app=debug` and confirm the extra debug lines appear (in the file and, for a terminal-launched build, on stdout).

### First-run onboarding (ZEB-338) — required on every release

Run on a machine with NO existing Harmony identity (or wipe first). This exercises the owner-identity hard gate that unblocks fresh installs.

1. **Wipe identity.** Remove `~/.harmony/` and the `harmony.client` keychain entry:
   - macOS: `rm -rf ~/.harmony` then in Keychain Access delete the `harmony.client` entry (or `security delete-generic-password -s harmony.client`).
   - Windows: delete `%USERPROFILE%\.harmony` and the `harmony.client` entry in Credential Manager.
   - Linux: `rm -rf ~/.harmony` and clear the libsecret entry (`secret-tool clear service harmony.client`).
2. **Launch.** The `WelcomeModal` appears at the **"Create my identity"** pane and is a HARD GATE — pressing `Esc` and clicking outside the modal do NOT dismiss it.
3. **Create identity.** Click **Create my identity**. A "Creating your identity…" state shows for ~3 s (the node stops, mints, and restarts), then the pane transitions to the backup step.
4. **Save backup.** Enter a passphrase of at least `MIN_RECOVERY_PASSPHRASE_LEN` characters (currently **12**), click **Save recovery file**, pick a temp path. Export succeeds.
5. **Main UI works.** Modal closes; the main UI loads; **+ Create community** succeeds (no "Owner identity not loaded" / "crdt_state missing" error — that error must NEVER be reachable through the UI now).
6. **Returning user.** Quit + relaunch — the main UI loads directly (no Welcome), and NO backup banner (you backed up).
7. **Skip path.** Wipe again, relaunch, this time click **Skip for now → I accept the risk**. The main UI loads with a persistent backup-reminder banner. Relaunch → the banner persists. Click **Back up now**, save → the banner disappears and stays gone on the next launch.
8. **(If a Zeblithic invite URL is available) Deep-link + fresh install.** Wipe, then open the `harmony://invite/...` URL to launch the app. The Welcome modal still hard-gates first; after you mint + back up (or skip), the `RedeemInviteDialog` opens automatically with the invite pre-filled.

Pass all 7 base steps **and** the first-run onboarding checklist: publish the release. Any failure: fix or document as a known issue before publishing.

---

## 4. Severe-incident playbooks

---

### Signing key LEAK (attacker has private key)

1. Generate a new keypair locally (OP-1 again, new passphrase).
2. Update `src-tauri/tauri.conf.json` `plugins.updater.pubkey` with the new public key.
3. Update GH Actions secrets to the new private key + passphrase (OP-2 again).
4. Update 1Password + lockbox with the new keypair (OP-4 again).
5. Cut a new release immediately (§2 flow). This is the first release clients will refuse to install without the new pubkey — existing installed clients with the old pubkey embedded **cannot auto-update to this new release**.
6. **Communicate to ALL testers:** "Manual re-download required. Auto-update from any prior version is broken. Download `v0.1.X+1` from the Releases page and re-install."
7. Write a post-mortem: how did the key leak? Tighten backup hygiene.

Recovery is possible but requires every tester to manually reinstall. Treat signing key with the same care as a root CA.

---

### Signing key LOSS (backups all destroyed)

Same recovery path as leak (generate new keypair, ship release, require manual reinstall), but with worse comms: testers may not understand why auto-update broke. Write a post-mortem. Improve backup hygiene.

---

### Release published broken (critical bug found post-publish)

1. **Delete the broken release** (removes GitHub Release + tag):
   ```bash
   gh release delete v0.1.X \
     --repo zeblithic/harmony-client \
     --cleanup-tag --yes
   ```

2. **Revert `latest.json`** to the prior version's manifest so installed clients don't auto-update to the broken version:
   ```bash
   # Option A: push a revert to gh-pages manually
   git checkout gh-pages
   git revert HEAD   # reverts the latest.json push
   git push origin gh-pages
   git checkout main

   # Option B: cut hotfix immediately (faster if the fix is trivial)
   ```

3. **Cut hotfix** `v0.1.X+1` via the §2 flow with the bug fixed.

4. Anyone who already downloaded the broken `N` will auto-update to `N+1` once it's published (if `latest.json` was reverted in time; otherwise they may be stranded on `N` until they manually check for updates or relaunch).
