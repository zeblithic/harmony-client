# ZEB-650 · Commons G deferred data-backed elements (design)

> Covers ZEB-650's five deferred items across **three sequential PRs** (slices 1–3)
> plus the bundled **ZEB-659** dev-flag gate (slice 1). Item 3c (rotation/revoke/
> last-seen) spun off to **ZEB-668**. Approvals (Jake, 2026-07-09): slicing; slice-2
> security posture = Option A (deliberate reveal, Reticulum idiom).
> Prior spec: `2026-07-06-zeb-610-commons-g-onboarding-identity-design.md` (§0/§6).

## §0 — Exploration corrections (what the ticket got stale)

Verified against code 2026-07-09 (code-map explorer):

- **0.1 Restore symmetry already exists.** `OwnerRestoreWizard` already has the full
  GUI mnemonic-paste restore (`preview_owner_mnemonic_identity` →
  `classifyRestore` tier → typed-confirm → `restore_owner_mnemonic_from_words`).
  Ticket item 1's "reconcile OwnerRestoreWizard" bullet is **done** — slice 2 is
  export + display only.
- **0.2 Words-in-webview precedent exists.** `export_mnemonic_words`
  (`identity_commands.rs:559`) already returns the 24 **Reticulum node** words to the
  renderer, displayed via `IdentityPanel`'s `mnemonicReveal` step
  (`{words, revealed, storedSafely}` — blur-gate + "stored safely" checkbox,
  `IdentityPanel.svelte:184-214`). Slice 2 mirrors this state machine for the
  **owner** words. What remains genuinely new: it is the first command returning
  *owner* seed material to the webview → §3's invariant re-statement.
- **0.3 Invite preview needs no protocol change.** The `harmony://invite/` URL
  payload (`CommunityInvitePayload`, `community_invite.rs:108`) already carries
  `community_name`, `is_invite_only`, `expires_at`, and the Ed25519-signed
  `InviteToken` (`inviter`, `minted_at`, `expires_at`; signed body =
  `canonical_invite_token_bytes`, `:1878`). Preview = local decode + verify. There
  is no resolve-without-joining IPC today; `redeem_invite` joins immediately.
- **0.4 No owner created-at exists.** `OwnerState` CRDT has no mint timestamp; the
  honest proxy is the earliest `DeviceView.enrolledAt` (already on the wire).
  `Space.created_at` is *community creation*, not join time — there is no join
  timestamp anywhere.
- **0.5 Real bug found:** DevicesPanel's `commitBackup` never calls
  `markRecoveryBackedUp`, so a Devices-panel backup does not clear the
  `BackupReminderBanner`. (Banner + WelcomeModal do call it.) Fixed in slice 1.
- **0.6 "N days since backup" figure:** the flags (ZEB-587) are booleans; no
  timestamp is stored anywhere. Spec §0.6 of ZEB-610 was wrong on this point; the
  ticket is right. Slice 1 adds timestamps.
- **0.7 Avatar is identicon, not initials.** `Avatar.svelte` requires an `address`
  and renders `generateIdenticon(address, size)` when no `avatarUrl`. At
  NamePromptModal time the owner **is** minted, so a real identicon is available —
  better than the mock's initials chip.

## §1 — Slice map

| Slice | PR | Contents | New IPC |
|---|---|---|---|
| 1 | `zeb-650-659-slice1-onboarding-meta` | DevicesPanel meta row (3a/3b) · backup timestamps + banner N-days + `commitBackup` gap fix (4) · NamePromptModal identicon chip (5) · **ZEB-659 dev-flag gate** | none |
| 2 | own branch/PR | Owner recovery phrase: `export_owner_mnemonic_words` command + `OwnerPhraseReveal.svelte` in WelcomeModal Step 3 + DevicesPanel | 1 |
| 3 | own branch/PR | `preview_invite` command + RedeemInviteDialog preview card (§0.4 shape from ZEB-610) | 1 |

Sequential 1 → 2 → 3, one PR per repo at a time. Dropped entirely: the mock's
"new community since then" delta — an identity-only backup does not go stale when
communities are joined (memberships live in the OwnerState CRDT, not the recovery
artifact), and no join timestamps exist to compute it honestly (§0.4).

## §2 — Slice 1 (pure frontend)

### 2.1 DevicesPanel meta row (items 3a + 3b)

In the owner header section (beside the `● self-sovereign` badge,
`DevicesPanel.svelte:367`), add a muted meta row of three facts:

- **`ed25519`** — static label (true invariant: all identity keys are Ed25519).
  Mono/pill grammar matching the ss-badge.
- **`First device enrolled <date>`** — `min(state.devices[].enrolledAt)` formatted
  as a local date. Label names exactly what it measures (a mnemonic restore
  re-enrolls and resets it — "Member since" would over-claim).
- **`N communities`** — count of `invoke('list_owner_communities', {})` rows
  (existing IPC, `community-service.ts:268`), fetched once on mount alongside
  `get_owner_state`. On IPC failure: omit the fact (render nothing) — never a
  fabricated 0.

