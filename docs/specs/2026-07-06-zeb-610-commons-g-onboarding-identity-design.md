# ZEB-610 · Commons G — onboarding & identity/backup restyle (design)

**Ticket:** ZEB-610 (Commons G), child of epic ZEB-603 (Commons design adoption).
**Branch:** `zeb-610-commons-g-onboarding-identity` off `main`@`beb036f5`.
**Scope:** frontend-only, **honest restyle** — restyle the self-sovereign onboarding / identity / backup surfaces to the Commons design system, rendering only real or derived data. Functionality wins over the mock.
**Reference:** `docs/design/commons/references/Harmony Onboarding.dc.html`, `screens/05-onboarding.png`.
**Predecessors:** Commons A–F (ZEB-604..609) merged; this follows the same §0-ledger + budget-0-token + SDD-per-surface cadence as F (#411).

---

## §0 — Premise corrections (honesty ledger)

The mock shows data a first-run app cannot honestly have. Verified each element against real code (code-map explorer, 2026-07-06). Method mirrors ZEB-609 §0.

### 0.1 — The centerpiece is not backed: **Step 3's 12-word phrase**
`mint_owner_identity` returns `MintIpcResult { state, recovery_token }` where `recovery_token` is an **opaque single-use UUID** (5-min TTL), *not* seed material (`src-tauri/src/owner_commands.rs:226/282`). There is **no `#[tauri::command]` that generates or displays an owner mnemonic** — the 24-word owner mnemonic exists only via the CLI (`export_owner_mnemonic_words_with_keychain`, `src-tauri/src/recovery_cli.rs:239`; absent from the `generate_handler!` list in `lib.rs`). The GUI's real recovery artifacts are exactly two: (1) a **passphrase-encrypted `.bin` file** (`export_owner_recovery_file_to_path` → `ExportInfo { path, byteLen, identityHash }`), and (2) the **owner-id fingerprint** (`OwnerStateView.ownerId`, shown `xxxx·xxxx·…`). `WelcomeModal`'s tests further enforce a **redaction invariant**: `container.innerHTML` must never contain a `[0-9a-f]{32,}` run — a deliberate no-seed-in-webview posture.
**Decision (Jake, 2026-07-06):** **file-based recovery kit; defer the phrase.** Step 3 restyles the real encrypted-file + passphrase + keychain flow (two of the mock's three mechanisms). The word-grid is dropped. → follow-up ticket (§6).

### 0.2 — DID shown *before* mint → surface the real fingerprint *after* mint
The mock's Step 2 shows `did:harmony:7f3a·…` before minting. The owner-id does not exist until `mint_owner_identity` runs. **Honest:** Step 2 ("Create identity") explains the keypair + carries the "Mint identity" CTA; the real fingerprint is surfaced *post-mint* (backup/done). No fabricated pre-mint DID.

### 0.3 — Handle "@jake · ✓ available" → dropped
Handle availability implies a global namespace registry, which contradicts self-sovereign keypair identity. No such service exists. **Drop** the handle input + availability affordance from Step 2. The user's **display name** is a real, separate field set at the end (`NamePromptModal` → `handleNamePromptSave`), not a mint-time handle.

### 0.4 — Redeem-invite preview "142 members · 6 channels" → dropped
Per-community member/channel counts for a community you have **not joined** are unknowable client-side (same class ZEB-609 §0 dropped). **Keep:** the resolved community name + the **"✓ signature verified"** inviter tick (invites are signed). **Expect:** the inviter *display name* to fall back to a stub (Phase-2/often-null, ZEB-281) — mirror F's `?? 'an unknown member'` idiom.

### 0.5 — DevicesPanel: key-rotation / revoke / danger-zone → not built, do not invent
`DevicesPanel.svelte` has **no** key-rotation UI, **no** revoke-device control, **no** danger-zone, and **no** backing IPC for any of them (device rows are read-only: rename + butler-pin only). **Do not add** that chrome. Backup status is only the boolean `canBackUp` (disables the button + tooltip). The self-sovereign badge **is** honest (always true for the owner viewing the panel); `ed25519` key type and local **membership count** are real static/derivable facts and are kept. **Verify at implementation:** whether an identity *creation date* is persisted — if not, omit "Created …" rather than fabricate (a device's `enrolledAt` is available; the owner's creation date may not be).

### 0.6 — Kept (real, no correction needed)
Owner-id fingerprint (`xxxx·xxxx·…`), encrypted-file + passphrase + keychain backup, `canBackUp` gating, device list (trust badge, `enrolledAt`, `fingerprint`, rename, butler-pin), SAS pairing (6-digit numeric), 24-word *restore* input (`OwnerRestoreWizard`, input-only), days-since-backup (drives `BackupReminderBanner` visibility — the "It's been N days" figure is real). Traffic-light window dots are mock chrome (real Tauri window supplies chrome) — omit.

---

## §1 — Progress / wizard model (honest progress)

The mock renders "five calm steps" on one morphing card. The real architecture is **`WelcomeModal`** (a hard mint-gate: stages `explain → minting → backup → skip-confirm`, also hosting `joining`/`restore` branches) **+ `RedeemInviteDialog`** (post-mint, also reachable via deep-link) **+ `NamePromptModal`** (post-mint display name). Steps 4–5 are separate, optional, and branch-reachable.

**Decision:** apply the pip-rail idiom only to the linear path `WelcomeModal` actually owns, and let the other surfaces *rhyme* without a false global counter — the honesty principle applied to progress, not just data.

- **`WelcomeModal`** gets the Commons card chrome + an **honest 3-pip rail**: `Welcome · Create · Back up`, with the **backup pip in clay** and a mono `Step N of 3` eyebrow on stages 2–3. (Not "N of 5".)
- **`RedeemInviteDialog`** and **`NamePromptModal`** restyle to the same sage card language (resolved-community preview card; success/profile chip) **without** a step counter.
- We do **not** refactor redeem/name-prompt into modal stages to force a literal 5 — that restructures the mint gate and breaks the deep-link path (against "functionality wins over the mock").

---

## §2 — Per-surface restyle specs

**Global token vocabulary** (all defined in `src/app.css`, light + `:root[data-theme='dark']`; both values exist):
- **Sage / primary:** `--accent` (#466b4c), `--accent-hover`, `--primary-deep` (#2f4a35), `--primary-soft` (#e4ece2), `--primary-border` (#c9d6c6). Buttons/checkbox/focus-ring/avatars/verified-ticks.
- **Clay / governance (Step-3 + banner ONLY):** `--gov-clay` (#b9742c), `--gov-clay-soft` (#f1e2cc), `--gov-clay-deep` (#5a4321).
- **Surfaces:** `--paper`, `--surface`/`--bg-primary`, `--bg-secondary`, `--bg-tertiary`, `--surface-raised`, `--surface-highlight`, `--border`, `--border-default`, `--line-soft`.
- **Text:** `--text-primary`, `--text-secondary`, `--text-muted`, `--faint`, `--text-bright`.
- **Danger (DevicesPanel only, existing usage):** `--danger`, `--danger-deep`, `--danger-text-muted`, `--danger-border-muted`.
- **Shadows:** `--shadow-e1` (cards), `--shadow-e2`, `--shadow-e3` (modal float). **Fonts:** `--font-display` (Newsreader — headings/wordmark/community names), `--font-ui` (Public Sans — body/labels/buttons), `--font-mono` (IBM Plex Mono — fingerprint/DID/SAS/dates/counts/"Step N").
- **Whitelisted idioms only** for shades with no token: `color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))` (clay-tan hairline, per 608/609), `color-mix(in srgb, var(--accent) 12%, transparent)` (sage focus ring), `transparent`. **Zero raw hex, zero new `app.css` tokens.**

### 2.1 — Shared primitive: `WizardProgress.svelte` (new)
Small bespoke component (the pip idiom differs from `PipMeter` — one *elongated* active pip + stepper semantics, so it is not a `PipMeter` variant). Props: `{ steps: { label: string; accent: 'sage' | 'clay' }[]; activeIndex: number; showCounter?: boolean }` (`showCounter` default `true`). Renders: a mono uppercase eyebrow `Step {activeIndex+1} of {steps.length}` shown only when `showCounter` (the **caller** decides — `WelcomeModal` passes `showCounter={stage !== 'explain'}`, so the Welcome step has no counter while Create/Back-up show "Step 2 of 3"/"Step 3 of 3") and a flex row of pips — inactive = `6×6` rounded `--faint`/track square; active = `24×6` rounded bar tinted `--accent` (sage) or `--gov-clay` (clay). New test file asserts pip count, the elongated-active position, the clay tint on a clay step, and that `showCounter={false}` hides the eyebrow. `<style>` budget 0 (var()/color-mix only).

### 2.2 — `WelcomeModal.svelte` (Steps 1–3; the mint hard-gate)
Keep the **hand-rolled backdrop** (`.modal-backdrop`/`.modal-content`, own `trapFocus`) — do **not** convert to shared `Modal` (that would add Esc/backdrop dismiss and break the hard gate). Restyle chrome in place; mount `WizardProgress`.
- **Stage `explain` (Step 1 · Welcome):** `<HarmonyMark>` + wordmark **"Harmony"** (`--font-display`), sage-italic tagline, calm body, sage-filled primary **"Get started"**, and the existing restore/join affordances. Pip 1 (sage), no counter.
- **Stage `minting` (Step 2 · Create identity):** `--font-display` heading "Create your identity"; honest body (keypair minted on this device; no email/password); **no** handle input, **no** fabricated DID. Sage-filled CTA whose accessible text starts with the existing pinned label. Pip 2 (sage), "Step 2 of 3".
- **Stage `backup` (Step 3 · Save recovery kit — the ONE clay step):** clay is contained here. `--gov-clay-soft` warning callout (clay-tan `color-mix` border, `--gov-clay-deep` text, 🔑) — "you hold the only copy…"; the real **encrypted-file** export (passphrase input `welcome-backup-passphrase` + save) and a sage keychain-stored note; a **sage** consent checkbox; sage-filled Continue. Pip 3 (**clay**), "Step 3 of 3". Surface the real owner fingerprint here (post-mint). **No phrase grid.**
- **Stage `skip-confirm`:** restyle in place (sage/clay per severity), copy byte-identical.
- **Preserve byte-identical:** every `data-testid` — `welcome-modal`, `welcome-modal-backdrop`, `welcome-create-identity`, `welcome-join-existing`, `welcome-restore-mnemonic`, `welcome-save-backup`, `welcome-skip-backup`, `welcome-skip-confirm`, `welcome-backup-passphrase`, `welcome-mint-error`, `welcome-already-exists-reload`; the friendly mint-error copy `/couldn.t create your identity/i` with raw detail inside `<details>`; and the **redaction invariant** (no `[0-9a-f]{32,}` run in `innerHTML`). `<style>` budget 0; tokenize the existing `.error { color: crimson }` → `--fg-error`/`--danger`.

### 2.3 — `RedeemInviteDialog` + `NamePromptModal` + App startup-error overlay
- **`RedeemInviteDialog`:** Commons **resolved-community preview card** — sage avatar chip, community name in `--font-display`, **"✓ signature verified"** inviter tick (sage), inviter name via the F fallback idiom. **No** member/channel counts. Sage-filled join CTA + muted "later" skip. Preserve existing redeem testids/roles and the `redeem_invite` wiring.
- **`NamePromptModal`:** Commons success/profile chip + display-name input; preserve every `data-testid` and the Enter-scoped-to-input behavior; copy byte-identical.
- **App startup-error overlay** (`App.svelte:3850-3881`): Commons pass on the hand-rolled `.modal-overlay`/`.modal-content` — preserve `role="alertdialog"`, testids `startup-error-backdrop/-modal/-detail/-retry`, and the headline **"Couldn't start Harmony"**. `.redeem-status-banner` restyle to sage. `App.svelte` `<style>` not allowlisted (budget 0) — no raw hex.

### 2.4 — `DevicesPanel.svelte`
Restyle the **three real sections** to Commons cards; add **no** invented chrome (§0.5).
- **Owner header card:** `--surface-raised` card + `--border`, sage avatar, name (`--font-display`), a **"● self-sovereign"** badge (mono 600 uppercase pill, `--primary-deep` on `--primary-soft`, radius 20 — RoleBadge grammar), mono sub-line with the fingerprint (`·` middot preserved) + `did:`-style value, "⧉ Copy DID" outline; hairline meta row `ed25519` + membership count (+ created-date only if backed).
- **Device rows:** white cards, trust badge (existing "● trusted" etc.), mono `enrolledAt` + fingerprint, rename (aria-label "Device name"), butler-pin checkbox. "this device" → sage pill.
- **Footer:** "＋ Add another device" sage. Hosted backup/mint modals: Commons pass, `Modal` reused as-is.
- **Cool trust-blue device-chip (`#e8eef0`, no dark token)** → remap to a neutral `--bg-tertiary` surface (per 609 §D1 trust-blue-rejection).
- **Preserve byte-identical (heavy pin set — `DevicesPanel.test.ts`):** `/bind this device/i`, `/join existing identity/i`, mint modal `/will create your owner identity/i` + confirm `/^create owner identity/i` (anchored — label must START "Create owner identity") + `/cancel/i`; `getByText('zeblith')`, fingerprint `/a4f1·c823/i` (**`·` middot**), `/back up owner identity/i`; `/this device/i`, `/trusted/i`, `/aa11·bb22/i`; footer `/add another device/i`; rename `/rename/i` + textbox `/device name/i` + Save/Cancel (asserts `saveDeviceLabel`, not `saveProfile`); backup modal `role="dialog"`, inputs **"Passphrase"** / **"Confirm passphrase"** / **"Comment"**, `/save backup/i`, errors `/do not match/i` `/at most 256 bytes/i` `/at least 12 characters/i`, success echoes `ExportInfo.path` verbatim; butler checkbox name `/set {name} as always-on butler/i` → `invoke('set_butler_pin', { deviceId: vkHex })`. `<style>` budget 0.

### 2.5 — `BackupReminderBanner.svelte`
Commons **clay banner** (never modal): `--gov-clay-soft` strip, clay-tan `color-mix` border, 🔑, `--gov-clay-deep` title + body, **clay-filled** "Back up now", clay "✕" dismiss.
- **Tokenize** the raw `background: #4a3a1a` (line 147) → a clay token / `color-mix`, **and delete that entry from `src/style-token-allowlist.json`** (ratchet down; regenerate with `UPDATE_STYLE_TOKEN_ALLOWLIST=1`). Tokenize `.error { color: crimson }` → `--fg-error`.
- **Preserve byte-identical:** `role="status"`; testids `backup-reminder-banner`, `backup-reminder-backup-now`, `backup-reminder-dismiss`, `backup-reminder-passphrase`, `backup-reminder-save`, `backup-reminder-error`; the owner-scoped visibility matrix; dismiss → `sessionStorage['harmony.onboarding.backupBannerDismissed:owner-<id>']='true'`; save → `localStorage['…recoveryArtifactBackedUp:owner-<id>']='true'`. Copy is free (no test pins it) — keep the honest **days-since** figure; soften any "new community since then" copy to the days-only claim (§0.6).

### 2.6 — `PairingInviter.svelte` + `PairingJoiner.svelte` + `OwnerRestoreWizard.svelte` (extend, don't skip)
Commons chrome on the shared-`Modal` bodies; the SAS display (`--font-mono`, letter-spaced) is the visual centerpiece — restyle, don't restructure.
- **`PairingInviter`:** preserve `invoke('start_inviter_pairing', {displayName})`, SAS regex `/987\s*654|987654/` (**keep digits as text, only whitespace between triplets**), peer `displayName` verbatim, `/cancel/i`→`cancel_pairing`, `aria-labelledby="invite-heading"` (id pinned).
- **`PairingJoiner`:** preserve label **"Give this device a name"** (`/give this device a name/i`), `/start pairing/i`→`start_joiner_pairing`, SAS `/012\s*845|012845/`, `aria-labelledby="join-heading"` (id pinned), complete copy `/this device is now part of the owner identity/i`, and the Escape routing (terminal `complete`→`onComplete`, no `cancel_pairing`; non-terminal→`onClose`+`cancel_pairing`).
- **`OwnerRestoreWizard`:** Commons chrome; preserve testids `owner-restore-words/-continue/-confirm/-typed-confirm/-error`, the 24-word gate, `preview_owner_mnemonic_identity`, the same-owner typed-prefix (8-char) → `force:true` mechanic, `restore_owner_mnemonic_from_words {words, force}`, and `/different identity/i`. This is *input-only* — no phrase display (consistent with §0.1).

### Read-only seams (NO changes this PR)
`src/lib/owner-gate.ts`, `owner-service.ts`, `pairing-service.ts`, `owner-restore-logic.ts`, `recovery-policy.ts`, `onboarding-backup-flags.ts`, `types/onboarding.ts`; `Modal.svelte`, `HarmonyMark.svelte`, focus-trap actions; **all** `src-tauri/**` (no backend), the `generate_handler!` list, every IPC contract, and `owner-gate.test.ts` (pure-logic pin). No cross-repo / harmony-core changes.

---

## §3 — Invariants
- Frontend gates: `npx tsc --noEmit && npx vitest run` (repo root).
- Svelte 5 runes (`$props/$state/$derived/$effect`).
- **Budget-0 tokens:** zero new hex/rgb/named colors in `<style>` blocks (`color-mix(in srgb, var(--x) N%, …)` and `transparent` allowed). `commons-hex-guard` stays empty. Allowlist only ratchets **down** — §2.5 removes the one `BackupReminderBanner` entry.
- **Clay containment:** clay tokens appear only in `WelcomeModal` stage `backup` (+ its pip) and `BackupReminderBanner`. Sage everywhere else.
- Preserve every test-pinned `data-testid`, accessible name/label, aria id, the `·` fingerprint middot, SAS triplet-whitespace, and the WelcomeModal redaction invariant — byte-identical.
- One PR, commit per task, no worktrees, branch off latest `origin/main`.

---

## §4 — Testing
Preserve all pins above (each surface's existing test file continues to pass unchanged). Add: `WizardProgress` unit tests (pip count / elongated-active / clay tint); a `WelcomeModal` assertion that the redaction invariant still holds after restyle and that the clay backup stage renders the passphrase field (not a phrase grid); a `BackupReminderBanner` assertion that the amber literal is gone (guard) and the clay banner still shows/dismisses. No new IPC → no service-layer test changes.

---

## §5 — SDD task shape (6 tasks, one PR)
1. **`WizardProgress.svelte`** primitive + unit tests (§2.1).
2. **`WelcomeModal`** Steps 1–3 restyle + `WizardProgress` mount; preserve testids + redaction (§2.2).
3. **Redeem + Done + startup-error** — `RedeemInviteDialog`, `NamePromptModal`, App startup-error overlay Commons pass (§2.3).
4. **`DevicesPanel`** Commons restyle of the three real sections + hosted modals (§2.4).
5. **`BackupReminderBanner`** clay restyle + tokenize `#4a3a1a`/crimson + allowlist ratchet-down (§2.5).
6. **Pairing + Restore** chrome — `PairingInviter`/`PairingJoiner`/`OwnerRestoreWizard` (§2.6). Last so it is trivially peelable if the PR needs trimming.

Task boundaries: each ends with an independently testable deliverable; a reviewer could reject any one while approving its neighbors. T1 precedes T2 (T2 consumes `WizardProgress`); T3–T6 are order-independent.

---

## §6 — Follow-up ticket (deferred, cross-stack)
File after the PR opens: **"Owner recovery phrase in the GUI — export IPC + Step-3 phrase display + phrase-restore parity."** Scope: a `#[tauri::command]` wrapping the existing CLI-only `export_owner_mnemonic_words_with_keychain`; a deliberate, redaction-invariant-aware relaxation to display the 24-word phrase in Step 3 (grid + copy) alongside the file/keychain; and reconciling `OwnerRestoreWizard` so a user who *saw* their phrase can restore from it symmetrically. Parent epic ZEB-603. Records why ZEB-610 shipped the file-based subset (this §0.1).
