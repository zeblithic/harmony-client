# ZEB-790 — HLC bounded causal adoption

**Status:** approved (design)
**Ticket:** [ZEB-790](https://linear.app/zeblith/issue/ZEB-790)
**Relates:** ZEB-788 (the skew that made this observable), ZEB-792 (forward-skew bound whose safety premise this changes), ZEB-256 (squatting defence), ZEB-750 (CommitTicket admission), ZEB-267 (atomic HLC reservation)
**Date:** 2026-07-31

## 1. Problem

The client's `Hlc` (`owner_state_types.rs:311-345`) is named a Hybrid Logical Clock but
provides **per-device monotonicity and total order only** — not cross-device
happens-before. Both mint paths derive `prev` from this device's own previous stamp; a
remote peer's stamp is not a parameter to the mint at all (`dm_outbox.rs:3216 next_hlc`,
`community_state_sync.rs:3343 community_hlc_tick`, kernel `harmony_crdt_sync::HlcTick::next`
— inputs are exactly `(prev, wall_ms)`).

Field evidence (ZEB-788 A/B, 2026-07-25): Ildwyn received a message from AVALON stamped
`1785021612212` and, ~228 ms later in real time, minted its own stamp at `1785021611591`
— **621 ms below a stamp it had just verified and applied**, with `logical: 0`. A
conforming HLC cannot do that. The skew source was an ordinary unsynchronised Windows
Time service (~1 s), not misconfiguration.

Per AVALON's sharpening on the ticket: this is not a broken merge — **there is no merge**.
The ticket is therefore a decision: implement the contract the name promises, rename to
the per-device truth, or re-order the consuming surfaces structurally.

### Why it matters

- Any surface that sorts cross-device by HLC can causally invert under skew:
  the channel feed (`channel-message-service.ts:710 sortedInsertIndex`),
  `CharterView.svelte:87` (`pollCreateHlcMs`), `StatementVoteList.svelte:40`
  (`createdAtHlcMs`).
- Tier-3 voting enforces a **global cross-device** monotonic `last_received_hlc`
  (`community_voting_tier3.rs:457-467`): one accepted future-stamped event locks every
  honest device's subsequent events out with `HlcNotMonotonic` until real clocks catch up.
- Every future consumer will reasonably assume the causal contract from the name.

## 2. Decision

**Approach B — bounded causal adoption, layered** (chosen over doc-only and over a full
textbook merge; see §9 Alternatives):

1. **Truthful docs** — `Hlc`'s doc-comment states the real guarantee.
2. **Bounded merge-on-receive** — one shared observed-remote-wall floor, fed at the
   verified accept sites, read at the mint seams, clamped to a small cap.
3. **Deterministic ties at the UI** — the two governance sorts get the full
   `(wallMs, logical, deviceId)` tuple instead of `wallMs` alone.

### The new contract

`Hlc` keeps its structural guarantee — every mint strictly exceeds this device's previous
stamp — and gains: **if this device verified-and-applied a remote event with wall `W`
before minting, and `W < now + CAP` (strict — at exactly `now + CAP` the `+1` floor
clamps to `W`, not past it), the next mint's wall is `> W`.**

Consequence: cross-device happens-before holds whenever inter-device skew ≤ CAP. Beyond
CAP the clock degrades gracefully to today's per-device behaviour — documented, bounded,
not silent.

```rust
/// How far ahead of this device's own wall clock the mint may be pulled by
/// adopting a verified remote stamp. See §6 for the budget analysis.
pub const HLC_ADOPT_FORWARD_CAP_MS: u64 = 5_000;
```

**Why 5 s:** the observed failure class is ~1 s (ZEB-788: 621 ms inversion, ~41 ms/hour
drift) — 5 s covers it with 5× headroom. The tightest wall-time-coupled consumer budget
is **60 s** (invite/open-join forward windows, `open_join_admit.rs:164-169`,
`community_invite.rs:1836-1846`) — 5 s leaves 12× margin. The 30-min peer-stamp bounds
(`ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` et al.) are weakened by ≤ CAP ms ≈ 0.3%: the
maximum forward pull of a mint above true `now` is exactly `CAP` (not `CAP + 1`) —
`merged_now(now) = max(now, min(floor, now + CAP)) ≤ now + CAP`, so the `+1` in the
stored floor never survives the `now + CAP` clamp as extra forward skew.

## 3. The floor — `HlcAdoptFloor`

A session-only high-water: `Arc<AtomicU64>` holding **max verified remote `wall_ms` + 1**.

