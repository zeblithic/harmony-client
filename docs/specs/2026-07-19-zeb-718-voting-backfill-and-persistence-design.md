# ZEB-718 — Voting backfill / pull-on-rejoin + local persistence

**Status:** Design (approved decisions inline)
**Author:** Koya (fleet)
**Date:** 2026-07-19
**Ticket:** [ZEB-718](https://linear.app/zeblith/issue/ZEB-718)
**Related:** ZEB-717 voting-topic epoch encryption (`docs/specs/2026-07-19-zeb-717-voting-topic-epoch-encryption-design.md`, PR #504), ZEB-315 at-event-HLC membership resolution (PR #502), ZEB-270/248/585/593 channel-log backfill (the pattern mirrored here)

## 1. Problem

The voting Zenoh topic (`harmony/community/{id}/voting`) is the **sole** delivery path for voting
events. `VotingLog` is an in-memory registry (`lib.rs:1132`, `NodeState.voting_logs`) — not a field
of `OwnerState`/`Space`, not reconciled by `community_state_sync`, not persisted to disk, and with
**no backfill** (`community_voting_log_engine.rs:14`: *"No backfill."*). Two consequences, both the
same gap — *anything missed on the live topic is lost*:

- A peer offline when a voting event is published **permanently misses it** — no catch-up.
- ZEB-717's current-epoch-only receive cut (the provably-unique transport mechanism that defeats a
  kicked-then-rotated member — that spec §2) has an inherent cost: a legitimate vote published under
  epoch N, arriving at a peer that already applied the N→N+1 rotation, is **dropped** with no
  recovery (ZEB-717 §2.1).

And separately: a node that restarts loses its entire in-flight `VotingLog` (it is never persisted),
recoverable only from a peer that stayed up.

ZEB-718 closes both: a **peer-to-peer backfill** (pull-on-rejoin) so a peer recovers what it missed on
the wire, and **local persistence** so a node's own in-flight voting state survives its restart. User
decision (2026-07-19): approach **A+** — dedicated sparse pull channel **plus** persist `VotingLog` to
disk. Not the state-root fold (§3 D1).

## 2. The mechanism (derived from the two subsystems it borrows from)

The ticket said "mirror channel-log." Investigation showed we must mirror channel-log's backfill
**structure** but the voting adapter's **crypto** — two different subsystems — and get three details
exactly right or the acceptance criteria silently fail.

### 2.1 Re-encrypt under the *current* epoch on serve — not replay stored ciphertext

Channel-log's backfill responder re-encrypts served events under a **fixed** `channel_key` that never
rotates per epoch (derived once from `membership_key`; `community_channel_log_engine.rs:2585`). Voting
is the opposite: its adapter does a **per-packet current-epoch lookup** against live CRDT state — that
*is* the ZEB-717 containment cut. So the voting backfill responder must encrypt each served event at
serve time under `space.current_epoch_key` + `VOTING_TOPIC_AAD` (exactly the adapter's outbound path,
`event_loop.rs:9421-9430`), **not** re-emit whatever epoch each event was minted under.

This is what turns ZEB-717 §2.1's cross-rotation drop from *lost* into *recovered*: peer B, already at
epoch N+1, dropped A's epoch-N event `e1` on the live topic. B backfill-pulls; responder C re-encrypts
`e1` under N+1; B's own current-epoch cut passes (N+1 == N+1); B applies `e1`. The inner
`SignedVotingEvent` and A's signature are unchanged — only the transport envelope is fresh — so verify
still passes (at `e1`'s own HLC, ZEB-315 rolling eligibility, where A *was* a member).

It also means **the serve-path encryption doubles as backfill access control** (§3 D5): a
kicked-then-rotated member who issues a backfill `get` receives current-epoch ciphertext they cannot
decrypt (no `K(N+1)`), and still cannot inject (cannot produce `K(N+1)` ciphertext). The ZEB-717 cut is
preserved on the backfill path with **no** explicit requester authentication.

### 2.2 Coordinate dedup on apply — not the per-lane high-water tracker

The **only** dedup in the voting path today is `VotingReplayTracker`
(`community_voting_log_engine.rs:152`): a per-lane high-water map `(actor, device_id) → (wall, logical)`
that drops any event `≤` the mark. That is correct for the common catch-up case (an offline peer is
behind on every lane, so backfilled events are *newer* than its marks and pass) — but it **silently
swallows an in-lane gap**: the cross-rotation drop is exactly a peer that has *later* events on lane A
(received post-rotation) but missed the middle one `e1`. `contains(e1)` is true → dropped. That fails
acceptance criterion 2.

So the backfill apply path dedups by **exact event coordinate**
`(actor, hlc.wall_ms, hlc.logical, hlc.device_id)`, not the high-water mark. A `HashSet` of seen
coordinates gives O(1) membership; both live-inbound and backfill record into it, and the backfill
apply consults it. The live-inbound fast path keeps the high-water tracker unchanged (lowest
regression risk); the coordinate set is additive.

### 2.3 Full-dump, not RBSR

Channel-log offers two query families: `since/**` (dump events after a watermark) and `rbsr/**`
(range-based set reconciliation, ZEB-593). RBSR's payoff — avoiding re-transfer of a large shared
history — **does not exist** for a sparse log (a community has a handful of live polls), it is fragile
with multiple responders (`saw_extra_reply` → bail to fallback; common on rejoin), it needs a
chunk-index the voting log doesn't have, and — decisively — a per-lane watermark **cannot** recover an
arbitrary in-lane gap (§2.2). A full-dump of the community's *live* events + coordinate dedup recovers
any gap and is trivially cheap at voting volume. Archived polls are pruned from `log.events`
(`archive_finalized_polls`, `community_voting_log.rs:1191`), so "live events" is a naturally bounded
set — and archived polls are already finalized, so their outcome rode the membership CRDT and needs no
voting-event recovery.

## 3. Decisions

### D1. Dedicated sparse pull + persist `VotingLog` — **A+** (approved)

Not the state-root fold. Folding `VotingLog` into `OwnerState`/`community_state_sync` would be a large
change (persistence + state-root wire + a CRDT merge for conviction's Q96.32 fixed-point + deterministic
serialization) **and** would ride the state-root plane's old-key-*fallback* decrypt — which ZEB-717
deliberately kept voting out of — risking re-opening the containment hole. A dedicated pull keeps the
in-memory `VotingLog`, mirrors the proven channel-log structure, and leaves the ZEB-717 cut's shape
untouched. Local disk persistence (D3) provides restart-durability without that coupling.

### D2. Full-dump reconciliation, no RBSR (§2.3).

### D3. Persist `{events, policy, poll_restore}`; replay + overlay — **YES** (approved)

`VotingLog` as a whole is not serde-serializable (`Tier3PollState.committee_oracle: Arc<dyn
CommitteeOracle>` is a non-serde trait object; every materialized container lacks derives). Persist the
serde-clean subset: `events: Vec<SignedVotingEvent>` (already the CBOR wire type) and `policy:
CommunityVotingPolicy` (already serde; local-only, must persist or post-restart replay may use wrong
conviction thresholds). On boot, **replay** events through `apply_with_snapshot` to rebuild
`polls`/`delegation_graph` — safer than trusting serialized fixed-point conviction state.

**But replay alone is insufficient — it loses tick-driven lifecycle** (found in pre-push review). The
lifecycle transitions that matter are *not* events: Tier-1 auto-close (`meta.lifecycle = Closed`),
Tier-2 finalize (`meta.lifecycle = Finalized` + `finalized_at_ms`), and archive (`meta.lifecycle =
Archived` + event prune) are all in-place `PollMeta` mutations by the tick; the Tier-2 contestability
clock (`threshold_reached_at_ms`, `last_unsignal_after_threshold_ms`) lives in `tier_state`. Replaying
events alone reconstructs a Finalized/Archived poll as active `Open` (re-contestable or a permanent
zombie), and resets a mid-contestability window. So we also persist a **`poll_restore` overlay** —
`HashMap<PollId, {meta: PollMeta, tier2_timing}>` — and reapply it after replay. Tier-3 stage
transitions and Tier-2 auto-exec (ZEB-300) are *event*-driven (persisted + replayed), so the overlay
needs only `meta` + the two Tier-2 timing fields to be complete.

Disk shape mirrors the per-community CRDT (`community_state_persist.rs`): plaintext CBOR at rest
(the codebase convention — channel-log segments are plaintext at rest too; encryption is wire-only;
events are signed → tamper-evident), a 1-byte version prefix, a `community_id` match on load, and
`quarantine → default` on corruption. File: `identity_dir/communities/{id_hex}/voting.cbor`, alongside
`crdt.cbor`. The at-rest write is peer-recoverable (backfill), so the lightweight
`community_state_persist`-style atomic write (temp + rename) is the right weight.

### D4. Coordinate dedup on the backfill apply path (§2.2).

### D5. Serve-path current-epoch encryption doubles as backfill access control (§2.1) — no requester auth.

### D6. Eager engine spawn at boot — **YES**

Today voting engines spawn lazily on first voting IPC. For backfill to *recover on boot* (not
recover-on-next-click) and for a node to serve backfill to others, the engine + adapter must run.
Mirror channel-log's `reconcile_from_state`: at boot, per community, load+replay `voting.cbor` then
`ensure_voting_engine_for` (spawns engine + adapter + backfill driver). Engines are lightweight (mpsc +
a receive loop) and communities are few; the cost is bounded.

## 4. Architecture

Everything below is additive; the `VotingLogEngine`'s live publish/inbound paths and all mpsc-bridged
voting tests are unchanged except the additive coordinate set (D4) and the persist hook (D3).

### 4.1 Persistence — `community_voting_persist.rs` (new)

Mirror `community_state_persist.rs`:

```
// on-disk record (versioned, community-id-checked, plaintext CBOR):
struct PersistedVotingLog { version: u8, community_id: SpaceId,
                            events: Vec<SignedVotingEvent>, policy: CommunityVotingPolicy,
                            poll_restore: HashMap<PollId, PollRestore> }   // D3 tick-driven overlay
// The path helper OWNS the SpaceId→hex conversion (never a preformatted &str) so voting.cbor
// can never diverge from the sibling crdt.cbor:
fn voting_path_for(identity_dir: &Path, community_id: &SpaceId) -> PathBuf  // communities/{id_hex}/voting.cbor
// Split persist so blocking disk I/O runs off the async worker with no lock held:
fn snapshot_for_persist(log: &VotingLog, community_id: &SpaceId) -> VotingLogSnapshot   // clone under the lock
fn write_snapshot(path: &Path, snapshot: &VotingLogSnapshot) -> Result<(), PersistError> // blocking encode+write; spawn_blocking
pub fn save_voting_log(path: &Path, log: &VotingLog, community_id: &SpaceId) -> Result<(), PersistError> // sync convenience = snapshot+write
pub fn load_voting_log(path: &Path, expected_id: &SpaceId)
        -> Result<(Vec<SignedVotingEvent>, CommunityVotingPolicy, HashMap<PollId, PollRestore>), PersistError>
//   missing file            → Ok((empty, default, empty))
//   decode / version / id-mismatch → quarantine aside + Ok(empty default)   (self-heals; peer-recoverable)
//   OTHER I/O error on an existing file → Err   (present-but-unreadable: caller must NOT arm persistence & clobber it)
```

`VotingLog` gains a read accessor for `policy` (already `policy()`), and the snapshot reads the
public `events` field. The engine gains a **persist hook** `persist_now()` that clones a `{events,
policy, poll_restore}` snapshot and runs the CBOR-encode + atomic write inside **`spawn_blocking`** so
`std::fs` I/O never parks a Tokio worker (repo persistence pattern — PRs #74/#380/#381). It is called
after every mutation that changes the log: local mint (`publish_event`), inbound apply
(`process_inbound`), backfill apply (§4.3), archive-prune (`archive_finalized_polls`), and policy change
(IPC `set_policy`). The per-community `voting_log` mutex is held **across** the `spawn_blocking` write
(not released after the snapshot): `persist_now` runs concurrently from three tasks (IPC publish /
inbound loop / backfill apply) and the tick archive sweep writes the same `voting.cbor` — all serialize
on this one mutex, so holding it across the write prevents two writers racing on the fixed `.tmp` name
and landing out of order (a stale snapshot renaming last → lost update). The hold is a sub-ms clone +
off-worker write on a sparse path, so contention is negligible. The tick archive sweep first snapshots
the map's Arcs and **drops the global `voting_logs` lock** (so a 24h sweep can't block every community's
voting), then for each pruned log holds *that* community's `voting_log` mutex across its `spawn_blocking`
write — same serialization discipline. Full-rewrite per mutation is O(n) in log size; at voting volume
(sparse, archive-bounded) this is negligible.

**Load-error discipline (D2, as-built):** the "quarantine → default" self-heal applies ONLY to a file
that is present but *malformed* (decode / version / id-mismatch) — a corrupt local file self-heals
because voting is now peer-recoverable via backfill. A **transient I/O error on an existing file**
(e.g. permissions, a failing read) is NOT treated as empty: `load_voting_log` returns `Err`, and
`ensure_voting_engine_for` responds by leaving persistence **disarmed** for that session (it does not
`install_persist_dir`), so the first mutation can't overwrite the still-recoverable on-disk file with
empty state. A clean restart that reads successfully re-arms persistence.

The engine needs the write path: `ensure_voting_engine_for` / `VotingLogEngineParams` gain
`identity_dir: PathBuf` (in scope at every call site via `resolve_identity_dir`).

### 4.2 Backfill responder — adapter queryable (mirror `since/**`)

`spawn_voting_log_zenoh_adapter` (`event_loop.rs:9363`) gains a **queryable** on
`harmony/community/{id_hex}/voting/backfill` (a fixed key — full-dump takes no watermark). It also
gains a `read_for_backfill` closure (mirror channel-log's `read_for_query`,
`community_channel_log_engine.rs:2557`) supplied by the engine at spawn:

```
read_for_backfill: Fn() -> Pin<Box<dyn Future<Output = Vec<Vec<u8>>> + Send>>   // plaintext SignedVotingEvent CBOR frames, live polls only
```

On a query, the adapter calls `read_for_backfill()`, then — for each plaintext frame — encrypts under
the **current epoch** (`crdt_state.lock` → `space` → `encrypt_for_topic_with_aad(space, frame,
VOTING_TOPIC_AAD)`) and replies one `EncryptedEnvelope` CBOR frame per event, under
`ConsolidationMode::None` (so all frames stream; channel-log's `10102-10109` rationale). Missing epoch
state → serve nothing (a node without the current key is not a serving member). The engine stays
wire-agnostic: it hands out plaintext; the adapter owns all crypto (ZEB-717 invariant preserved).

### 4.3 Backfill requester — driver + coordinate-dedup apply

A lean **backfill driver** (mirror `run_backfill_driver` / `BackfillLatch` but no paging — full-dump is
atomic) is spawned per engine at `ensure_voting_engine_for`. It issues a full-dump `get` on the
backfill key (`ConsolidationMode::None`, `Locality::Remote` to skip self-reply, **`.timeout(10s)`** so a
hung/never-completing round can't stall anti-entropy — mirrors the RBSR get path) and, for each reply
`EncryptedEnvelope`, **caps the payload at `MAX_VOTING_PAYLOAD_BYTES` (64 KiB) before materializing it**
(replies are peer-controlled — parity with the live subscriber's allocation-DoS guard), then applies the
**current-epoch cut then decrypt** (identical to the live inbound seam) → plaintext `SignedVotingEvent`
CBOR → hands it to a new engine method:

```
async fn apply_backfilled_event(&self, plaintext: &[u8]) -> Result<Option<PollId>, String>
// 1. decode SignedVotingEvent
// 2. coordinate dedup: seen_coords.contains((actor, device_id, wall, logical))? -> Ok(None)
// 3. resolve membership snapshot at event.hlc (self.membership_resolver.snapshot_at)
// 4. verify_voting_event + inbound_eligibility_check (at event's own HLC)
// 5. log.apply_with_snapshot(event, community_id, Some(snapshot))
// 6. seen_coords.insert(coord); persist_now()
```

Driver re-arm (voting engine persists across reconnects, so spawn-time pull alone misses reconnect):
- **transport up-edge** (`transport_epoch_rx`, ZEB-434, already produced by `peer_liveness`) → re-pull
  (reconnect catch-up).
- **periodic floor** (jittered ~1h, restart-aware) → re-pull (anti-entropy backstop; catches
  router-only holders and the cross-rotation drop even absent a transport edge).
- **backoff-retry** on zero responders (exponential, capped — reuse `channel_backfill` constants).

Presence-resync (new-holder) re-arm is optional and deferred; the periodic floor + transport edge cover
the acceptance criteria.

**Out-of-order arrival — no buffering needed (self-healing by construction).** Replies stream one frame
at a time and apply immediately; with multiple responders there is no cross-responder causal-order
guarantee, so a dependent event (e.g. a `BallotCast` for a poll whose `PollCreate` hasn't arrived yet)
can be rejected by `apply_with_snapshot`/eligibility. This does **not** lose the event: `apply_backfilled_event`
records the dedup coordinate (`seen_coords.insert`) **only after a successful apply** — every failure
path (`decode` / `snapshot resolve` / `verify` / `eligibility` / `apply`) returns without recording. So a
rejected event is *not* suppressed, and the next periodic full-dump re-pull re-delivers and re-attempts
it, by which time its predecessor (delivered earlier in the same or a prior pull) is present. The scheme
converges in at most *D* pulls, where *D* is the dependency depth (shallow for voting: `PollCreate` →
`BallotCast`/`Signal` → Tier-3 stage events). Explicit reply buffering / topological reordering is
therefore an unnecessary complication and is deliberately **not** implemented.

### 4.4 Boot reconcile (D6) — `reconcile_voting_from_state`

In `start_node`'s per-community boot loop (`lib.rs:7871-7987`, after community sync engines are
reconciled so membership-at-HLC is available), for each community (`OwnerState.spaces`, Community,
not-left): `load_voting_log(voting_path, id)` → build `VotingLog`, set policy, **replay** each event in
stored order via `apply_with_snapshot(event, id, Some(resolver.snapshot_at(event.hlc)))` (per-event
errors logged, not fatal) → insert into `NodeState.voting_logs` → `ensure_voting_engine_for` (attaches
engine + adapter + driver to the reloaded log). Replay in stored order reproduces the pre-restart
state (the stored log holds only successfully-applied events, in apply order). `NodeStateMembershipResolver`
(`lib.rs:47810`) supplies `snapshot_at`.

### 4.5 Data-flow summary

```
persist:  every log mutation -> engine.persist_now() -> {events, policy} CBOR -> atomic write voting.cbor
boot:     load voting.cbor -> replay@snapshot(hlc) -> rebuild polls/delegation -> ensure engine (spawn adapter+driver)
serve:    peer GET harmony/community/{id}/voting/backfill
          adapter: read_for_backfill() plaintext frames -> encrypt@current+AAD -> reply per-frame envelopes
recover:  driver GET (spawn / transport-up / periodic) -> per reply: [epoch==current?] -> decrypt@current+AAD
          -> plaintext -> engine.apply_backfilled_event (coordinate-dedup -> verify@hlc -> apply -> persist)
```

## 5. Error handling

- **Serve, missing epoch state / space:** serve nothing (drop, `debug`). Non-serving member.
- **Recover, stale/unknown epoch reply:** dropped by the current-epoch cut (as live). A correct
  responder serves under current epoch; a stale reply is a misbehaving/old peer.
- **Recover, decrypt failure:** drop (tamper / kicked-member ciphertext they can't have produced).
- **Recover, duplicate (coordinate seen):** `Ok(None)`, no re-apply.
- **Recover, verify/eligibility/apply reject:** log, skip (defense-in-depth; the responder's log could
  contain an event this replica legitimately rejects at its own HLC).
- **Persist write failure:** log `warn`, continue (the in-memory log is authoritative; next mutation
  retries; state is peer-recoverable). No panic.
- **Load, corrupt `voting.cbor`:** quarantine to `voting.cbor.corrupt.<ms>`, start empty, recover via
  backfill (mirror `community_state_persist::quarantine_corrupted`).
- **Load, community-id mismatch:** `PersistError::CommunityIdMismatch`, treat as corrupt.

## 6. Testing

**Unit — persistence (`community_voting_persist.rs`):**
- round-trip: `save_voting_log` → `load_voting_log` → replay reconstructs identical `polls` (tally +
  conviction) and `delegation_graph` for a Tier-1 poll, a Tier-2 conviction proposal, and a delegation
  chain.
- version prefix + community-id match; corrupt file → quarantine + empty.

**Unit — coordinate dedup (`community_voting_log_engine.rs`):**
- `apply_backfilled_event` skips an already-applied coordinate (`Ok(None)`).
- `apply_backfilled_event` **applies an in-lane gap** the high-water tracker would have dropped
  (received `e2`, missing `e1 < e2` on the same lane) — the direct regression pin for criterion 2.

**Integration — the acceptance criteria** (extend `community_voting_zenoh_integration.rs`, two real
`zenoh::Session`s with the adapter on each; seed epoch state on both):
1. **Offline-miss recovery:** A publishes while B's engine is not yet delivering; B (engine up) issues a
   backfill pull → recovers and materializes the missed events. (Criterion 1.)
2. **Cross-rotation recovery:** A publishes `e1` under epoch N; B rotates to N+1 and drops `e1` on the
   live cut; B backfill-pulls → responder re-encrypts `e1` under N+1 → B applies `e1`. (Criterion 2 —
   exercises §2.1 re-encrypt + §2.2 coordinate dedup together.)
3. **Cut not weakened:** a kicked-then-rotated identity's backfill `get` returns current-epoch envelopes
   it cannot decrypt (no `K(N+1)`) and it cannot inject; B applies nothing from it. (Criterion 3 / D5.)

**Boot replay (`tests/…`):** persist a log, restart-simulate (drop + `reconcile_voting_from_state`),
assert `voting_logs` rebuilds identical materialized state without any peer.

**Wire fixtures:** backfill replies reuse the existing `EncryptedEnvelope` wire — no new format. Assert
round-trip (encryption is nondeterministic); inner plaintext `SignedVotingEvent` fixtures unchanged.

## 6.1 Implementation notes (as-built, diverged from plan)

- **Eager boot-spawn (D6) → reload-on-first-setup.** The voting-engine's full handle set (signing key,
  app_handle, dfrost/beacon) is not assembled at the early per-community boot loop where channel-log
  reconciles, so `reconcile_voting_from_state` is instead called from `ensure_voting_engine_for`'s
  first-setup path (idempotent). The engine still spawns lazily (first voting IPC / access), but it
  attaches to the reloaded log — restart-durability holds; only the *timing* of reload shifts from
  boot to first-access. Trade-off: a pending Tier-2 finalization on a passive node resumes at first
  voting access rather than immediately at boot (acceptable at current scale; eager boot-spawn is a
  future enhancement).
- **Driver inlined in the adapter** (no separate `community_voting_backfill.rs`): the requester loop
  lives inside `spawn_voting_log_zenoh_adapter` (which owns the Zenoh session), pulling on spawn and
  every `VOTING_BACKFILL_INTERVAL` (300s). Transport-up re-arm is deferred (the periodic floor covers
  reconnect within the interval).
- **Backfill closures cross the engine↔adapter boundary** via `VotingBackfillReadFn` /
  `VotingBackfillApplyFn` type aliases; the underlying `read_backfill_frames` / `apply_backfilled_event`
  stay `pub(crate)`, exposed to integration tests only through the `test-fixtures`-gated
  `backfill_closures_for_test` seam.

## 7. Known limitations & out of scope

- **Transport-up / presence-resync re-arm** deferred; the periodic floor (300s) is the sole re-pull
  trigger beyond pull-on-spawn. Reconnect/cross-rotation recovery latency is therefore bounded by the
  floor, not immediate. A follow-up can wire `transport_epoch_rx` for prompter recovery.
- **Eager boot-spawn** deferred (§6.1) — reload happens at first voting access, not at boot.
- **Voting policy is local-only** (per-node, IPC-set, ZEB-298); persistence now keeps it across restart,
  but community-wide policy *sync* remains out of scope (separate concern, unchanged).
- **RBSR for voting** — intentionally not built (§2.3); the additive `rbsr/**` seam can be layered later
  if voting volume ever grows to justify it.
- **State-root fold** (D1 option B) — not pursued; the containment-path risk and scope outweigh the
  "reconcile for free" benefit at current scale.

## 8. Files touched

- `src-tauri/src/community_voting_persist.rs` — **new**: `PersistedVotingLog`, `save_voting_log` /
  `load_voting_log` / `voting_path_for`, quarantine-on-corrupt (mirror `community_state_persist.rs`).
- `src-tauri/src/community_voting_log_engine.rs` — `VotingReplayTracker` gains an additive `seen_coords`
  set; new `apply_backfilled_event`; `persist_now` hook called after every mutation; `read_for_backfill`
  accessor; `VotingLogEngineParams` gains `identity_dir`. Live publish/inbound paths otherwise unchanged.
- `src-tauri/src/event_loop.rs` — `VotingLogAdapterRequest` gains `read_for_backfill` + a backfill
  request channel; `spawn_voting_log_zenoh_adapter` declares the `voting/backfill` queryable
  (encrypt-on-serve@current) and the requester get-driver (current-epoch-cut-then-decrypt).
- `src-tauri/src/community_voting_backfill.rs` — **new** (or fold into engine): lean full-dump driver
  (spawn / transport-up / periodic-floor / backoff), no paging.
- `src-tauri/src/lib.rs` — `ensure_voting_engine_for` threads `identity_dir` + spawns the driver;
  `reconcile_voting_from_state` in the boot loop (load + replay + ensure-engine per community).
- `src-tauri/src/community_voting_log.rs` — no structural change (public `events`, `policy()` reused);
  possibly a small `seen`-coordinate helper.
- `src-tauri/tests/community_voting/community_voting_zenoh_integration.rs` — the three acceptance tests.
- `src-tauri/tests/community_voting/…` + wire fixtures — boot-replay test; envelope round-trip.
