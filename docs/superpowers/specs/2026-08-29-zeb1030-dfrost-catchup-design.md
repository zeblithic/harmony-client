# ZEB-1030 — D-FROST catch-up: evidence-based epoch/beacon adoption (not RBSR) — Design

**Ticket:** ZEB-1030 (surfaced during ZEB-1028; sibling of ZEB-1029/ZEB-1031)
**Status:** approved (Jake, 2026-08-29 — scope: both populations)
**Author:** Koya (Claude)
**Date:** 2026-08-29

## 1. Goal & motivation

The dfrost committee-event topic is live-only Zenoh pub/sub (`event_loop.rs:11936-12071`; explicit no-backfill design note at `event_loop.rs:267-274`). A node partitioned across a refresh window diverges permanently: every inbound door at the new epoch is locked (`di` → `InvariantViolation` while active, `dr`/`dk`/`rf` rn=2 → `UnknownCeremony`, `rf` rn=1 → engine id-mismatch drop, `rp` → epoch-mismatch), and the one designed heal path — `maybe_heal_straggler`'s `dk` re-mint — structurally cannot reach a receiver whose pending slot was cleared by promotion, abort, or `from_restored` (`community_dfrost_log_engine.rs:1553-1613` vs `community_dfrost_log.rs:1229-1247`).

The code survey (2026-08-29, at `66f325c1`) found the straggler is worse off than the ticket recorded:

1. **The ZEB-1029 sealed share reinstalls cleanly and is still wrong** after a missed refresh: `install_restored_share` checks epoch and `G·x` against `verifying_shares[self]`, but both sides come from the same stale snapshot (`community_dfrost_log.rs:889-953`). The node then reports `has_key_package`, which suppresses the RTS repair path that is its actual recovery.
2. **New-epoch `ts`/`vb` traffic is silently accepted** (refresh preserves members and the joint vk), mis-keying beacon lookups — `find_vrf_beacon_output_by_seed` derives the key from the node's stale `current_epoch` (`community_dfrost_log.rs:747-751`) — and accumulating never-completable `pending_sign` sessions.
3. **A post-promotion fresh joiner gets committee state never.** The sole production writer of `active = true` is `apply_dkg_complete`; a node not subscribed during the live ceremony starts at `DfrostLog::new()` (`lib.rs:60845`) with no path to the state, so Tier-3 verification and sortition stay dark for it permanently.

**Goal:** a catch-up plane that lets (a) a member straggler adopt a later epoch and re-enter signing via existing repair, and (b) a fresh joiner/observer adopt the committee's public state — both with **zero trust in the responder**, preserving the per-event authentication model.

## 2. Approach (decision recorded)

**Approach C — targeted evidence-based catch-up.** A `dfrost/catchup` queryable serves, on request: the responder's status, the **current epoch's ≥t signed `dk` events verbatim**, and recent `vb` beacon events. The receiver re-verifies everything itself through the existing eight-layer authentication model and adopts via three new verify-from-evidence entry points. "Serve the evidence, not the conclusion."

