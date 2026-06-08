# ZEB-342 — Trust-bootstrap liveness (fresh-mint sole device shows "● refused")

**Status:** Design approved 2026-06-08
**Linear:** ZEB-342 (High, Bug)
**Repos:** `harmony` (upstream `harmony-owner` crate) + `harmony-client`

## Problem

On a fresh first-run mint, the Devices panel shows the current and **only** device with a
red **"● refused"** trust badge. A brand-new owner's sole device should read trusted. This
is alarming on a clean install and reads as broken.

## Root cause (confirmed against `harmony-owner` @ `04449d6`)

The ticket hypothesized "zero vouches → Refused." The real mechanism is a freshness gate that
fires earlier:

`trust::evaluate_trust` (`crates/harmony-owner/src/trust.rs:23`) evaluates in this order:

1. revoked → `Refused(Revoked)`
2. not enrolled → `Refused(NotEnrolled)`
3. `active = state.active_devices(now, active_window)` — a device is **active only if it has a
   `LivenessCert`** within the window (`state.rs:400`, `.filter(|id| liveness.get(id) … )`).
4. `any_fresh = (fresh liveness from an active signer) OR (fresh vouch)`; if `!any_fresh` →
   `Refused(StaleTrustState)`.
5. single-device case `active_set == {target}` → `Full` — **never reached** when step 4 fails.

The decisive finding: **the client never publishes a liveness cert.** `mint_owner`
(`mint.rs:80`) enrolls device #1 but adds no liveness, and a sweep of the entire client
backend finds zero `add_liveness` / `LivenessCert` calls against owner-state. So
`loaded.state.liveness` is always empty → `active_devices` returns `[]` → `any_fresh = false`
→ **every** device, including the sole fresh-mint one, returns `Refused(StaleTrustState)`.