```rust
pub struct HlcAdoptFloor(Arc<AtomicU64>);   // 0 = nothing observed

impl HlcAdoptFloor {
    /// Feed: AcqRel fetch_max of remote_wall.saturating_add(1).
    pub fn observe(&self, remote_wall_ms: u64);
    /// Read: max(now, min(floor, now + HLC_ADOPT_FORWARD_CAP_MS)) (Acquire load).
    pub fn merged_now(&self, wall_now_ms: u64) -> u64;
}
```

**The `+1` is load-bearing.** We adopt only the wall, not `logical`. A remote stamp
`(W, l>0, their_dev)` would out-sort a naive adoption minted at `(W, 0, our_dev)` —
the derived `Ord` compares `logical` second. Storing `W+1` makes the adopted mint
strictly exceed the observed stamp on the **first** tuple component, so `logical` and
`device_id` never matter. Cost: ≤ 1 ms wall inflation per causal hop, all inside the
CAP clamp. (Observed fleet traffic is universally `logical: 0`, but correctness must not
depend on that.)

**Algebra of `merged_now(now)`:**

| Case | Result | Meaning |
|---|---|---|
| `floor ≤ now` | `now` | remote is behind us — identity, today's behaviour |
| `now < floor ≤ now+CAP` | `floor` | adopt: next mint wall strictly exceeds every observed `W` |
| `floor > now+CAP` | `now+CAP` | clamp: beyond-cap skew not fixed, damage bounded |

An **empty floor is the identity** (`merged_now(now) == now`), so every existing mint
test passes unchanged by constructing a fresh floor.

**Not persisted.** It is re-learned from live traffic within seconds of boot, the clamp
is applied against current `now` at every read anyway (so even a stale floor would be
harmless), and session-only avoids a new disk format. Monotonicity across restart is
already owned by the persisted replay trackers, not the floor.

## 4. Feed sites (write side) — verified accepts only

| # | Source | Where | Trust binding |
|---|---|---|---|
| 1 | Community state | after `tracker.commit(replay_ticket)` at `community_state_sync.rs:4466-4474` | Ed25519 `verify_publisher_sig`, member Joined-at-HLC, TOCTOU re-check |
| 2 | Channel log | in `ChannelLogEngine::process_inbound_packet`, after step 2c's atomic `check_and_advance` succeeds — i.e. `community_channel_log_engine.rs:1715-1738` — so replay check **and** signature verify (step 2b) both passed | Ed25519 `verify_strict` against enrolled author device keys |
| 3 | Owner-state fleet | after `ctx.replay_tracker.lock().await.commit(ticket)` at `fleet_sync.rs:1442-1445` | fleet AEAD (own sibling devices only) |
| 4 | Community voting inbound (ZEB-843) | after V6-membership + Ed25519 verify + apply + record all succeed, in `VotingLogEngine::process_inbound` (`community_voting_log_engine.rs:2810-2823`) and its backfill twin `apply_backfilled_event` (`:2884-2895`) | Ed25519 `verify_voting_event` (V6 membership-at-HLC), same trust class as channel-log |
| 5 | Mint-state-root sync inbound (ZEB-845) | after the replay-tracker advance in `MintSyncEngine`'s verified-apply path, `mint_sync.rs:1260-1265` | fleet AEAD (own sibling devices only), same trust class as owner-state fleet |

**Invariant (mirrors the tracker's censorship-defence discipline,
`community_state_sync.rs:3743-3752`): a rejected or unverified frame never moves the
floor.** The feed lives structurally *after* the commit/record on each path — a rejection
returns before reaching it, exactly as rejections cannot advance the replay watermark.
This rejection-safety is **structural and unconditional** — it holds regardless of
threading, because a rejected frame never reaches the `observe` call at all.

**Visibility is a separate, weaker property.** `observe` and the mint seams run on
different tasks (inbound sync applies events; IPC handlers mint). The floor is a single
atomic (`AcqRel` feed / `Acquire` read), so per-location coherence guarantees a mint
never reads a floor value *older* than one it already saw, and a mint that observes a
given floor value strictly exceeds every remote wall folded into it. What the floor does
**not** promise is *real-time* cross-task visibility: a mint that races an in-flight
`observe` on another task may still read the pre-`observe` value and produce the very
inversion this feature reduces — now confined to a sub-microsecond window and
self-correcting as the atomic propagates. That is acceptable by construction: the floor
is a best-effort session hint (re-learned from live traffic, clamped against current
`now`), not a lock-synchronized happens-before edge. Per-device structural monotonicity —
the guarantee callers actually depend on — is owned by the replay trackers, not the floor.