**Rejected — Approach A, RBSR over the event set** (the ticket's candidate). The retained history is deliberately **not replayable into state**: `from_restored` restores events with no handler replay because `di` admission depends on engine-only wall-clock context, and the handlers are stateful and destructive, not a pure fold (`community_dfrost_log.rs:842-871`, `community_dfrost_persist.rs:36-40`). Acquiring the full historical event set therefore reconstructs nothing without an adopt step — and once an adopt step exists, the only events a lagging node *needs* are the current `dk` quorum and missed beacons. RBSR's sublinear-diff machinery solves a problem this plane does not have (the needed set is tiny and targeted), while inheriting one it does (the log grows unboundedly — nothing is ever pruned, `DfrostEventPolicy` inherits `supersession_key = None` — so the RBSR universe ratchets forever).

**Rejected — Approach B, derived-state snapshot transfer.** Shipping `CommitteeState` as a blob replaces a t-of-n quorum of independently-signed, individually-verified events with **one peer's assertion**, discarding all eight authentication layers (size/decode, epoch decrypt, envelope signature + identity binding, replay defense, ceremony-id binding, membership-at-mint-HLC, per-handler committee gates, `dk` cross-confirmation consensus). A single malicious responder could hand a joiner a fabricated joint vk + verifying-shares map, after which the joiner accepts forged Tier-3 tally shares and rejects honest ones (`community_voting_tier3.rs:1090-1112` reads both T1 membership and the T2 DLEQ basis from that map).

**The trust anchors that make Approach C sound:**

- **Straggler (vk-anchored, the strong case):** refresh preserves the joint verifying key and the member set by construction (`community_dfrost_crypto.rs:227`). An epoch-N member can verify an epoch-M `dk` quorum — ≥t distinct actors from the member set it already holds, all agreeing on a vk **equal to the one it already holds** plus a complete verifying-shares map — for any M, with no per-epoch chaining and no responder trust.
- **Beacons (self-certifying):** each `vb` carries a Schnorr signature verifiable against that same preserved joint vk plus a VRF-output derivation check (`community_dfrost_log.rs:1626-1650`). Valid under any epoch.
- **Fresh joiner (membership-rooted, the weaker case):** no held vk exists, so the quorum is validated the way `di` admission validates a committee — every claimed member must exist in the community membership snapshot resolved at the payload mint HLC (`community_dfrost_log_engine.rs:877-908`) — plus a **multi-responder consistency requirement** (§5.4) and the existing vk-immutability check pinning the result forever after (TOFU ratchet).

**Out of scope:** community-epoch key staleness (orthogonal axis — a node with a stale *community* epoch key can't decrypt any plane; healed by existing community state sync); committee-membership changes and true data loss (ZEB-1031's ceremony); pruning/compaction of the dfrost log (pre-existing growth, unchanged).

## 3. Ground-truth constraints (from the survey)

- Full signed event history **is** durably retained and servable: `DfrostSnapshot.events` via `export_events()` (`community_dfrost_persist.rs:88-102`, `community_dfrost_log.rs:838`); the `DkOnly` re-mint family (`lib.rs:63748-63749`) is the narrow precedent for re-serving from history.
- Event identity is the synthesized `DfrostEventId` `(wall_ms, logical, device_id, actor, sig)` — including `sig` deliberately, so re-mints are distinct events. Exact-duplicate apply is a structural no-op via the `VerifiedLog` keyed map.
- `apply_dkg_complete` promotion effects (`community_dfrost_log.rs:1354-1471`): sets epoch/vk/shares **from `consensus_verifying_shares`**, clears the slot, kills DKG transcript secrets, voids `pending_repair`, and drops a held `local_key_package` that mismatches promoted consensus. The adopt path must mirror this discipline.
- The replay tracker advances **only on successful apply** (`community_dfrost_log_engine.rs:1360-1364`) — adopted events must go through the same recording so later live re-deliveries dedup.
- Inbound current-epoch-only cut at `event_loop.rs:12145-12155` gates on the **community** epoch key — the catch-up plane inherits this, by design.

## 4. Architecture: reuse map

Mirror the voting plane's proven layering (engine never touches crypto/Zenoh; adapter never touches the log; type-erased hooks bridge):

**Reused shapes (not code-shared where coupling would be forced):**
- Queryable + requester-task transport pattern from voting backfill/RBSR (`event_loop.rs:11443-11713`): sealed request as GET payload, reply frame stream, `ConsolidationMode::None`, `Locality::Remote`, 10 s timeout, 64 KiB/frame + 16 MiB/round caps, payload-less/oversize/unopenable GETs answered with silence.
- **Channel-log "pattern B" backpressure**: drain the entire reply stream into a `Vec<Vec<u8>>` before touching the engine (`event_loop.rs:13385-13439`) — satisfies the no-await-into-bounded-channel rule structurally rather than leaning on the timeout.
- Single-epoch-snapshot sealing per reply (ZEB-920 rule, `voting_rbsr_seal_reply_and_bodies` at `event_loop.rs:11087-11118`).
- Hook plumbing shape: `VotingRbsrHooks` (`event_loop.rs:189-231`, built `lib.rs:61315-61328`).
- Existing verification helpers, unchanged: `verify_signed_committee_event` (envelope sig + identity binding), the membership-at-mint-HLC resolver used by `di` admission, `DfrostReplayTracker`.

**New (this ticket):**
1. `community_dfrost_catchup.rs` — sans-I/O wire types + validation + pure selection logic (§5.1, §5.2).
2. Three adopt entry points on `DfrostLog` (§5.3).
3. Engine halves `catchup_build_request` / `catchup_respond` / `catchup_ingest` + hint flag (§5.4, §5.5).
4. Transport wiring: `harmony/community/{id_hex}/dfrost/catchup` queryable + per-community requester task + `DFROST_CATCHUP_AAD` (§5.6).

## 5. Components

### 5.1 Wire protocol (`community_dfrost_catchup.rs`)

CBOR, single-letter fields, version byte, mirroring `RbsrMessage` discipline.

- **Request** `CatchupRequest { vr: u8, ep: u64 /* my committee epoch, 0 = none */, ac: bool /* active */, bw: Option<BeaconWatermark> /* latest vb envelope HLC held */ }`, where `BeaconWatermark { wm: u64 /* wall_ms */, lg: u32 /* logical */, dv: String /* device_id */ }`.
- **Reply frames** — every frame is independently sealed under `DFROST_CATCHUP_AAD` and decodes to `CatchupFrame { vr: u8, ri: [u8; 8], bd: CatchupBody }` where `CatchupBody` is `Status { epoch: u64, active: bool }` | `DkEvidence(bytes)` | `Beacon(bytes)` (the `bytes` being a verbatim encoded `SignedCommitteeEvent`). `ri` is a per-round random responder id stamped on every frame so the requester can group frames by responder without relying on transport metadata (Zenoh reply order/attribution is not load-bearing). Exactly one `Status` per responder group; a group missing its `Status` is discarded.
- **Caps:** 64 KiB/frame (`MAX_DFROST_CATCHUP_FRAME_BYTES`), 16 MiB/round drain cap, `MAX_CATCHUP_BEACONS_PER_ROUND` (64) — beacons served **oldest-first above the requester's high-water** so repeated rounds converge contiguously; `dk` evidence is one event per distinct actor (newest per actor) whose payload epoch equals the responder's current epoch.
- `validate_frame` / `validate_request` at the trust boundary: version, size, exactly-one-status-per-group discipline enforced by the requester.

### 5.2 Responder selection logic (pure)

Given the log view and a request: if requester epoch == mine and beacon high-water current → `Status` only. Else: `Status` + the current epoch's `dk` events (from retained history, one per distinct actor, newest per actor — served **only if** ≥ threshold distinct actors are available; a sub-threshold set is served anyway and the requester simply cannot adopt from it) + up to the beacon cap of `vb` events above the high-water. The responder never re-signs or re-mints — verbatim retained bytes only.

### 5.3 Adopt entry points (`community_dfrost_log.rs`)

All three take **already envelope-verified** events (the engine runs `verify_signed_committee_event` per event first — same helper, same resolver, same strictness as live inbound). All three record adopted events into the `VerifiedLog` (so the node can serve catch-up onward — transitive healing) and `notify_dirty`.

- **`adopt_refresh_quorum(events)`** — requires local `active` state. Verifies: ≥ `threshold` distinct actors, every actor ∈ held `members`, all payloads byte-agree on (epoch M > `current_epoch`, joint vk **== held vk**, complete 1:1 verifying-shares map over held members — same 1:1/no-duplicate/no-non-member checks as `apply_dkg_complete`). On success, mirror the promotion discipline: set `current_epoch = M`, install the agreed shares map, **drop the now-stale local key package**; staged rotated material (`pending_rotated`) is installed iff it matches the adopted consensus (mirroring live promotion), else dropped — either way, a held key package that doesn't match the adopted consensus un-suppresses `has_key_package` so the existing ZEB-1027 auto-repair mints the epoch-M share — void all four pending ceremony slots and every `pending_sign` session (none can complete across the epoch move), clear DKG transcript secrets. Rejection is total — no partial state on any failure.
- **`adopt_initial_quorum(events, membership_check)`** — requires **no** active state (`!committee_state.active` and no pending DKG). Same quorum/consensus rules, plus: every committee member claimed by the quorum must resolve in the community membership snapshot at each event's own envelope HLC (`dk` carries no payload mint stamp; served events are verbatim originals, so their envelope HLCs are the promotion-time ones) — the engine closes over the same resolver `di` admission uses. Epoch must be ≥ 1. On success: full promotion (state active, epoch, vk, shares). The existing vk-immutability check (`community_dfrost_log.rs:1216-1222`) then pins the adopted vk forever.
- **`adopt_beacons(events)`** — per `vb`: Schnorr verify against the held joint vk + `derive_vrf_output` check + 64-byte sig-length check (exactly the `apply_vrf_beacon` crypto, minus the `pending_sign`-session requirement); insert into `beacon_index` if absent. Idempotent; per-event failures skip that event without failing the batch (each beacon is independently self-certifying). Requires `active` (a vk to verify against).

The mis-keyed-lookup bug heals by construction: entries the straggler accepted from new-epoch live traffic were inserted under the *true* `message_hash`; once `current_epoch` adopts forward, `find_vrf_beacon_output_by_seed` derives the same hash and finds them. Pinned by test (§7.6).

### 5.4 Engine halves + joiner consistency rule

- `catchup_build_request()` — snapshot `(current_epoch, active, beacon high-water)` under the log lock, encode, seal.
- `catchup_respond(sealed_request) -> Option<Vec<sealed_frame>>` — open, validate, run §5.2 selection under the log lock, seal all frames under **one** community-epoch snapshot + one fresh `rid`. `None` (silence) on any open/validate failure.
- `catchup_ingest(frames)` — group by `rid`; discard status-less groups. **Straggler path** (local state active): pick any single group whose status epoch > mine and whose `dk` evidence verifies → `adopt_refresh_quorum`; then `adopt_beacons` from that group. Groups are tried in descending status-epoch order; the vk anchor makes single-group adoption sound. **Joiner path** (no local state): require **every** responder group's `dk` evidence to agree on the joint vk; any disagreement → adopt nothing, warn loudly (`dfrost catchup: responders disagree on joint vk`), retry next tick. With agreement, each candidate group must also pass a membership snapshot at each event's own envelope HLC (`dk` carries no payload mint stamp; served events are verbatim originals, so their envelope HLCs are the promotion-time ones), tried in descending epoch order; adopt from the group with the highest epoch, then `adopt_beacons` from that group. **Beacon-only path** (epochs equal): `adopt_beacons` from all groups (idempotent). Every adopted event is recorded in the replay tracker exactly as a successful live apply would be.

### 5.5 Cadence + hint

- One requester task per community (spawned beside the dfrost adapter, gated on the same wiring): immediate first pass, then `DFROST_CATCHUP_INTERVAL = 300 s`, waking early on a `catchup_hint: Arc<Notify>`.
- The engine fires the hint (rate-limited to once per `rebroadcast_interval`) from the epoch-ahead failure sites: `dk`/`dr`/`rf` `UnknownCeremony` on an *active* node, `ts`/`vb` `UnknownCeremony`, and `rp` epoch-mismatch. These are exactly the signals that today end at a warn-and-drop.
- `closing` checked at the top of every round; task joined at teardown like its voting sibling.

### 5.6 Transport + AAD

- New constant `DFROST_CATCHUP_AAD = b"harmony-dfrost-catchup-v1"` beside the plane AADs in `community_state_sync.rs`, with a domain-separation test in both directions against `DFROST_TOPIC_AAD` (a catch-up frame must never open as a live dfrost packet and vice versa — catch-up events are routed to *adopt*, never to the live apply path that would drop them).
- Queryable `harmony/community/{id_hex}/dfrost/catchup` declared beside the existing dfrost subscriber; requester GETs with `ConsolidationMode::None` + `Locality::Remote` + 10 s timeout; pattern-B full-drain before ingest.

## 6. Invariants to preserve

1. **Serve evidence, never conclusions** — the responder ships verbatim retained `SignedCommitteeEvent` bytes; nothing it asserts is trusted.
2. **Every adopted event passes `verify_signed_committee_event`** — same resolver, same address-hash binding, same `verify_strict`. Catch-up is a different *delivery* path, never a different *trust* path.
3. **Straggler adoption is anchored to the held vk and held member set** — a quorum proposing a *different* vk is rejected outright (that is ZEB-1031's ceremony, not catch-up).
4. **Joiner adoption requires membership-at-mint-HLC + all-responder vk agreement**, and is a one-way door pinned by vk-immutability thereafter.
5. **Adoption mirrors `apply_dkg_complete`'s discipline** — consensus map installed wholesale, stale key package dropped (un-suppressing repair), pending slots and `pending_sign` voided, transcript secrets cleared, all-or-nothing.
6. **No new signing paths** — the responder never re-signs; `resign_dfrost_event_with_fresh_hlc` is not used here.
7. **Pattern-B drain** — never `.await` engine work inside the reply arm; caps before alloc.
8. **One community-epoch snapshot per reply** (ZEB-920); distinct AAD per plane.
9. **Live-path behavior unchanged** — no existing handler's guards are loosened; the catch-up plane is purely additive.

## 7. Testing (TDD, `--features test-fixtures`)

1. **Adopt-path reject matrices** (log units, real 2-of-3 DKG material via existing fixtures): refresh — sub-threshold, non-member actor, vk mismatch, epoch ≤ current, shares map not 1:1 / duplicate / non-member, disagreeing payloads, inactive local state; initial — active local state, membership check fails, disagreeing payloads; beacons — bad Schnorr, bad VRF derivation, wrong-length sig, no active state. Each pins no-partial-state on rejection.
2. **Straggler happy path** (engine): member A partitioned while B/C refresh N→N+1; A ingests catch-up frames → adopts epoch N+1 + shares map, key package dropped, pending_sign cleared, replay tracker advanced; then (integration) the existing repair path mints A's share and A signs at N+1.
3. **Joiner happy path**: fresh log adopts the initial quorum (membership check passes), verifies a served beacon, state active; subsequent live `dk` at the same vk dedups; a later `di` still rejects (active gate intact).
4. **Joiner disagreement**: two responder groups with different vks → nothing adopted, warn; agreement round → adopts.
5. **Wire/transport**: frame caps, oversize/garbage/payload-less request → silence; AAD domain separation both directions; status-less group discarded; beacon oldest-first-above-high-water windowing converges over repeated rounds.
6. **Mis-key self-heal pin**: straggler that accepted a new-epoch `vb` pre-adoption finds it by seed post-adoption.
7. **Full wire-crossing integration** (`community_dfrost_integration.rs`): two live engines over the real queryable path — partition, refresh, catch-up, repair, sign — the arbiter being messages crossing the wire.
8. **Cadence units**: hint rate-limiting; interval-vs-hint wake ordering; closing-drains.

## 8. Scope, sequencing, risk

**Files:** new `community_dfrost_catchup.rs`; `community_dfrost_log.rs` (three adopt methods + beacon high-water accessor); `community_dfrost_log_engine.rs` (three halves + hint); `community_state_sync.rs` (AAD); `event_loop.rs` (queryable + requester task + hooks); `lib.rs` (wiring); `tests/community_voting/community_dfrost_integration.rs`.

**Sequencing:** (1) wire types + validation (pure); (2) adopt entry points + reject matrices (log units); (3) selection logic + engine halves against in-memory pairs (no transport); (4) transport wiring + hint; (5) integration test. Each stage independently gated.

**Risks:**
- *Joiner trust model is the novel surface* — mitigated: membership-at-mint-HLC reuses the exact `di` admission check; all-responder agreement bounds a lone malicious responder to denial (which it already has by silence); vk-immutability makes adoption one-shot-auditable. Called out for focused review.
- *Auto-repair trigger after share drop* — the design assumes the ZEB-1027 orchestrator requests repair when `active && member && !has_key_package`; verify at implementation time and nudge explicitly post-adopt if the trigger is narrower.
- *Responder load* — bounded per round by caps; requester cadence is 300 s + rate-limited hints.

## 9. Success criteria

- A member partitioned across ≥1 full refresh windows returns, adopts the current epoch from any single honest peer, re-acquires a share via existing repair, and threshold-signs — no manual intervention, no committee action.
- A node that joins the community after DKG promotion acquires verified committee state from agreeing peers and verifies Tier-3 beacons.
- A straggler's pre-adoption mis-keyed beacons become findable post-adoption; stale `pending_sign` sessions do not accumulate past adoption.
- No existing dfrost invariant weakened; the full existing dfrost battery stays green untouched.
