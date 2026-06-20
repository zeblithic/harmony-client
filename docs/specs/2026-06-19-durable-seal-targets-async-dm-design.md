# Durable seal-targets for async DM delivery — design

**Date:** 2026-06-19
**Status:** Approved (brainstorm 2026-06-19)
**Closes:** ZEB-493 (butler rung), ZEB-488 (relay rung)
**Advances:** ZEB-416 / ZEB-418 (butler store-and-forward), under the ZEB-321 cross-WAN umbrella
**Touches the ZEB-458 sealed-relay design** (revisits decision D35)

> Linear-hygiene note: keep ZEB-NNN out of the PR title/body/branch/commit messages
> (Linear auto-closes every ZEB-NNN in a merged PR body, including parents). Cross-
> reference issues in PR *comments*. This spec file may reference them freely — the
> diff content is not the PR body.

## Problem

Harmony's async ("store-and-forward") DM delivery lets a sender deposit a message
for an **offline** recipient onto a *rung* — either the recipient's **butler** (a
second always-on device in the recipient's fleet) or a community **sealed-relay**
host — which holds the sealed blob until the recipient comes online and pulls it.

A deposit must be **sealed to device keys**: per ZEB-458 decision **D35**, there is
"no device-openable owner key," so the seal target is the recipient's device
ed25519 verify-key(s), and the recipient opens its sealed copy with the matching
device secret. This is correct and stays.

The defect is in **how the sender resolves those device-key seal-targets**:

1. **The seal-target set (the "butler-set") is non-durable.** It is populated only
   inside the recipient's **pkarr routing blob** (`lib.rs:6766-6814`,
   `build_butler_set` from the live fleet-net snapshot), signed under the recipient's
   **owner identity key** and fresh for `BUTLER_SET_FRESHNESS_MS` (15 min). Only the
   recipient's *primary* device (the one holding the owner identity key) can publish
   or refresh that blob.

2. **The durable community CRDT omits it.** `ReachabilityAnnounce`'s payload builder
   hardcodes `butler_set: Vec::new(), bs_at: 0` (`reachability_record.rs:279-281`),
   so the replicated, persisted, boot-replayed reachability record carries **no**
   seal-targets.

3. **The resolver therefore has only the live pkarr source.**
   `ReachabilityResolver::resolve_async` (`reachability_resolver.rs`) does in-memory
   CRDT-cache → pkarr fallback; since the CRDT cache entries carry an empty
   butler-set, the only non-empty source is pkarr.

**Consequence — the store-and-forward promise is not met.** When the recipient's
primary device is offline > 15 min, no fresh butler-set exists, so:

- **Butler rung:** `IrohButlerDepositClient::deposit` resolves an empty set →
  `DepositRungOutcome::SkippedNoFreshButlerSet` (`butler_deposit.rs:513-521`,
  surfaced at `dm_outbox.rs:1016`). No deposit is attempted. A cert-only always-on
  butler **cannot** keep the blob fresh (it does not hold the owner identity key).
- **Relay rung:** `ProdCommunityRelayDepositClient::deposit` step 1
  (`community_relay_prod.rs:744-753`) resolves the *same* live butler-set as its seal
  targets and returns `false` immediately when empty (the "D35 accepted gap"). So a
  recipient with **no butler at all** can **never** be served by a community relay —
  defeating the relay's stated role as the no-butler fallback.

This is a structural durability gap in the async-delivery model, not a co-located
test artifact: cross-WAN delivery hits the identical wall (verified by code
inspection; the deposit-rung timing is not the blocker — `DEPOSIT_NOACK_WINDOWS = 2`
fires the deposit at ~15 s, well inside the 60 s/90 s harness budgets).

## Goals

- A sender can resolve a recipient's device-key seal-targets **while the recipient is
  fully offline**, for both rungs, using durable replicated state.
- The community relay can serve a recipient who has **no butler** at all.
- Preserve D35's security model: seal to **device** keys only, bounded fan-out (≤2).
- Keep the change security-sound: a co-member must not be able to forge or strip a
  recipient's published seal-targets.

## Non-goals

- **Butler-refreshes-reachability** (a cert-only always-on device republishing the
  owner's reachability so the owner stays resolvable when the *whole* fleet's primary
  is long-offline). Deferred — it is a signing/trust-model change better scoped under
  ZEB-461 / ZEB-321.
- Changing the pull / open / ingest path (recipient-scoped pull, per-device sealed
  copies) — unchanged.
