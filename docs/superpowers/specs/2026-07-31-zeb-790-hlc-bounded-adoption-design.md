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
(`ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` et al.) are weakened by ≤ CAP + 1 ms ≈ 0.3%.

## 3. The floor — `HlcAdoptFloor`

A session-only high-water: `Arc<AtomicU64>` holding **max verified remote `wall_ms` + 1**.

```rust
pub struct HlcAdoptFloor(Arc<AtomicU64>);   // 0 = nothing observed

impl HlcAdoptFloor {
    /// Feed: relaxed fetch_max of remote_wall.saturating_add(1).
    pub fn observe(&self, remote_wall_ms: u64);
    /// Read: max(now, min(floor, now + HLC_ADOPT_FORWARD_CAP_MS)).
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
| 1 | Community state | after `tracker.commit(replay_ticket)` at `community_state_sync.rs:4457-4460` | Ed25519 `verify_publisher_sig`, member Joined-at-HLC, TOCTOU re-check |
| 2 | Channel log | in `ChannelLogEngine::process_inbound_packet`, after `verify_channel_event` returns Ok — i.e. its step-8 `record` (`community_channel_log.rs:1569-1574`) has committed, so replay check **and** signature verify both passed | Ed25519 `verify_strict` against enrolled author device keys |
| 3 | Owner-state fleet | after `ctx.replay_tracker.lock().await.commit(ticket)` at `fleet_sync.rs:1422` | fleet AEAD (own sibling devices only) |

**Invariant (mirrors the tracker's censorship-defence discipline,
`community_state_sync.rs:3743-3752`): a rejected or unverified frame never moves the
floor.** The feed lives structurally *after* the commit/record on each path — a rejection
returns before reaching it, exactly as rejections cannot advance the replay watermark.

**Deliberately excluded in v1:**

- **DM `sent_at`** (`dm_inbox_ingest.rs:377/:935/:1191`) — feeding it would widen the
  nudge surface from "community members + own fleet" to *any friend*, and DM thread
  ordering is driven by locally-minted `received_at`, so the causal payoff is small.
  Revisit if cross-participant DM ordering becomes a surface.
- **Tier-3 voting inbound** (`community_voting_log_engine.rs`'s `process_inbound_packet`) —
  a verified accept path of the same trust class as the channel-log feed, NOT fed in
  v1 (surfaced by the final branch review). Consequence: the §6 lockout side-benefit
  engages only when the skewed peer's clock is learned via a fed path; a voting-only
  interaction retains today's `HlcNotMonotonic` behavior. Follow-up: ZEB-843.
- Unverified/synthetic stamps of any kind (pkarr-derived, payload-claimed).

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

**Known bypasses, out of scope (see §10):** the hand-rolled mints at `mint_sync.rs:976`,
`lib.rs:8402/:44072`, `owner_quorum_sync.rs:455`, `fleet_net.rs:479`, and
`community_state_sync.rs:2443` do not adopt in v1. They are LWW bump paths deriving from
`prev`; leaving them per-device costs ordering only on their own narrow surfaces.

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

**Side benefit (state in code comments where relevant):** the tier-3 lockout window
shrinks from `skew` to `max(0, skew − CAP)` — for ordinary skew it vanishes — and the
clamp converts the previously **unbounded** accepted-future-stamp clock-drag
(`community_voting_log_engine.rs:1249` acknowledges the hazard) into a ≤ CAP nudge.
Caveat (ZEB-843): this engages only when the skewed peer's clock was learned via a
fed path (§4) — the voting inbound path itself does not feed the floor in v1.

## 7. UI layer — deterministic ties

Today the tier-3 DTOs truncate to `wall_ms` (`lib.rs:55596 created_at_hlc_ms`,
`:55647/:55769 poll_create_hlc_ms`), so the two governance sorts compare milliseconds
only and ties fall back to content-hash order (the `BTreeMap<[u8;32], _>` iteration
order) — the exact bug class `community_voting_conviction.rs:673-678` (`HlcOrdinal`,
"CR R3 Major") already fixed once elsewhere.

- DTOs gain the full tuple alongside the kept `*HlcMs` fields (compat):
  `pollCreateHlc: {wallMs, logical, deviceId}` on `Tier3PollSummary`/`Export`,
  `createdAtHlc` on `DeliberationStatementExport` (`src/lib/types/voting.ts`).
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
`community_channel_log.rs:1569-1573`'s failed-auth-does-not-mutate rule).

**Integration (the repro):** skew-injection — verify-and-apply an event stamped
`now + 600 ms`, mint, assert our stamp exceeds it. This is the exact ZEB-788 621 ms
inversion, made impossible.

**Budget pins:** the §6.2 relation test.

**Suites:** wire fixtures unaffected by design (mint values unpinned, `Hlc` shape
untouched). Iteration via `scripts/test-select`; full `--workspace --all-targets` sweep
before PR.

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
  authors (same), and own fleet siblings (AEAD; the `fleet_sync.rs` feed fires from every fleet-doc
  engine — owner-state, notes, relay-hold, trust, quorum, etc. — all the same
  own-fleet trust class). No anonymous or unverified path feeds
  the floor.
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
  `community_channel_log.rs:1503/:1574`, `fleet_sync.rs:1348/:1422`.
- Keyspace + squatting defence: `community_state_sync.rs:823-846`,
  `replay_admission.rs:265-302`.
- Wall-coupling inventory highlights: `community_membership.rs:5490-5509/:5926`,
  `open_join_admit.rs:164`, `community_invite.rs:1836`, `owner_commands.rs:337`,
  `community_voting_tier3.rs:457/:1081`.
- Prior-art clamps: `community_address_book.rs:38/:175` (5-min clamp-and-store),
  `reachability_resolver.rs:46/:421` (5-min, incl. the one existing `Hlc.wall_ms` clamp).
- Fleet evidence: ZEB-788 A/B (`#fleet-ops` 2026-07-25 23:20), ticket + AVALON comment.
