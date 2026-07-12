# ZEB-668 S6 — "Replace this device" Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Seed-holder **Replace…** action on a sibling device row: typed-confirm
revoke (reason locked to `decommissioned`) → immediately launch inviter pairing
→ on completion, carry the old row's fleet petname to the successor via
`set_device_petname` (spec §7).

**Architecture:** Pure frontend composition — zero Rust changes. S2 built the
revoke (`revoke_device`, master path bumps the S5 fleet epoch inline), the
pairing stack exists (`start_inviter_pairing`, `pairing-state-changed` event,
`Complete { deviceIdHex }`), and S4 built `set_device_petname` (64-hex vk key,
live-disk enrolled-set union per ZEB-491 — no enrollment race). The one real
gap: nothing calls `get_pairing_state` after Complete, so the disk→resident
trust fold (`get_pairing_state_inner`'s Complete branch) never runs and the
panel — which renders from the RESIDENT doc — can't see the successor. T1
closes that for ALL pairings, not just replace.

**Tech stack:** Svelte 5 runes, vitest + @testing-library/svelte, Tauri invoke.

## Global constraints

- JS invoke args are camelCase (`deviceVkHex`), Rust params snake_case.
- Error extraction: `extractError` (owner-service re-export) everywhere.
- Severity tiering: replace REMOVES first → typed-confirm (same tier as
  remove); the removal is immediate and survives pairing abandonment — copy
  must say so.
- §7 "master rotation out of scope" must land in UI-adjacent copy → the
  replace dialog states the owner identity/master key are unchanged.
- Honesty rule: affordances render only where the IPC can succeed →
  Replace… is gated `{#if state.canBackUp}`, sibling rows only.
- Petname key is `deviceVkHex` (64-hex), NEVER `deviceId` (32-hex);
  Complete reports `deviceIdHex` (32-hex) → map via refreshed device list.
- Gates: `npx vitest run` + `npx tsc --noEmit`; `scripts/test-select
  --context task` for the (empty) Rust surface; no Rust diff → full nextest
  sweep is CI's (state this in the PR body).

---

### Task 1: Fold-on-complete + `onComplete` seam (PairingService + PairingInviter)

**Files:**
- Modify: `src/lib/pairing-service.ts` (add `refreshSnapshot`)
- Modify: `src/lib/components/PairingInviter.svelte` (add `onComplete` prop + effect)
- Test: `src/lib/pairing-service.test.ts`, `src/lib/components/__tests__/PairingInviter.test.ts`

**Interfaces:**
- Produces: `PairingService.refreshSnapshot(): Promise<void>`;
  `PairingInviter` prop `onComplete?: (deviceIdHex: string) => void`, fired
  once per mount after the fold attempt.

- [ ] **Step 1: failing tests.** pairing-service.test.ts: `refreshSnapshot`
  invokes `get_pairing_state`, applies the snapshot, fires `onChange`.
  PairingInviter.test.ts: mount with snapshot `{ kind: 'complete',
  deviceIdHex: 'cc'.repeat(16) }` and an `onComplete` spy → spy called once
  with `'cc'.repeat(16)`; `get_pairing_state` invoked twice (init + fold);
  a second state flush does not re-fire the spy.
- [ ] **Step 2: implement.** pairing-service.ts:

```ts
/** Re-fetch the backend snapshot. On Complete the backend's
 *  get_pairing_state_inner ALSO folds the freshly-persisted enrollment
 *  from disk into the resident trust doc (ZEB-668 S1) — the Devices
 *  panel renders from that doc, so this call is what makes the new
 *  device visible without a restart. */
async refreshSnapshot(): Promise<void> {
  this.state = await invoke<PairingState>('get_pairing_state');
  this.onChange?.();
}
```

  PairingInviter.svelte: prop `onComplete?: (deviceIdHex: string) => void`;
  effect (fires once, fold is fail-open):

```ts
let completeNotified = false;
$effect(() => {
  if (state.kind === 'complete' && !completeNotified) {
    completeNotified = true;
    const deviceIdHex = state.deviceIdHex;
    void (async () => {
      try {
        await svc.refreshSnapshot();
      } catch {
        // Fail open: pairing succeeded and disk is durable; the next
        // get_pairing_state poll or boot retries the fold.
      }
      onComplete?.(deviceIdHex);
    })();
  }
});
```

- [ ] **Step 3: run** `npx vitest run src/lib/pairing-service.test.ts
  src/lib/components/__tests__/PairingInviter.test.ts` → PASS.
- [ ] **Step 4: commit** `feat(pairing): fold enrollment + onComplete seam at inviter completion`.

### Task 2: RemoveDeviceDialog `mode="replace"`

**Files:**
- Modify: `src/lib/components/RemoveDeviceDialog.svelte`
- Test: `src/lib/components/__tests__/RemoveDeviceDialog.test.ts`

**Interfaces:**
- Produces: optional prop `mode?: 'remove' | 'replace'` (default `'remove'`
  — every existing call site unchanged). Replace mode: title
  `Replace {deviceName}?`, reason radios hidden (locked `decommissioned`),
  confirm label `Remove & continue`, copy states removal-first semantics +
  owner identity/master key unchanged (§7 out-of-scope note).

- [ ] **Step 1: failing tests.** Replace mode renders `Replace X?` title, no
  radio inputs, `Remove & continue` button disabled until `X` typed, then
  `onConfirm` called with `'decommissioned'`; copy mentions master key
  unchanged; remove mode unchanged (existing tests stay green).
- [ ] **Step 2: implement.** Add `mode = 'remove'` to props. Template:
  title branches on mode; replace mode swaps the lead paragraph for
  removal-first + pairing-next copy and an identity-unchanged line, wraps
  the reason fieldset in `{#if mode === 'remove'}`, confirm button label
  branches, `onclick={() => onConfirm(mode === 'replace' ? 'decommissioned' : reason)}`.
- [ ] **Step 3: run** the dialog test file → PASS.
- [ ] **Step 4: commit** `feat(devices): replace mode for the remove dialog`.

### Task 3: DevicesPanel Replace… flow + petname carry

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Test: `src/lib/components/__tests__/DevicesPanel.test.ts` (new S6 describe)

**Interfaces:**
- Consumes: T1 `onComplete`, T2 `mode="replace"`,
  `setDevicePetname(deviceVkHex, petname)`, `svc.revoke(vk, 'decommissioned')`.

- [ ] **Step 1: failing tests** (S6 describe, keyed `mockImplementation` by
  command name; fixture = seed-holder + sibling with petname):
  1. Replace… on sibling row iff `canBackUp`; never on self row.
  2. Full flow: Replace… → typed-confirm → `revoke_device` with
     `reason: 'decommissioned'` → `start_inviter_pairing` invoked (modal
     open) → snapshot-complete path fires `onComplete` →
     `set_device_petname` called with the SUCCESSOR's `deviceVkHex` and the
     OLD petname → refresh.
  3. Old row has no petname → no `set_device_petname` call.
  4. Pairing closed before complete → pending cleared, no petname write.
- [ ] **Step 2: implement.** State + handlers (after the S2 remove block):

```ts
// ZEB-668 S6: replace = typed-confirm revoke (reason locked to
// "decommissioned") + immediately launch inviter pairing; the old row's
// fleet petname carries to the successor at pairing completion.
let replaceTarget = $state<DeviceView | null>(null);
let replaceInFlight = $state(false);
let replaceError = $state<string | null>(null);
// Captured at confirm; null = nothing to carry. Cleared by the carry
// attempt or by closing the pairing modal un-completed (the replace
// honestly degrades to a plain remove — the revoke already landed).
let pendingCarryPetname = $state<string | null>(null);
let carryError = $state<string | null>(null);

async function handleReplaceConfirm() {
  if (!replaceTarget || replaceInFlight) return;
  replaceError = null;
  replaceInFlight = true;
  try {
    const carry = replaceTarget.petName?.trim() || null;
    await svc.revoke(replaceTarget.deviceVkHex, 'decommissioned');
    await svc.refresh();
    pendingCarryPetname = carry;
    carryError = null;
    replaceTarget = null;
    inviterOpen = true;
  } catch (e) {
    replaceError = extractError(e);
  } finally {
    replaceInFlight = false;
  }
}

async function handlePairingComplete(deviceIdHex: string) {
  const carry = pendingCarryPetname;
  pendingCarryPetname = null;
  try {
    await svc.refresh(); // successor row needed for deviceId → vk map
  } catch {
    /* best-effort; owner-devices-updated re-refreshes */
  }
  if (!carry) return;
  const successor = state?.devices.find((d) => d.deviceId === deviceIdHex);
  if (!successor) {
    carryError = `Couldn't carry the name "${carry}" to the new device — rename it manually.`;
    return;
  }
  try {
    await setDevicePetname(successor.deviceVkHex, carry);
    await svc.refresh();
  } catch (e) {
    carryError = `Couldn't carry the name "${carry}" to the new device — rename it manually. (${extractError(e)})`;
  }
}
```

  Template: sibling branch gains `Replace…` (class `remove-btn`, between
  Rename and Remove…, inside the same `{#if state.canBackUp}`); dialog mount
  `{#if replaceTarget}` with `mode="replace"`, `isSelf={false}`,
  `isSeedHolder={false}`, `busy={replaceInFlight}`, `error={replaceError}`;
  inviter mount gains `onComplete={handlePairingComplete}` and its onClose
  clears `pendingCarryPetname`; `{#if carryError}` alert beside the list
  errors.
- [ ] **Step 3: run** DevicesPanel test file → PASS; then full
  `npx vitest run` + `npx tsc --noEmit` → clean.
- [ ] **Step 4: commit** `feat(devices): ZEB-668 S6 replace-device flow with petname carry-over`.

## Self-review notes

- Spec §7 coverage: typed-confirm ✓ (T2), reason pre-set decommissioned ✓
  (T2 locks it), immediate inviter launch ✓ (T3), petname carry via
  set_device_petname ✓ (T3), master-rotation-out-of-scope UI-adjacent ✓
  (T2 copy).
- Revoke-before-pairing ordering is load-bearing: the master revoke bumps
  the fleet epoch FIRST, so `start_inviter_pairing` reads
  `fleet_current_epoch = N+1` and the successor is sealed into the
  post-revocation window, never the compromised one.
- Abandonment semantics: revoke is immediate and irreversible by design —
  dialog copy says so; closing pairing early leaves an honest plain remove.