- Changing the pkarr first-contact path — it stays live (15-min) for discovery.
- Relay reputation / selection / unlinkability (out of scope per ZEB-458 D44).

## Design

Approach **A — split by rung**: each rung uses the durable seal-target source that
fits its mechanics. The butler rung dials a butler endpoint, so it needs the full
`ButlerSetEntry` (vk **+** `iroh_endpoint_id`); the relay rung only seals (the relay
holds, the recipient pulls), so it needs only device **vks**, which are already
durable in community membership.

### Part 1 — Butler rung: carry the butler-set in the durable community CRDT

1. **Populate** `butler_set` + `bs_at` in `build_signed_payload_with_key`
   (`reachability_record.rs:254`) from the same fleet-net snapshot + `build_butler_set`
   (`fleet_net.rs:210`) the pkarr path already uses, instead of the hardcoded empty at
   `reachability_record.rs:279-281`. `ButlerSetEntry` already carries
   `{device_id, iroh_endpoint_id, device ed25519 vk, pinned}` (`reachability_record.rs:25`).

2. **Authenticate it.** Extend `inner_signed_bytes` (`reachability_record.rs:178`) to
   cover `butler_set` + `bs_at`, so the recipient's identity signature binds its own
   seal-targets. A malicious co-member or relay cannot forge a different butler-set or
   strip it without breaking `verify_inner_signature`. **This is a flag-day change**
   to the signed preimage and CBOR wire bytes (see Wire format below).

3. **Resolve from the durable source.** No structural change to
   `resolve_async` — once CRDT-cache entries carry a non-empty signed butler-set,
   `freshest_butler_set` returns it. The resolver already prefers the CRDT cache over
   pkarr.

4. **Freshness exemption (Decision 3).** CRDT-sourced butler-sets are **exempt from the
   15-min `BUTLER_SET_FRESHNESS_MS` window**; bounded instead by community-membership
   validity and self-healing via CRDT last-writer-wins on the recipient's next online
   publish. The 15-min window stays for **pkarr-sourced** butler-sets (live discovery).
   Rationale: the seal-target *vk* is durable even when the *endpoint* drifts — a stale
   butler endpoint merely fails the dial and falls through to the relay rung; the vk
   remains a valid seal-target. Implementation: thread a per-source freshness policy
   into the resolve path (live vs durable), rather than a single global window.

### Part 2 — Relay rung: durable enrolled-device fallback

In `ProdCommunityRelayDepositClient::deposit` step 1 (`community_relay_prod.rs:744-753`),
replace the live butler-set requirement with a durable resolution:

1. Resolve the recipient's **durable butler-set** (Part 1). If non-empty, use it as the
   seal targets (≤2, unchanged behavior, now durable).
2. **Else fall back** to ≤2 of the recipient's **enrolled device ed25519 vks** drawn
   from durable community membership — `OwnerDeviceEntry.device_identity_pubs`
   (`owner_state_types.rs:661-662`), which is the replicated "signature-verification
   source of truth" already available to any co-member (D40 already gates the rung on a
   shared `Joined` community). Cap at `BUTLER_SET_MAX_ENTRIES` (≤2), recent-first where
   per-device recency is known, else deterministic (device-id order).

Seal targets are still device vks via `birational(vk)` (ZEB-372) — D35 preserved. The
relay still holds opaque per-device sealed copies; the recipient pull/open/ingest path
is unchanged. This fixes ZEB-488 (butler-less recipient is now servable) using only
already-durable data, and also closes the durability gap for the relay rung even when a
butler exists.

Also: correct the ZEB-488 finding text — the co-located `held=false` was **not** a
"ZEB-466 community-relay resolve/dial gap"; the deposit returned `false` at the
seal-target precondition (`community_relay_prod.rs:751`) before any relay dial.

## Decisions

1. **Split seal-target source by rung.** Butler rung → durable CRDT butler-set (needs
   endpoint to dial); relay rung → durable butler-set else enrolled-device vks (seal
   only). Rejected: one uniform CRDT-butler-set source for both — leaves a butler-less
   recipient unservable by the relay (the ZEB-488 defect).
2. **Relay enrolled-device fan-out capped at ≤2, recent-first.** Preserves D35's cost
   bound. Rejected: seal to all enrolled devices (N relay-stored copies — over-built
   for alpha).
