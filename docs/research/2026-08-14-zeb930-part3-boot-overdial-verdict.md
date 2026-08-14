# ZEB-930 Part 3 — boot over-dial: quantification + verdict

**Task:** quantify the R4 boot over-dial (records seeded before the admission
oracle binds them fail open at the boot-seed kick); fix only if material.

**Verdict: real but latent.** Material in router mode for communities above
`FULL_MESH_THRESHOLD` (32); **no current production impact** because production
defaults to peer mode. Fix deferred to **ZEB-931**, gated on router-mode
enablement.

## Evidence chain

1. **Pre-oracle seed.** The resolver is populated in `start_node`, before
   `event_loop::run` installs the oracle at `event_loop.rs:1652`:
   - the address-book routing seed (`lib.rs:9361`, "BOOT-PROBE 10") replays
     persisted `addrbook.cbor` rows through `ingest_verified_row`, which *does*
     call `note_enrolled_binding` — but the oracle is still `None`, so the bind
     is a **no-op**;
   - the fleet-sibling seed (`lib.rs:6985`, `update_with_source(FleetSibling)`)
     and the SAS first-contact seeds never bind at all.
2. **Boot-seed kick.** `seed_boot_peers_into_supervisor` (`event_loop.rs:1695`,
   after the install) kicks **every resolver-known peer** as `NewPeer`.
3. **Fail-open.** In router mode the kick-time filter finds those node_ids
   **unbound** → `admit()` returns `true` (`admission_oracle.rs:73`) → all dialed.

## Magnitude and window

- **Magnitude:** the boot-seed enumerates the resolver's **dialable** view
  (`list_dialable_peers`, freshest across durable + pkarr + fleet slots —
  `iroh_zenoh_registration.rs::boot_seed_node_ids_by_recency`), so it kicks every
  known peer across all three sources. In router mode a node therefore boot-dials
  its full dialable roster (~N) instead of ~degree neighbors (~14 at N=200) — a
  ~14× fan-out storm, exactly the unbounded fan-out R4 exists to bound. The
  **genuine over-dial** is the community address-book members that are not
  ring-neighbors; fleet-sibling and SAS-paired entries failing open is *correct*
  (same-owner / first-contact peers are not community ring members subject to the
  degree bound — see the Disposition note). Only bites communities above
  `FULL_MESH_THRESHOLD` (below that the ring is full-mesh, so all peers are
  neighbors and there is no over-dial).
- **Window:** not short. `ingest_verified_row` rebinds only on a *fresher*
  (`Inserted | Replaced`) row (`address_book_sync.rs:227`). A stable peer whose
  boot snapshot equals our disk copy is a no-op upsert → **no rebind** until that
  peer's next republish (idle interval ~60 min). The boot snapshot requester
  closes the window sooner only for peers that have republished since our last save.

## Why it is latent, not a live bug

`zenoh_session_mode()` defaults to `"peer"` (router is opt-in via
`HARMONY_ZENOH_MODE=router`, `event_loop.rs:13341`). In peer mode the oracle is
constructed disabled (`AdmissionOracle::new(false)`), so `admit()` is always
`true` — no filtering, no over-dial, byte-for-byte pre-R4 behavior. The gap
manifests only when router mode is turned on for a large community.

## Disposition

Documented and deferred (this PR ships **Part 2** — the beacon/pkarr bind fix —
plus this verdict). The boot backfill is tracked as **ZEB-931** and gated on
router-mode enablement, so the boot-path change is made and validated
end-to-end when router mode is actually being turned on, rather than landed
blind against a regime we cannot yet exercise in a live fleet.

The proposed fix (for ZEB-931): before `seed_boot_peers_into_supervisor`, walk
each joined community's address book (`CommunityAddressBook::rows_for_community`)
and `note_enrolled_binding(row.actor.0, payload.iroh_node_id, row.device)` for
every Reachability row, so the boot-seed kicks classify against real bindings.

**Scope — address-book rows only, by design.** The backfill covers the community
address-book Reachability rows and deliberately does **not** bind the
fleet-slot-only (`FleetSibling`) or SAS-paired first-contact seeds that
`list_dialable_peers` also returns. Those must **stay fail-open**: they are
same-owner butler siblings or pre-membership first contacts, not community ring
members, so they are not subject to the ring degree bound. Binding one would put
it under the ring-neighbor filter and **park** it — breaking legitimate
same-owner butler deposits / first-contact dialing. So fleet/SAS fail-open at
boot is the intended terminal state, not a residual gap; ZEB-931 should assert
this with a test rather than extend the backfill to cover them.
