# ZEB-494 — First-run onboarding: "join an existing device" at the WelcomeModal gate

- **Issue:** [ZEB-494](https://linear.app/zeblith/issue/ZEB-494) (parent: ZEB-169 Track A multi-device identity)
- **Status:** design approved 2026-06-18
- **Predecessors:** ZEB-197 (pairing UI), ZEB-492 (KeyTree distribution at pairing), ZEB-338 (WelcomeModal hard gate)

## Problem

The multi-device backend is built end-to-end — owner→device binding, SAS pairing
(`PairingJoiner`/`PairingInviter`, ZEB-197), fleet KeyTree distribution at pairing
(ZEB-492), and the fleet-sync substrate. But the **front door is a dead end**:
`WelcomeModal` (ZEB-338) is a first-run hard gate that mounts iff `start_node`
reports `hasOwnerIdentity=false`, and its only exit is a successful **mint**
("Create my identity"). The `PairingJoiner` "Join existing identity" path lives
only in `DevicesPanel`'s empty state, which is unreachable behind the gate. So a
user setting up their **second** device is forced to mint a throwaway new
identity — the entire multi-device story is unreachable at the moment a user
would use it.

## Key finding — pairing works pre-identity (this is frontend-only)

The pairing transport is **not** gated on owner identity:

- `start_node` installs the transport-level `publish_tx` on `NodeState`
  unconditionally in the event-loop-spawn arm (`lib.rs:8046`), independent of
  whether an owner identity loaded. (The app shell + Network Health already
  render behind the gate, which proves the event loop runs with no identity.)
- The pairing state machine spawns from that `publish_tx` after the event loop
  reports ready (`lib.rs:8773–8804`), also outside any `if let Some(seed)` block.
- `start_joiner_pairing_inner` (`pairing_commands.rs:72`) generates a **fresh**
  signing key and only needs `require_pairing_handle` — no existing identity.

Therefore the joiner pairing IPCs all function at `hasOwnerIdentity=false`. This
work is **frontend-only**: no Rust changes.

## Design

### Unit 1 — `WelcomeModal.svelte`: add a `'joining'` stage

The explain pane gains a secondary action alongside the primary "Create my
identity":

```text
Already have Harmony on another device? Add this one to your existing identity.

  [ Create my identity ]            ← primary (most first-runs)
  [ Join another of my devices ]    ← secondary
```

- Mint stays the primary action; join is a clear secondary. The now-false
  *"Single-device only in v0.1.0-alpha"* copy is replaced.
- Selecting "Join another of my devices" sets `stage = 'joining'`.
- When `stage === 'joining'`, render `<PairingJoiner>` **instead of** the
  WelcomeModal backdrop/content (not nested inside it), so there is exactly one
  modal/backdrop on screen. The hard-gate property is preserved: the only ways
  out of the gate remain (a) a successful mint, or (b) a completed enrollment —
  cancelling pairing returns to the explain pane, it does not dismiss the gate.

### Unit 2 — `PairingJoiner.svelte`: add an optional `onComplete` prop

`PairingJoiner` currently exposes only `onClose`, fired identically for the
`complete`, `failed`, and user-cancel outcomes. The onboarding gate must
distinguish a **completed enrollment** (→ load the new identity) from a
cancel/fail (→ stay on the gate). Checking `get_owner_state` after close is
unreliable: the running node booted with no identity, so its in-memory
`NodeState` still reports none even though enrollment was written to disk.

Add an **optional** `onComplete` callback, invoked from the `complete` state's
button (`onclick={onComplete ?? onClose}`). Backward-compatible:

- `DevicesPanel` passes only `onClose` → unchanged (`refresh()` on close).
- `WelcomeModal` passes `onComplete` → triggers the gate exit.

### Unit 3 — Gate exit on completed enrollment

On `onComplete`, `WelcomeModal` calls **`location.reload()`**.

Rationale: enrollment installs `owner_state.cbor` + the distributed fleet
KeyTree on disk, but a cert-only device builds its fleet engines from that
KeyTree only on a fresh boot (the ZEB-492 / s7 "B2 boots its engines after
relaunch" reality). A reload re-runs `start_node`, which loads the installed
identity (`hasOwnerIdentity=true` → gate no longer mounts) and builds the fleet
engines. This is more correct than the `onMinted` hot-flip, which only works
because mint installs a **seed-holder** in-place within the same boot.

### Cancel / failure

`PairingJoiner`'s `onClose` (user cancel, SAS mismatch, or close-after-fail)
sets `stage = 'explain'`. No identity was installed, so the gate stays; mint
remains available.

## Out of scope

- Cross-WAN pairing (the joiner discovers the inviter via Zenoh mDNS on the LAN;
  cross-internet pairing remains ZEB-197's deferred v3).
- Any change to the inviter flow, `DevicesPanel`, or the pairing backend.
- Syncing owner display name / device labels (separate gap, separate ticket).

## Testing

- **Vitest component test** (`WelcomeModal`): the explain pane renders both
  "Create my identity" and "Join another of my devices"; clicking Join mounts
  `PairingJoiner` (and hides the welcome content); a `PairingJoiner` `onClose`
  returns to the explain pane (gate not dismissed).
- The full pair → reload path is the manual / two-machine validation (a live SAS
  exchange + `location.reload()` can't be unit-tested).
- Gates: `npx tsc --noEmit` + `npx vitest run` clean.

## Acceptance

At first launch on a fresh device, the user can choose "Join another of my
devices", complete SAS pairing against another of their online devices, and land
in the app enrolled under their existing owner identity — devices visible in
both DevicesPanels, synced datasets (notes/communities/DMs) present — with no
throwaway-identity detour.
