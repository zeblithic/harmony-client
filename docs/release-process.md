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

**TL;DR:** Windows MSI (WiX) rejects pre-release identifiers. Use clean numeric SemVer like `0.1.0`, `0.1.1`, `0.2.0`. Never `0.1.0-alpha.N`, `-beta.N`, or `-rc.N`.

WiX — the Microsoft Windows MSI installer toolchain Tauri uses for the Windows matrix job — encodes version metadata as `Major.Minor.Build[.Revision]`: four numeric components, each a UInt16 (max 65535). The MSI Windows Installer format has no slot for SemVer's pre-release suffix; the WiX compiler rejects strings containing letters or dashes after the patch component.

Concretely: if `tauri.conf.json` `version` is `0.1.0-alpha.1`, the macOS + Linux matrix jobs succeed but the Windows job fails inside `tauri build` with a WiX validation error. The release workflow's "Verify all platform bundles present" step then fails because the required `*-setup.nsis.zip` glob is missing, the draft release is never created, and you've burned ~15 min of Actions minutes on a dead run.

**Why we don't use Tauri's `bundle.windows.wix.version` override.** Tauri 2.x exposes `bundle.windows.wix.version` to set a Windows-specific 4-numeric version (e.g., `0.1.0.1`) while leaving the SemVer `version` field as `0.1.0-alpha.1`. We don't use it because:

1. **Support is inconsistent across Tauri 2.x point releases** — works in some, silently ignored in others. Pinning to a specific Tauri version to make it work is fragile.
2. **Two version strings doubles operator burden and introduces drift risk** — every bump touches both fields, and the auto-updater manifest may use either, causing version-check mismatches.
3. **Most large Tauri projects converge on plain-numeric SemVer** — matches Tauri's auto-updater documentation examples, the path of least friction.

**How to signal "alpha" status without a version suffix:**

- The `0.x.y` range IS the alpha cycle by convention. `0.1.0`, `0.1.1`, `0.1.2`, ..., `0.2.0`, etc.
- Release notes title: `## Harmony v0.1.0 — alpha` for narrative framing.
- GitHub Release name: `v0.1.0 (alpha)` for visibility on the releases page.
- README + About modal: explicit "alpha" labels visible to users.

**This constraint was caught by Cursor Bugbot on PR #164** — the operator playbook originally used `0.1.0-alpha.N+1` examples that would have broken every Windows build. Lesson: validate the version pattern against ALL platform bundlers before committing to a docs convention.

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

Check: no draft PRs you meant to include. CI is disabled (`ci.yml.disabled`) — trust local test runs instead.

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

Expected wall-clock times:
- precheck job: ~7 min (fmt + clippy + version cross-check)
- matrix jobs (4 platforms in parallel): ~10-12 min each
- release fan-in: ~2 min
- Total: ~25 min

Common early failures and fixes:
- `version mismatch`: tauri.conf.json version doesn't match `-f version=` input — re-run with the correct value.
- `TAURI_SIGNING_PRIVATE_KEY not set`: OP-2 incomplete — set secrets and re-run.
- macOS `xcode-select` failure: transient GH runner issue — retry the run.

---

### Step 5: Manual smoke test (pre-publish)

See §3 below. Do this before publishing the draft release.

---

### Step 6: Edit draft release notes (still draft — do NOT un-draft yet)

The workflow creates a draft release with auto-generated notes. Add narrative framing:

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

Pass all 6: publish the release. Any failure: fix or document as a known issue before publishing.

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