The badge is therefore *correct*: the device genuinely is not "active" in the CRDT. A
display-only special-case in `build_owner_state_view` (the ticket's fallback idea) would mask
a real trust-state gap — any other consumer of `evaluate_trust` would still refuse the device.

### Two same-named, distinct stores (disambiguation)

- `harmony_owner::state::OwnerState` → **`owner_state.cbor`** — device-trust registry
  (enrollments / liveness / vouching / revocations). **This is the ZEB-342 store.** The badge
  reads it via `load_owner_state` (`owner_state.rs:374`); the running node holds no competing
  in-memory copy (`get_owner_state` loads from disk each call).
- `crate::owner_state_crdt::OwnerState` → `owner_state_crdt.cbor` — community/space membership
  (ZEB-393). **Unrelated to this fix.**

## Design

A device is trusted only if it has a fresh `LivenessCert`. We make the local device legitimately
active by publishing a self-liveness cert at two surfaces:

- **At mint (upstream):** every minted identity is born active.
- **On owner-state load (client):** repairs identities minted before this fix (incl. Jake's),
  and refreshes before the 30-day freshness window lapses.

### Change set A — upstream `harmony` (`harmony-owner` crate)

`crates/harmony-owner/src/lifecycle/mint.rs`, in `mint_owner(now)`, after
`state.add_enrollment(cert, now, …)?`:

```rust
use crate::certs::LivenessCert;
// device #1 is alive at mint — without this, evaluate_trust refuses the sole device.
let liveness = LivenessCert::sign(&device_sk, owner_id, now)?;
state.add_liveness(liveness)?;
```

`add_liveness` (`state.rs:304`) validation is satisfied: `cert.owner_id == owner_id`, device #1
is enrolled and not revoked, and the cert is signed by `device_sk` (the enrollment's pubkey).
It is last-writer-wins by timestamp, so this composes cleanly with the client refresh.

**Test (TDD):** strengthen `mint_produces_active_device_one` to assert
`result.state.liveness.len() == 1` **and**
`evaluate_trust(&result.state, device_id, now, DEFAULT_ACTIVE_WINDOW_SECS, DEFAULT_FRESHNESS_WINDOW_SECS) == TrustDecision::Full`.
Today the test only checks `enrollments.len() == 1` — the exact gap that let the bug ship.

This is one `harmony` PR.

### Change set B — client `harmony-client` (durable refresh)

New helper in `src-tauri/src/owner_state.rs`:

```rust
/// Ensure the local device has a fresh liveness cert in `state`. Returns true if it
/// mutated `state` (caller must then persist via save_owner_state_cbor_only).
///
/// Publishes a fresh LivenessCert when the local device has NO liveness, or its liveness
/// is older than DEFAULT_FRESHNESS_WINDOW_SECS / 2 (~15 days) — refresh-if-stale bounds
/// disk writes to ~once per boot per fortnight rather than write-on-every-read.
pub fn refresh_self_liveness(
    state: &mut harmony_owner::state::OwnerState,
    device_sk: &ed25519_dalek::SigningKey,
    now: u64,
) -> bool
```

Implementation:
- Derive the local `device_id` from `device_sk` (same derivation as
  `owner_commands::derive_this_device_id` — `PubKeyBundle::classical_only(vk).identity_hash()`).
- Compute `stale = match state.liveness.get(&device_id) { Some(c) => c.timestamp < now.saturating_sub(REFRESH_THRESHOLD_SECS), None => true }` where
  `REFRESH_THRESHOLD_SECS = trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2`.
- If `stale`, `state.add_liveness(LivenessCert::sign(device_sk, state.owner_id, now)?)` and
  return `true`; else return `false`.
- On signing/add error, return `false` (non-fatal — never block the panel render; the device
  stays Refused-as-today, no regression). Log at warn.

**Persistence — cbor-only, keychain untouched.** A liveness refresh mutates only the
`OwnerState` CRDT's `liveness` map; it must NOT round-trip the keychain. The existing
`save_owner_state_atomic` couples three writes and, critically, **clears** the master-seed
keychain entry when `master_seed == None` (`owner_state.rs:444`, the joiner branch). Reusing it
for a refresh would put the master key at risk whenever the seed is transiently unreadable. So
add a narrow writer:

```rust
/// Persist only the OwnerState CRDT to owner_state.cbor (canonical CBOR, atomic 0600).
/// Does NOT touch the device_sk / master_seed keychain entries — used by liveness refresh,
/// which mutates only the CRDT. Callers MUST hold OWNER_STATE_WRITE_LOCK.
pub fn save_owner_state_cbor_only(identity_dir: &Path, state: &OwnerState) -> Result<(), String>
```

Call site — `src-tauri/src/owner_commands.rs`, `get_owner_state` (the **only** read caller of
`build_owner_state_view`; mint is the other caller and is covered upstream). Hold
`OWNER_STATE_WRITE_LOCK` across the whole load+refresh+save window so the `.cbor` write stays
serialized with mint / pairing-install (the discipline documented at `owner_commands.rs:42`):

```rust
run_blocking(move || {
    let _guard = OWNER_STATE_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut loaded = match load_owner_state(&identity_dir, KeychainStore::new().ok())? {
        Some(l) => l,
        None => return Ok(None),
    };
    if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now_unix()) {
        // Fail open (Decision 4): a persist failure must not block the panel — the
        // in-memory state already carries the fresh liveness; next load retries.
        if let Err(e) = save_owner_state_cbor_only(&identity_dir, &loaded.state) {
            tracing::warn!(error = %e,
                "get_owner_state: failed to persist refreshed liveness; rendering from in-memory state");
        }
    }
    Ok(Some(build_owner_state_view(&loaded, display_name)))
})
.await
```

Loading *inside* the lock (rather than the current lock-free load) closes the read-modify-write
race: without it, a pairing-install landing between load and save would be clobbered by the
refresh write. `get_owner_state` is user-initiated (panel open) and infrequent, so the added
lock contention with the rare mint/pairing writers is negligible.

### Data flow

```text
mint:   mint_owner(now) ─► enroll dev#1 ─► add_liveness(sign(dev_sk)) ─► state has fresh liveness
                                                                          │ persisted by mint flow
boot:   get_owner_state ─► load_owner_state(owner_state.cbor)
                           ─► refresh_self_liveness(&mut state, dev_sk, now)
                                ├─ missing/stale ─► add_liveness ─► save_owner_state_cbor_only
                                └─ fresh         ─► no-op
                           ─► build_owner_state_view ─► evaluate_trust ─► Full ─► "trusted" badge
```

## Decisions

1. **Trust target = `Full`** for the sole live, unrevoked device (not Provisional) — correct per
   the model once it's active.
2. **Refresh-if-stale** (threshold = freshness/2) rather than write-on-read, to bound disk writes.
3. **Sequencing:** harmony PR first → merge → client PR bumps all 7 `harmony-*` deps
   `04449d603c042c121ee9836ebd244310adaf7f6a` → new rev (Cargo.toml + Cargo.lock) *and* lands the
   refresh + tests + live-verify. Avoids pointing the client at an unmerged branch commit.
4. **On error, fail open to today's behavior** (no liveness published, badge unchanged) — never
   block the panel.
5. **Persist via a cbor-only writer under `OWNER_STATE_WRITE_LOCK`** (safety-driven, surfaced
   during design): the refresh touches only the CRDT, so it must not go through
   `save_owner_state_atomic` (which clears the master-seed keychain entry when the seed arg is
   `None`). Load happens *inside* the lock to close the read-modify-write race with mint /
   pairing-install.

## Out of scope (YAGNI)

- **Ongoing multi-device liveness *sync*** (a periodic heartbeat propagating liveness to paired
  devices). The pairing-install path already carries whatever liveness is in-state at install
  time; a live heartbeat is a separate feature. File a follow-up note, do not build here.
- Vouching changes. Single-device Full does not require a vouch.

## Testing

### Upstream (`harmony`)
- Strengthened `mint_produces_active_device_one` (red today: sole device is
  `Refused(StaleTrustState)`; green after the liveness publish).

### Client (`harmony-client`) — TDD, Rust unit tests in `owner_state.rs`
- `refresh_self_liveness`: (a) **missing** liveness → returns true, publishes, and a subsequent
  `evaluate_trust` is `Full`; (b) **fresh** liveness → returns false (no-op); (c) **stale**
  liveness (timestamp older than threshold) → returns true, re-signs with a newer timestamp.
- `get_owner_state`-level: an `owner_state.cbor` saved with no liveness, when loaded, returns a
  device whose `trustDecision.kind == "full"`, and the liveness is persisted (reload shows it).
- **Master-seed survival:** after a refresh-persist on an identity that has a master seed,
  `load_owner_state` still returns `master_seed.is_some()` (proves `save_owner_state_cbor_only`
  leaves the keychain entry intact — guards against the `save_owner_state_atomic` clear hazard).
- Negative: signing failure path returns false and does not panic / does not block the view.

### Live-verify on Ildwyn (Playwright/CDP, isolated throwaway `HOME`)
- Fresh mint → onboard → Profile → Devices → assert the sole device badge reads **trusted**
  (not red "refused"); screenshot.
- Existing-identity self-heal: pre-seed a liveness-less `owner_state.cbor`, launch, confirm the
  badge reads trusted and the file now carries a liveness cert.

## CI gates (client PR)
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo fmt --all -- --check`
- `npx tsc --noEmit` + `npx vitest run` (no frontend logic change expected; badge is data-driven)
- Large-tests + MSRV per the standard pipeline.
