# ZEB-805 — community sync stall: bounded blob re-fetch, per-call fetch budget, sync-advance observability

**Status:** design approved (Jake, 2026-07-28) — four decisions of record in §4.
**Ticket:** ZEB-805 (High). Incident family: ZEB-803 (relay-acceptor stall), ZEB-804 (per-peer
traffic staleness, merged as #566).
**Base:** `main @ c91087d9`.

---

## 1. The problem

A node that boots while its only reachable peer is down can stall community sync
**permanently**, while every health surface it exposes reports normal. Observed live
2026-07-26: 90 minutes exchanging nothing in either direction; `reachability: reachable`,
both peers `direct`, all members `online`, zero messages. Only a restart cleared it.

The stall is not a lost connection. The transport self-healed at `04:28:10`; community sync
did not.

## 2. Root cause — verified against `main @ c91087d9`

### 2.1 A ~100 KB blob is fetched under a 500 ms budget

- `content_store.rs:194` — `pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 500;`
- `lib.rs:5127-5136` — the production `RuntimeContentStore` for all three sync engines is
  built with exactly that constant.
- `content_store.rs:249-256` — `get()` sends `CasOp::GetOrFetch { timeout: self.fetch_timeout }`.
- `event_loop.rs:4902-4981` — the handler does a cache check, then on miss
  `tokio::time::timeout(timeout, fetch_via_zenoh(...))`. **Timeout → `Ok(None)`.**

500 ms bounds the *entire* fetch: zenoh query dispatch, routing to a holder, the holder's
lookup, and the full transfer back. Fleet-measured peer RTTs during the incident were
12 / 14 / 62 / 121 ms. The four dropped blobs were 101084 B → 101682 B → 109151 B → 109151 B.

### 2.2 The nested timeouts disagree by 60×

`fetch_via_zenoh` carries its **own 30-second deadline** (`event_loop.rs:7575-7598`) — under a
caller-imposed 500 ms. The inner budget is unreachable: the outer always wins by 60×.

That mismatch is the tell. The fetch layer was written believing ~30 s was a sane CAS budget;
the caller imposed 500 ms. One of those assumptions is wrong about this payload class.

### 2.3 Chronology — the 500 ms was never revisited

`DEFAULT_FETCH_TIMEOUT_MS = 500` was introduced 2026-05-02 in `7692fc07`
("Phase 3b — real harmony-content CAS", #75) and has **not been touched since**. That
predates the announce-log bloat that grew the community-state root (ZEB-813, fixed
2026-07-26) and predates cross-WAN operation. It is not a recent deliberate tuning decision.

### 2.4 The drop is terminal, and its stated justification is self-defeating

`community_state_sync.rs:3827-3835` returns `ErrPreMutation(BlobNotFound)`; the engine loop at
`:2771-2790` logs `warn!("community incoming publish dropped")`, fires `report_degraded`, and
**drops the wire bytes**. No queue, no re-fetch, no backoff.

The same premise justifies the drop in four places:

| site | text |
| -- | -- |
| `content_store.rs:190-193` | "on miss the subscriber drops the publish and CRDT eventual consistency carries recovery via the next state-root from any peer" |
| `content_store.rs:156-160` | "On timeout: `Ok(None)`." |
| `event_loop.rs:4979` | "Timeout → Ok(None) (CRDT carries recovery)." |
| `community_state_sync.rs:3823-3826` | "Cache-miss is a pre-mutation failure … CRDT eventual consistency lets the next state-root from any peer recover." |

The named recovery is a **larger blob under the same budget**. Recovery is not merely absent —
it is *anti-correlated* with the failure mode. Every failure makes the next one likelier.

### 2.5 Retry alone would not have cleared the incident

From the ticket's log, cid `3b1b60a3…, 109151B` was dropped **twice, 60 s apart**
(`04:29:11`, `04:30:11`). A re-publish of the identical blob failed again — a retry in all but
name. Corroborating: Koya received the same burst flush and fetched it fine; Ildwyn did not.

This is why the fix is both halves. Retry addresses genuinely transient misses; the budget
addresses the structural ones. Each alone provably leaves the other class wedged.

### 2.6 Three engines, three behaviours on one condition

| engine | site | on `Ok(None)` |
| -- | -- | -- |
| `fleet_sync` | `:1365-1378` | `Inbound::FetchMiss(wire)` → **bounded retry** (ZEB-705) |
| `community_state_sync` | `:3827-3835` | terminal drop + `report_degraded` |
| `mint_sync` | `:1030-1044` | terminal drop, `return Ok(())` — **caller cannot tell it failed** |

`fleet_sync` already carries the machinery this ticket needs, reviewed and tested:
`FETCH_RETRY_ATTEMPTS = 3` (`:66`), `FETCH_RETRY_DELAY_MS = 2000` (`:71`),
`FETCH_RETRY_MAX_INFLIGHT = 8` (`:81`) with a semaphore capping detached sleepers — each
retains its wire buffer, so the cap bounds memory under a publish flood. Tests at `:3083`
(retry succeeds once the blob becomes fetchable), `:3146` (attempt-exhaustion accounting),
`:3353-3427` (flood shield).

So the fix is **adoption of an in-repo precedent**, not invention.

## 3. What the evidence does and does not establish

Stated explicitly so the PR does not overclaim.

**Established:** the drop-loop mechanism (§2.1-2.5); that it is the entry into the stall.

**Not established:** that fixing it fixes the whole incident. The drop WARN fired
`04:28:12 → 04:30:11` and then went **silent for 70 minutes while the stall continued**. If
publishes kept arriving and being dropped, it would have kept firing.

Recon determined the community-state root is the `CommunityState` CRDT snapshot
(`community_state_sync.rs:3143-3150`), while channel messages ride a separate stack
(`community_channel_log` + relay-pull / RBSR). **Two paths**, coupled at two surfaces:

- channel keys derive from the membership epoch key —
  `derive_channel_key(membership_key, &community_id, channel_id)`
  (`community_channel_log_engine.rs:3050`); a node frozen at a stale epoch cannot decrypt
  post-rotation channel traffic;
- relay resolution reads community state (observable live: `passesNoRelay` is 77 on fleet
  nodes with no relay assignment versus 0 on nodes with one).

Whether that coupling caused the message stall **cannot be settled from the available
evidence** — AVALON's log carries no outbound publish trace, and the restart destroyed the
live state.

**HYPOTHESIS with a mechanism (testable, not asserted):** when the 500 ms timeout fires, the
`fetch_via_zenoh` future is dropped *at* `replies.recv_async().await`, abandoning a live zenoh
query. Zenoh then delivers a reply into a dropped receiver — which produces exactly the
`fifo: error=sending on a closed channel` ERROR the incident logged at `04:30:20`, nine
seconds after the final drop and immediately before inbound went permanently silent. The
error-production follows by construction; that abandonment *wedges* the subscriber does not.

§7's observability exists so that a recurrence settles this in one call. Raising the budget
also reduces abandonment frequency directly, whatever its downstream effect turns out to be.

## 4. Decisions of record

Jake, 2026-07-28:

1. **Both halves** — bounded retry *and* an adequate fetch budget. Neither alone suffices (§2.5).
2. **Investigate phase 2 before the spec** — done; result and its limits are §3.
3. **Per-call budget at the caller**, not a raised global constant.
4. **Ship the sync-advance observability** in the same PR (§7).

## 5. Component 1 — per-call fetch budget

### 5.1 Trait change

Add to `ContentStore`, mirroring the existing `get_local` precedent exactly (default body
delegates, so no stub or test impl changes):

```rust
/// Like `get`, but with a caller-declared network budget instead of the
/// store's default. Callers fetching a known-large payload class (community /
/// fleet / mint state-root blobs, ~100 KB and growing) MUST use this — the
/// default budget is tuned for small, latency-sensitive reads.
///
/// INVARIANT: `budget` must stay below `fetch_via_zenoh`'s own internal
/// deadline (event_loop.rs), which is the hard backstop. A budget above it
/// silently becomes that deadline.
async fn get_with_budget(
    &self,
    cid: &ContentId,
    budget: std::time::Duration,
) -> Result<Option<Vec<u8>>, ContentStoreError> {
    let _ = budget;
    self.get(cid).await
}
```

`RuntimeContentStore` overrides it to pass `budget` straight into
`CasOp::GetOrFetch { timeout: budget, .. }`. No event-loop change is needed — the handler
already takes the timeout from the op.

### 5.2 The state-root budget

```rust
/// Fetch budget for state-root blobs (community / fleet / mint). These are a
/// different payload class from the 500 ms default: ~100 KB and growing with
/// membership, frequently fetched cross-WAN.
///
/// 5 s sits ~3x above a pessimistic real fetch (100 KB at 500 kbps ≈ 1.6 s plus
/// query round-trips at the fleet's worst measured 121 ms RTT), and well below
/// both fetch_via_zenoh's 30 s backstop and the 450 s relay-pull cadence, so a
/// retry chain cannot overlap the next pass. See ZEB-805 §2.1-2.3.
pub const STATE_ROOT_FETCH_TIMEOUT_MS: u64 = 5_000;
```

`DEFAULT_FETCH_TIMEOUT_MS` stays 500 ms for every other caller — that is the point of the
per-call design, and the blast radius makes it concrete: `lib.rs:5127` builds **one**
`RuntimeContentStore` Arc and clones it to roughly ten consumers (`:5243`, `:5393`, `:5466`,
`:5546`, `:5587`, `:5701`, `:5796`, `:5846`, `:5937`, …). Raising the constant would have
silently re-tuned every one of them, including latency-sensitive paths that were never part
of this incident. Per-call keeps the change to the three call sites whose payload class
actually justifies it.

### 5.3 Call sites

`community_state_sync.rs:3827`, `fleet_sync.rs:1365`, `mint_sync.rs:1030` switch from
`get` to `get_with_budget(cid, STATE_ROOT_FETCH_TIMEOUT_MS)`.

### 5.4 Log the size against the budget

At the miss site, log `ContentId::payload_size()` beside the budget. The CID carries the size
*before* the fetch (`harmony-content/src/cid.rs:491-499` — `Debug` already prints it). A line
reading `blob 109151 B under a 500 ms budget` is self-evidently absurd, and its absence is
why this incident took three nodes and a night.

## 6. Component 2 — bounded retry in `community_state_sync` (+ `mint_sync` sweep)

Mirror the ZEB-705 pattern from `fleet_sync`, reusing its constants verbatim. Consistency
across the three engines is itself the deliverable; those numbers already survived review and
adversarial flood analysis.

- `handle_incoming_publish`'s CAS-miss arm returns a new outcome carrying the wire bytes
  (`IncomingOutcome::FetchMiss(wire)`) instead of `ErrPreMutation(BlobNotFound)`.
- The engine loop schedules a retry: acquire a semaphore permit **before** spawning (so
  detached sleepers, each retaining a wire buffer, are hard-capped), sleep
  `FETCH_RETRY_DELAY_MS`, `try_send` back into a re-injection channel. On permit saturation or
  a full/closed channel: drop and count, never block.
- On exhaustion: the existing terminal drop, with the existing `report_degraded`.
- The replay tracker stays un-advanced throughout — already true today (`:3806-3810`, ZEB-750:
  every early return drops the `CommitTicket`), so re-delivery remains admissible. **This is
  load-bearing for the retry and must be pinned by a test.**

`mint_sync` gets the same treatment. Its current `return Ok(())` is the worst of the three —
the caller cannot distinguish a swallowed fetch failure from success.

Counters, mirroring `fleet_sync`: `fetch_retries_scheduled`, `fetch_retries_dropped`,
`fetch_retry_inflight_peak`, plus `fetch_retries_exhausted`. These feed §7.

## 7. Component 3 — sync-advance observability

### 7.1 The two fields whose divergence names the bug

The load-bearing design point. Track **both**, per community:

| field | meaning |
| -- | -- |
| `lastInboundMs` | wall ms of the last inbound publish *received*, whatever became of it |
| `lastAdvanceMs` | wall ms of the last inbound publish that actually **merged** (step 14 mutation) |

`lastInboundMs` advancing while `lastAdvanceMs` stays frozen **is** the drop-loop signature,
and reading those two numbers side by side would have identified this incident immediately.
Either field alone is insufficient: `lastInboundMs` alone cannot distinguish "applying fine"
from "dropping everything", and `lastAdvanceMs` alone cannot distinguish "wedged" from
"genuinely quiet, nobody is publishing".

### 7.2 Surface

Extend `network_health_snapshot` with a `communitySync` array, one entry per active community:

```
communityId, lastInboundMs, lastAdvanceMs, staleness,
fetchMisses, fetchRetriesScheduled, fetchRetriesExhausted, fetchRetriesDropped
```

`staleness` reuses ZEB-804's tier vocabulary and constants (`fresh` / `quiet` / `dark`,
5 min / 30 min, derived as-of snapshot) so operators read one staleness idiom across the
whole surface rather than two. It is computed from `lastAdvanceMs` — the honest "is sync
working" signal — with `null` when the community has no peers to sync with, mirroring
ZEB-804's `null`-under-`noConnection` rule.

All fields additive, camelCase, `Option` / `#[serde(default)]`, serde-pinned with the
snake-leak sweep the ZEB-804 work established. TS types extended by hand (no `gen/`).

### 7.3 Regression pin

The incident replay, as a permanent test: inbound publishes arriving and being dropped must
render `lastInboundMs` advancing, `lastAdvanceMs` frozen, and `staleness: dark`. Verified
discriminating against the named mutations (tier derived from `lastInboundMs` instead of
`lastAdvanceMs`; tier emitted only when a fetch miss is present; derivation deleted).

## 8. Component 4 — adjacent corrections

1. **The four false-premise comments** (§2.4) are rewritten to state what is actually true:
   a miss is retried under a bounded budget, and the "next state-root recovers" reasoning is
   removed — it is the claim this incident falsified.
2. **The unearned-reassurance log line** (`event_loop.rs:3663`): *"startup root query: no
   responder — retrying with backoff; live push also catches up on next gateway publish"*.
   Both clauses were false in the incident. Reworded to assert only what the code guarantees.
   A log line that asserts a fallback covers the failure actively suppresses investigation.
3. **The root-query forever-retry contract.** The driver is documented as re-invoking
   "forever (600 s cap)"; observed behaviour was 11 attempts and then stop. Trace it: either
   restore the documented contract or correct the comment. **If the code turns out to be
   correct and the comment wrong, fixing the comment is the whole fix** — do not invent a
   retry loop to match a comment.

## 9. Testing

- Retry succeeds once the blob becomes fetchable (mirrors `fleet_sync.rs:3083`).
- Attempt-exhaustion accounting: initial attempt + `FETCH_RETRY_ATTEMPTS` retries, then a
  terminal drop with `report_degraded` (mirrors `:3146`).
- Flood shield: concurrent sleepers stay ≤ `FETCH_RETRY_MAX_INFLIGHT` under a publish flood
  from N > cap distinct publishers (mirrors `:3353-3427`).
- **Tracker-un-advanced pin:** after a CAS miss and after retry exhaustion, re-delivery of the
  same frame is still admitted. This invariant is what makes retry safe; it is currently
  incidental and must become explicit.
- Budget plumbing: `get_with_budget` reaches `CasOp::GetOrFetch` with the caller's duration,
  not the store default (assert on the op, as `content_store.rs:363-385` does for `GetLocal`).
- Budget-invariant pin: `STATE_ROOT_FETCH_TIMEOUT_MS` < `fetch_via_zenoh`'s internal deadline.
- Incident replay for the observability (§7.3).
- Serde pinning for the new DTO fields + snake-leak sweep.

## 10. Wire compatibility

No wire-format change. The retry re-injects bytes already received; the budget is local
policy; the DTO additions are additive and `Option`al. `lastSeenMs`-style absorption is not
involved.

## 11. Out of scope

- **ZEB-814** (root blob is one `ContentId` capped at 1 MiB; crosses at ~1,500 members).
  Same growth axis from the other end. A bigger budget buys headroom, not immunity — that
  ticket remains the durable fix and should record this one as evidence.
- **Chunked / resumable state-root transfer**, and incremental state deltas. Considered and
  deferred as its own design (option 4 at the design call).
- **ZEB-803's acceptor watchdog.** Adjacent, still open, and its trigger condition becomes
  better-founded once §7 exists.
- **Adaptive / size-derived budgets.** `ContentId::payload_size()` makes this feasible later;
  deliberately not taken now (adaptive timeouts are a known source of test flakiness), but
  §5.4 logs the size so the data to justify it accumulates.
- The **phase-2 causation question** (§3). Not closed by this work; §7 is the instrument that
  closes it on recurrence.