3. **CRDT-sourced seal-targets exempt from the 15-min pkarr freshness window**; pkarr
   source keeps the live window. Without this, a recipient offline more than 15 min
   still resolves empty and the fix does nothing.
4. **Inner identity signature must cover `butler_set` + `bs_at`.** Flag-day preimage
   change; required so seal-targets can't be forged/stripped by a co-member or relay.
5. **D35 unchanged in spirit:** seal to device keys, bounded fan-out. This design only
   changes *where the device keys are resolved from* (durable vs live), not *what* they
   are.

## Data flow

- **Publish (recipient online).** The recipient's primary builds its butler-set from
  the fleet-net snapshot and publishes a signed `ReachabilityAnnounce` (now carrying
  the butler-set, inner-sig-covered) into each `Joined` community's membership CRDT →
  replicates to co-members, persists, replays at boot.
- **Resolve (sender online, recipient offline).** The sender reads the recipient's
  reachability from its **durable CRDT cache**. Butler rung: dial the butler endpoints
  from the durable butler-set. Relay rung: seal to the durable butler-set vks, or fall
  back to ≤2 enrolled-device vks; deposit to a community relay both share.
- **Recover (recipient online on any device).** The device pulls from butler/relay,
  opens its sealed copy with its device secret, runs the normal receive path —
  unchanged.

## Security

- D35 preserved: seal to device vks only (no owner key), bounded fan-out.
- The inner identity signature now binds `butler_set` + `bs_at` → a co-member/relay
  cannot forge or strip the recipient's seal-targets.
- The enrolled-device fallback uses only EnrollmentCert-authenticated membership device
  keys — no new trust grant; the same keys already verify the recipient's membership
  events.
- The relay continues to hold opaque per-device sealed copies and admits deposits only
  for co-members (D36/D40) — unchanged.

## Error handling

- **Stale butler endpoint** in the durable set → dial fails → falls through to the
  relay rung / existing retry backoff. The seal-target vk is still valid.
- **Removed device** still in a not-yet-refreshed durable set → at worst a wasted
  sealed copy, TTL-GC'd by the holder; self-heals on the recipient's next publish
  (CRDT LWW).
- **Empty enrolled set** (should not happen for a real co-member) → relay returns
  `false` as today; the DM stays queued in the outbox.

## Wire format / flag-day

`ReachabilityAnnouncePayload` already declares `butler_set` + `bs_at`; the change is
(a) populating them in the CRDT path and (b) extending the signed preimage to cover
them. Old signatures will not verify against the new preimage → flag-day. Acceptable
for alpha (precedent: ZEB-474 accepted a `ContactAddress` postcard discriminant break).
Regenerate the pinned `ReachabilityAnnounce` wire fixtures with a non-empty, signed
butler-set.

## Testing

- **Unit:**
  - `freshest_butler_set` / resolve path: durable source returns a butler-set past the
    15-min window; pkarr source still filtered at 15 min.
  - Relay `deposit` seal-target resolution: butler-set present → uses it; butler-set
    absent → enrolled-device fallback, ≤2 cap, recent-first.
  - `verify_inner_signature` **rejects a forged/stripped butler-set** (tampered field
    breaks the inner sig).
- **Wire fixtures:** regenerate `ReachabilityAnnounce` pinned fixtures (flag-day),
  asserting a non-empty signed butler-set round-trips.
- **e2e-harness (`e2e_two_node.rs`):** promote **s6** (relay) and **s7** (butler)
  `HELD → RECV → CLEARED` from characterize-fallback to **hard assert**, co-located.
  This is now deterministic because the durable butler-set replicates to the
  sender/relay **before** the recipient's primary is SIGKILLed, so the post-kill resolve
  succeeds from the durable cache. The scenarios must publish/sync the recipient's
  reachability while it is still online, then kill, then send. Cross-WAN Scenario
  D2/D3 (Ildwyn/AVALON) becomes a confirmation run, not the sole proof.

## Scope / rollout

One coherent feature → one PR: src-tauri Parts 1+2 (reachability payload + sig +
resolver freshness policy + relay deposit resolution) + e2e-harness assert promotion +
fixture regeneration. Meaty but cohesive; phase into sequential commits/PRs in the
implementation plan if it grows too large (one PR in flight at a time, per the bundling
rule). Additive at the feature level; the only break is the flag-day reachability
preimage (alpha-acceptable).

## Future work

- Butler-refreshes-reachability (non-goal above) for whole-fleet-long-offline
  resolvability — ZEB-461 / ZEB-321.