**Deliberately excluded in v1:**

- **DM `sent_at`** (`dm_inbox_ingest.rs:377/:935/:1191`) — feeding it would widen the
  nudge surface from "community members + own fleet" to *any friend*, and DM thread
  ordering is driven by locally-minted `received_at`, so the causal payoff is small.
  Revisit if cross-participant DM ordering becomes a surface.
- Unverified/synthetic stamps of any kind (pkarr-derived, payload-claimed).

(Tier-3 voting inbound and mint-sync inbound were both excluded here in v1; both are now
fed — see rows 4-5 above, ZEB-843 and ZEB-845.)

## 5. Mint seams (read side)

`reserve_next_hlc_for_device` (`dm_outbox.rs:3295`) gains a `floor: &HlcAdoptFloor`
parameter and computes `floor.merged_now(wall_now_ms)` before the tick — preserving the
single-lock ZEB-267 atomicity (the floor is a lock-free atomic read). This is a
**compiler-driven mechanical sweep** of the ~90 call sites; each already clones the
shared tracker off `NodeState`, and the floor (a new `NodeState` field beside
`hlc_tracker`, constructed in `start_node`) rides alongside.

Same widening for the sibling mint seams:

- `community_state_sync.rs:3356 next_hlc(ctx)` — `InternalCtx` gains the field; the
  floor is read inside the already-held tracker lock; `community_hlc_tick` stays a pure
  `(prev, wall_ms, device_id)` function so the ZEB-750 non-vacuity test still binds.
- `fleet_sync.rs:1113 mint_next_hlc` / `:1139 peek_next_hlc` / `:1153 compute_next_hlc`.
- `mint_sync.rs:983 next_hlc_mint` (ZEB-845) — reads `floor.merged_now` at line 991 before
  computing the tick; the verified-inbound apply feeds the floor back at `mint_sync.rs:1265`
  (§4 row 5).
- The three open-coded acceptor mints: `profile_broadcast.rs:445`,
  `iroh_friend_acceptor.rs:1991`, `iroh_pex_acceptor.rs:359` (ctx structs gain the field).
- The `send_dm` split mint (`lib.rs:14596` caller-supplied `prev` + pure `next_hlc` at
  `dm_outbox.rs:1008`) — the caller applies `merged_now` to the wall it passes.

The parameter is **not** `Option` — tests construct a fresh (empty) floor, prod threads
the real one; there is no `None` to silently forget on a production path.

**Untouched:** the upstream `HlcTick` kernel and its pin
(`community_state_sync.rs:6310/:6342`); every replay lane and its keyspace
(per-device admission is orthogonal to what wall the mint reads);
`record_local`'s local-device `debug_assert` (`dm_outbox.rs:3262`); the `Hlc` wire shape
(no new field — adding one would change the signature preimage of every signed event
type and break ~50 hex fixture pins; mint *values* are pinned nowhere).

**Known bypasses, out of scope (see §10):** the hand-rolled mints at `lib.rs:8402/:44072`,
`owner_quorum_sync.rs:455`, `fleet_net.rs:479`, and `community_state_sync.rs:2443` do not
adopt in v1. They are LWW bump paths deriving from `prev`; leaving them per-device costs
ordering only on their own narrow surfaces. (`mint_sync.rs:976`'s hand-rolled mint was
listed here at v1 time — ZEB-845 wired it; it is no longer a bypass, see the sibling
mint-seam list above and §4 row 5.)

## 6. Consumers updated in tandem

1. **`community_membership.rs:5490-5509`** (`ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`) — its
   doc's safety argument is literally "`reserve_next_hlc_for_device` derives it from this
   device's previous stamp alone (ZEB-790). Should that ever change to merge remote
   walls, this bound must gain a clamp or it weakens." Rewrite: `now_ms` is now
   peer-influenced by **≤ `HLC_ADOPT_FORWARD_CAP_MS` + 1 ms**, so the effective forward
   bound is `30 min + CAP` (≈ 0.3% weakening — acceptable, documented).
2. **Budget-relation pin** — a test asserting `HLC_ADOPT_FORWARD_CAP_MS` stays far below
   the tightest consumer budgets (e.g. `CAP * 12 <= 60_000` for the invite window and
   `CAP * 360 <= ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`), so nobody widens CAP past the
   analysis silently.