All three render only when `state !== null`. New testids:
`devices-meta-keytype`, `devices-meta-enrolled`, `devices-meta-communities`.

### 2.2 Backup timestamps + banner day count (item 4)

`onboarding-backup-flags.ts` — two additive owner-scoped **localStorage** keys via
the existing `ownerKey()` idiom:

- `harmony.onboarding.backupSkippedAt` — stamped (`Date.now()`) inside
  `markBackupSkipped`.
- `harmony.onboarding.recoveryBackedUpAt` — stamped inside `markRecoveryBackedUp`.

New reads: `backupSkippedAtMs(ownerId): number | null`,
`recoveryBackedUpAtMs(ownerId): number | null`, and
`daysSinceBackupSkipped(ownerId, nowMs = Date.now()): number | null`
(whole days, floor). Backward compatible: existing users have booleans without
timestamps → reads return `null`, all new copy degrades to current behavior. No
migration. Visibility predicate `isBackupReminderVisible` is **unchanged**.

Surfaces:

- **BackupReminderBanner:** when `daysSinceBackupSkipped ≥ 1`, copy becomes
  "Your identity hasn't been backed up — you skipped backup N day(s) ago."
  (`null` or 0 → current copy unchanged). Testid `backup-reminder-days`.
- **DevicesPanel backup section:** "Last backed up `<date>`" muted line when
  `recoveryBackedUpAtMs` is non-null; omitted otherwise (testid
  `devices-last-backed-up`).
- **Gap fix (§0.5):** `commitBackup` calls `markRecoveryBackedUp(ownerId)` at its
  success point, exactly mirroring `BackupReminderBanner.svelte:102` (capture the
  initiating owner before the awaits, per the ZEB-587 pattern).

### 2.3 NamePromptModal identicon chip (item 5)

New prop `ownerIdHex: string | null` (App.svelte passes the resolved owner id it
already holds). When non-null, render a preview chip between the input and
actions: `Avatar` (`address=ownerIdHex`, `displayName=name.trim() || 'Anonymous'`,
`size=40`) + the live-typed name (falling back to "Anonymous") + a
`● self-sovereign` sub-line reusing the DevicesPanel ss-badge grammar. Pure
presentation of data that exists at this moment; no persistence changes. Testid
`name-prompt-chip`. `null` owner (should not happen post-mint, but the prop is
nullable) → no chip.

### 2.4 ZEB-659 — Network Viz dev-flag gate

`NavPanel.svelte`: new prop `showNetworkViz: boolean = import.meta.env.DEV`.
The "Network Viz" button (`:454-461`) renders only when `showNetworkViz`;
`openNetworkWindow` (`:206`) gets a matching early-return guard. Prod builds
(`DEV === false`) hide the mock-topology window entirely; `tauri dev` keeps it.
Tests pass the prop explicitly (both states); the default expression itself is
not unit-tested (build-time constant).

## §3 — Slice 2: owner recovery phrase (Option A)

### 3.1 New command — the only new owner-seed IPC

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerMnemonicDto {
    pub words: Vec<String>,   // 24 BIP39 words
    pub owner_id: String,     // hex; UI cross-checks against current owner
}

#[tauri::command]
async fn export_owner_mnemonic_words(/* state */) -> Result<OwnerMnemonicDto, String>
```

Thin wrapper over the existing `export_owner_mnemonic_words_with_keychain`
(`recovery_cli.rs:239`), resolving `identity_dir` + keychain exactly as the
sibling owner commands do (ZEB-428: tests exercise an `*_inner` seam with
`keychain: None` + `HARMONY_PASSPHRASE`; never `KeychainStore::new()` in
test-reachable code). Inherits the CLI fn's three gates: minted; `master_seed`
present (wiped → "backup no longer possible" error); seed↔owner-id invariant.
Words come back `Zeroizing`-wrapped internally; only the word list crosses IPC —
never seed bytes, never hex.

### 3.2 `OwnerPhraseReveal.svelte` (shared component)

Mirrors the `IdentityPanel` `mnemonicReveal` state machine, plus a fetch gate:

1. **Collapsed:** a "Reveal recovery phrase" affordance + warning copy
   ("Anyone who sees these 24 words controls your identity. Make sure no one is
   watching."). **No IPC call has happened yet.**
2. **Click-confirm** (single explicit click on the warning's confirm button —
   click-confirm tier, not typed-confirm): invoke `export_owner_mnemonic_words`.
   Error → inline message (wiped-seed case reads naturally). On success,
   **cross-check `dto.ownerId` against the current `OwnerStateView.ownerId`**;
   mismatch → error, words discarded, nothing rendered.
3. **Revealed:** numbered 24-word grid, blurred until hover/hold per the
   IdentityPanel idiom, + Copy button.
4. **"I've written these words down" checkbox** → calls
   `markRecoveryBackedUp(ownerId)` (which now also stamps `recoveryBackedUpAt`,
   §2.2). Mere reveal does **not** count as backed up.
5. **Teardown:** on collapse/unmount/modal-close, word state is reset to `[]`
   (best-effort — JS strings cannot be zeroized; the invariant is about DOM
   exposure and lifetime, documented in the component header).

Mount points: **WelcomeModal `backup` stage** (below the existing
file/passphrase flow, as the "or write it down" alternative — always available
there, the seed was just minted) and **DevicesPanel** via a "View recovery
phrase" button beside "Back up" (both gated on `state.canBackUp`), opening a
small modal hosting the component.

### 3.3 Redaction invariant — deliberate re-statement

- The existing WelcomeModal test invariant (`container.innerHTML` never contains
  a `[0-9a-f]{32,}` run) stays **byte-identical and still passes** — BIP39 words
  are not hex. But `OwnerMnemonicDto.ownerId` is exactly 32 hex chars (16 bytes),
  enough to trip that regex: it exists only for the cross-check and must never be
  rendered by the reveal component. Test pins this.
- New stated invariant (component header + tests): *owner seed material may
  exist in the webview only as BIP39 words, only inside `OwnerPhraseReveal`,
  only after an explicit user reveal action, and never persists past the
  component's visible lifetime. The export IPC fires only on that action —
  never on mount.*

## §4 — Slice 3: invite preview

### 4.1 `preview_invite` command

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitePreviewDto {
    pub community_name: String,
    pub is_invite_only: bool,
    pub inviter_verified: bool,          // InviteToken sig valid over canonical bytes
    pub inviter_fingerprint: Option<String>, // xxxx·xxxx middot idiom, from token.inviter
    pub inviter_display_name: Option<String>, // only if locally resolvable; else None
    pub expired: bool,                   // vs payload.expires_at / token expiry
}

#[tauri::command]
fn preview_invite(url: String) -> Result<InvitePreviewDto, String>
```

