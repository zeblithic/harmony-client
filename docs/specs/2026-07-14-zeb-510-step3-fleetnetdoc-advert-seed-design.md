# ZEB-510 step 3 — seed the SAS-observed sibling endpoint into P's FleetNetDoc (butler-set advert)

**Status:** design (Jake approved the direction "Extend branch: FleetNetDoc upsert" via
decision gate 2026-07-14; driving autonomously to the s7 oracle).
**Branch:** `zeb-510-fleet-sibling-dial-seeding` (extends steps 1+2, HEAD `3489c8b5`).
**Grounding:** `.superpowers/sdd/step3-recon.md`.

## Problem

The s7 acceptance (`s7_butler_deposit_recover`) fails co-located: sender A's async-DM
deposit never lands on P's butler sibling B2. Root cause (code-verified): A learns B2's
dialable endpoint from **P's published butler-set advert**, built by
`fleet_net::build_butler_set`, which reads each butler's endpoint from P's **FleetNetDoc
row** (`row.iroh_endpoint_id`/`row.home_relay`). Co-located, P's FleetNetDoc has **no B2
row at all** — the only writers are P's self-row and fleet-net sync-merge, and the merge
never fires without an established peering (the circular bootstrap). Steps 1+2 captured
B2's endpoint via the SAS `Confirm` and wired it into the ReachabilityResolver + a
`fleet_peer_seed` store — the dial-*out* path — but `build_butler_set` reads neither.

This is the general ZEB-510 shape: **a co-located device holds data in one structure, but
the published advert reads a different projection that only converges over the network.**

## Approach

At pairing-commit on the **inviter** side (P in s7), upsert a FleetNetDoc row for the
freshly-enrolled sibling B2 carrying the SAS-observed endpoint, so P's advert carries it
before any fleet-net peering. B2's genuine self-published row later supersedes it via LWW
(same node_id, so dialability is unaffected either way).

### Seam & mechanism (from recon Q2/Q6)
- **NOT** `install_*_state_inner` (sync + file-only + races the live `FleetSyncEngine`,
  which re-persists its whole in-memory doc and would clobber a blind `fleet_net.cbor`
  write). Instead: the **async inviter drainer** (`lib.rs:~12013`), right after the
  `spawn_blocking` install returns `Ok`.
- Write **through the engine**, mirroring the existing self-row upsert
  (`lib.rs:5615-5668`): lock the `fleet_net_doc` tokio Mutex → `devices.insert(dev_id,
  FleetNetRow{ iroh_endpoint_id, home_relay, seen_at, feed_binding: None })` → mirror into
  `fleet_net_snapshot` under the same lock → `notify_dirty()` (+ optional spawned
  `flush_now()` to shorten the persist window before P's kill). The engine then persists
  the row to `fleet_net.cbor`; P's relaunch loads it and `build_butler_set` includes it.
- **device_id** = `hex(result.cert.device_pubkeys.classical.ed25519_verify)` (64-hex SP1
  key, the `devices` map key), captured **before** `result.cert` moves into
  `add_enrollment`. Value from `result.peer_iroh_endpoint` (node_id + relay). No-op when
  `peer_iroh_endpoint` is `None` (pre-step-2 peer).
- **HLC** = `Hlc{ wall_ms: now_ms, logical: 0, device_id: <B2 hex> }` (real wall-clock;
  superseded by B2's genuine later self-row; never inflate).
- **Scope:** inviter-side only (covers s7, where P is the inviter). Joiner-side symmetry
  (B2 learning P's row) needs a new SM field carrying the peer's `PubKeyBundle` — deferred,
  not needed for s7.

### Known second dependency — empirical (recon Q5)
`build_butler_set` emits a sibling entry **only if `vk_lookup(dev_id)` resolves**, and
`vk_lookup` reads `owner_device_cache` (a *separate* projection), NOT
`owner_state.enrollments`. The recon found no boot-seed of `owner_device_cache` from
enrollments, so co-located B2's identity-pub may be absent → the advert would skip B2 even
with a perfect FleetNetDoc row. This is the *same* pattern (B2's pubkey IS in P's
`owner_state.enrollments` from the cert, just not in the projection the advert reads).
Whether it actually bites co-located "needs a runtime check" — so:

**We use s7 + instrumentation as the oracle rather than speculatively building both seeds.**
Phase A ships the FleetNetDoc upsert plus targeted `build_butler_set` instrumentation
(log, per sibling: row-present? vk-resolved? emitted?). Re-run s7:
- **s7 green** → done. Ship steps 1+2+3 as one PR.
- **s7 red, logs show B2 row present but vk_lookup skipped** → add a parallel
  `owner_device_cache` seed at the same drainer (P has B2's `PubKeyBundle` in the cert),
  re-run s7.
- **s7 red, logs show B2 emitted with endpoint but A never dials** → residual transport:
  node_id-only dialability with an empty `home_relay` co-located. No projection-seed fixes
  this; surface to Jake as a deeper-transport finding (do NOT weaken the assert).

## Non-goals / residual risk
- Joiner-side (B2→P) FleetNetDoc symmetry — deferred (SM field needed; s7 doesn't need it).
- `home_relay` may be empty at pairing (relay unresolved co-located); `build_butler_set`
  still emits the entry (filters on vk + staleness, never relay). Node_id-only dialability
  is the residual the s7 HELD re-run truly exercises.

## Testing
- Unit: a drainer-level or fleet_net-level test that a completed inviter enrollment with
  `Some(peer_iroh_endpoint)` upserts a `devices[<B2 hex>]` row with the right
  endpoint/relay/HLC, and a `None` endpoint upserts nothing.
- Acceptance: the `s7_butler_deposit_recover` HELD assert (unchanged, un-weakened) — the
  oracle for the whole fix.