3. **Analyzed, not changed** (all shift by ≤ CAP against windows of hours-to-days):
   epoch dual-read cut (`owner_commands.rs:337-352`), 48 h recovery time-lock
   (`community_membership.rs:5541`), 90-day poll archival
   (`community_voting_log.rs:1191`), backup staleness nag (`backup_state.rs:125`),
   pending-join 30-day aging floor (`community_membership.rs:2593`), RCH4 payload-vs-HLC
   skew checks (`community_membership.rs:4938/:4983` — budget 30 min, our own announces
   stay ≤ CAP divergent).
4. **Deliberately non-adopting** (ZEB-843 minor #3): `current_hlc_estimate`
   (`community_voting_log_engine.rs:626`) — the tier-3 deadline/expiry comparator — reads
   raw `SystemTime::now()`, never `floor.merged_now`. This is intentional, not an
   oversight: it produces a ≤ `HLC_ADOPT_FORWARD_CAP_MS` (5 s) same-instant asymmetry
   against `reserve_next_local_hlc` (which does adopt), but the asymmetry is conservative
   in direction — a deadline is never judged *past* earlier than the local clock honestly
   says, only (at most) later — so it stays safely inside the analyzed consumer-budget
   envelope above.

**Side benefit (state in code comments where relevant):** the tier-3 lockout window
shrinks from `skew` to `max(0, skew − CAP)` — for ordinary skew it vanishes — and the
clamp converts the previously **unbounded** accepted-future-stamp clock-drag
(`community_voting_log_engine.rs:1249` acknowledges the hazard) into a ≤ CAP nudge. ZEB-843
wired the voting-inbound accept path itself as a feed site (§4 row 4: `process_inbound`
at `community_voting_log_engine.rs:2823`, and its backfill twin `apply_backfilled_event`
at `:2895`), so this now applies unconditionally to any verified voting-inbound accept —
not only when the skewed peer's clock happened to be learned via a different fed path
first.

## 7. UI layer — deterministic ties

Today the tier-3 DTOs truncate to `wall_ms` (`lib.rs:55596 created_at_hlc_ms`,
`:55647/:55769 poll_create_hlc_ms`), so the two governance sorts compare milliseconds
only and ties fall back to content-hash order (the `BTreeMap<[u8;32], _>` iteration
order) — the exact bug class `community_voting_conviction.rs:673-678` (`HlcOrdinal`,
"CR R3 Major") already fixed once elsewhere.

- DTOs gain the full tuple alongside the kept `*HlcMs` fields (compat), as **flat
  scalar fields** (not a nested object — chosen so the additive serde change needs no
  new struct and the existing `*HlcMs` wall stays the primary field): `pollCreateHlcLogical:
  u32` + `pollCreateHlcDeviceId: String` on `Tier3PollSummary`/`Tier3PollExport`,
  `createdAtHlcLogical` + `createdAtHlcDeviceId` on `DeliberationStatementExport`
  (`src/lib/types/voting.ts`; TS fields optional with `?? 0` / `?? ''` fallbacks so
  existing fixtures need no churn). The FE reassembles the tuple at the sort call.
- `CharterView.svelte:87`, `StatementVoteList.svelte:40` sort via the existing
  `src/lib/hlc.ts:14 compareHlc`; the backend pre-sort at `lib.rs:55776` moves to the
  same full-tuple key.
- New FE ordering tests pin tie behaviour (none exist today — both components' tests
  assert counts and labels only).

## 8. Testing

**Unit (floor):** observe/merged_now algebra per the §3 table; empty-floor identity;
saturating `+1` at `u64::MAX`.

**Unit (mint):** adopts within cap (mint wall strictly exceeds observed `W` with
`l > 0` — the `+1` rule); clamps beyond cap; per-device monotonicity preserved through
adoption; existing mint tests unchanged via fresh floors (incl.
`lib.rs:37370/:44655` `wall == wall_now` assertions, which hold under an empty floor).

**Unit (feed discipline):** a rejected community publish does not move the floor; a
signature-failed channel event does not move the floor (parity with
`community_channel_log_engine.rs:1569-1573`'s failed-auth-does-not-mutate rule).

**Integration (the repro):** skew-injection — verify-and-apply an event stamped
`now + 600 ms`, mint, assert our stamp exceeds it. This is the exact ZEB-788 621 ms
inversion, made impossible.

**Budget pins:** the §6.2 relation test.

**Suites:** wire fixtures unaffected by design (mint values unpinned, `Hlc` shape
untouched). Iteration via `scripts/test-select` (paste its `round=… bucket=…` summary
line into task reports for auditability, per CLAUDE.md's iterative-test-selection
convention); full `--workspace --all-targets` sweep before PR.

## 9. Alternatives considered

- **A. Document-only** — zero risk, leaves the observed inversion in place forever;
  both fleet reviewers (Ildwyn, AVALON) leaned implement. Folded in as layer 1 rather
  than chosen alone.
- **C. Full textbook merge (30-min bound or unbounded)** — rejected on the blast-radius
  inventory: breaks the 60 s invite/open-join budget for marginally-fast devices, voids
  the `community_membership.rs:5490` premise outright, can close the epoch dual-read
  window early (a survivor device losing decrypt), and effectively shortens the 48 h
  recovery time-lock. `wall_ms` is load-bearing as ≈real-time in too many places.
- **Ticket Option 3 (structural ordering)** — unavailable: deliberation statements and
  polls carry no parent/lineage field (`community_voting_core.rs:119-124` payload is
  `{poll_id, text}`); adding one is a signed-payload wire change. Channel messages do
  have `reply_to`, but they are not the ticket's surface. Possible future work.

## 10. Scope & non-goals

**In scope:** `HlcAdoptFloor` module + `NodeState` field; three feed sites; mint-seam
widening sweep; §6 consumer updates; §7 DTO/FE ordering; `Hlc` doc rewrite; tests.
Client-only — the upstream 10-crate lockstep rev does not move.

**Non-goals (candidate follow-up tickets, to be filed only after review):**

1. Consolidating the five hand-rolled mints onto the kernel — including the
   `community_state_sync.rs:2443` anomaly, which can emit a stamp carrying **another
   device's id** (pre-existing, unrelated to adoption).
2. DM-path floor feed (§4 exclusion).
3. Forward-jump drift telemetry — today's clock-anomaly surfacing is backward-only
   (`owner_state.rs:939` regression banner; nothing detects a fast peer).
4. Structural parents for deliberation statements (wire change).

## 11. Security summary

- **Nudge surface:** community members with valid enrolment (Ed25519-bound), channel
  authors (same), community voting members (same — ZEB-843, `process_inbound` /
  `apply_backfilled_event`), and own fleet siblings (AEAD; the `fleet_sync.rs` feed fires from every fleet-doc
  engine — owner-state, notes, relay-hold, trust, quorum, etc. — all the same
  own-fleet trust class), plus own-fleet mint-state siblings (AEAD — ZEB-845,
  `MintSyncEngine`; same own-fleet trust class, just a distinct fleet doc). No anonymous
  or unverified path feeds the floor.
- **Worst-case malicious influence:** a verified member stamping far-future drags every
  adopter's mints forward by **at most CAP against each device's own clock**,
  non-compounding (each device clamps against its own `now`). Compare today: the same
  stamp locks tier-3 polls unboundedly and (pre-clamp analysis) would have dragged a
  textbook merge arbitrarily far.
- **ZEB-256 not reopened:** adoption changes only the wall input to the local mint; the
  replay keyspaces, `(publisher_addr, device_id)` namespacing, and CommitTicket
  discipline are untouched. Squatting a *slot* remains impossible; nudging the *clock*
  is newly possible but capped at 5 s.
- **Rejection paths are inert** by construction (§4 invariant) — the floor cannot be
  moved by traffic that fails verification, so it adds no new censorship or DoS lever.

## 12. References

- Mint kernel: `harmony-crdt-sync/src/hlc.rs:98` (`HlcTick::next`); client seams
  `dm_outbox.rs:3216/:3295`, `community_state_sync.rs:3343/:3356`,
  `fleet_sync.rs:1113/:1153`.
- Accept/commit sites: `community_state_sync.rs:4054/:4459`,
  `community_channel_log_engine.rs:1503/:1574`, `fleet_sync.rs:1348/:1422`.
- Keyspace + squatting defence: `community_state_sync.rs:823-846`,
  `replay_admission.rs:265-302`.
- Wall-coupling inventory highlights: `community_membership.rs:5490-5509/:5926`,
  `open_join_admit.rs:164`, `community_invite.rs:1836`, `owner_commands.rs:337`,
  `community_voting_tier3.rs:457/:1081`.
- Prior-art clamps: `community_address_book.rs:38/:175` (5-min clamp-and-store),
  `reachability_resolver.rs:46/:421` (5-min, incl. the one existing `Hlc.wall_ms` clamp).
- Fleet evidence: ZEB-788 A/B (`#fleet-ops` 2026-07-25 23:20), ticket + AVALON comment.