Pure local computation: decode `CommunityInvitePayload` with the **existing**
deserializer (malformed → `Err`), verify the `InviteToken` signature when
present, evaluate expiry. **Mints nothing, joins nothing, no network.** The
two-IPC TOCTOU rule doesn't apply: preview and redeem both derive
deterministically from the same immutable URL string the user pasted — there is
no server-side state to rebind. Honesty per ZEB-610 §0.4: **no member/channel
counts** (the token signature does not commit to the payload's name/snapshot;
the ✓ is scoped to the inviter authorization, and the card copy reflects that).

### 4.2 RedeemInviteDialog card

When the URL passes the existing `canSubmit` format check, debounce ~300 ms →
`preview_invite`. Card above the actions (testid `redeem-preview-card`):
community name headline; `✓ invite signature verified — from <displayName |
fingerprint>` when `inviterVerified` (fallback order: display name, else
fingerprint); a neutral "signature not verifiable" line otherwise (not an
error — old/foreign invites may lack tokens); `invite-only` chip when set.
`expired: true` → inline notice + submit disabled. Preview `Err` → keep the
dialog usable, show "This invite link looks invalid" only (redeem's own error
path remains authoritative). Preview failures never block cancel.

## §5 — Testing

- **Slice 1:** flags — timestamps stamped, owner-scoped keys, `null` reads for
  legacy booleans, `daysSinceBackupSkipped` day math (injected `nowMs`);
  banner — N-days copy at 0/1/7 days + legacy-null unchanged; DevicesPanel —
  meta row facts, IPC-failure omission, `commitBackup` marks backed-up (the gap
  regression test), last-backed-up line; NamePromptModal — chip renders
  identicon + live name + null-owner absence; NavPanel — `showNetworkViz`
  true/false render + guard.
- **Slice 2:** Rust — inner-seam test (mint fixture, `keychain: None`,
  `HARMONY_PASSPHRASE`): 24 words + ownerId round-trip; wiped-seed error path.
  TS — no words in DOM pre-reveal; IPC fires only after confirm; ownerId
  mismatch discards; checkbox → `markRecoveryBackedUp`; teardown clears; the
  existing WelcomeModal hex-redaction test unchanged; new test that
  `dto.ownerId` never renders.
- **Slice 3:** Rust — decode/verify round-trip from a minted invite; tampered
  sig → `inviter_verified: false`; expired flag; **asserts no Join minted / no
  state mutation**. TS — debounce, card fields, expired-disables-submit,
  invalid-URL copy.

## §6 — Invariants (all slices)

- Frontend gates: `npx tsc --noEmit && npx vitest run`. Rust gates: fmt +
  clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  + targeted nextest; full `--workspace --all-targets` sweep is CI's.
- Svelte 5 runes; budget-0 color tokens (`commons-hex-guard` stays empty); all
  existing testids/aria/copy pins preserved byte-identical.
- New DTOs `#[serde(rename_all = "camelCase")]`; IPC params snake_case (Rust) /
  camelCase (TS callers).
- ZEB-428: any keychain touch goes through injectable seams; no
  `KeychainStore::new()` reachable from tests.
- One PR per slice, sequential; commit per task; no worktrees; branch off
  latest `origin/main`.
