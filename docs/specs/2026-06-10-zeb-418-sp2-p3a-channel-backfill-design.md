# ZEB-418 SP2 P3a — Community channel-log backfill (join + reconnect)

**Status:** approved design (Jake, 2026-06-10)
**Parent:** ZEB-418 (SP2 Butler), epic ZEB-416. Folds in **ZEB-403** (pre-join channel history).
**Predecessors:** P1 inbound deposit (PR #221, squash `e39a3339`, spec `2026-06-09-zeb-418-sp2-butler-design.md` §4 as amended) and P2 outbound hold (PR #222, squash `266cd823`, spec `2026-06-10-zeb-418-sp2-p2-outbound-hold-design.md`, D11–D18).
**Decision numbering** continues the SP2 sequence: D19–D26 below.

## 1. Goal

A community member who was offline — or who joined after the fact — receives the channel history they are entitled to, as soon as **any** device holding that history is online. "Posts land and backfill" from the SP2 framing, delivered as one backfill path serving both the reconnect case and the new-joiner case (ZEB-403).

## 2. Reframe vs. the umbrella spec (D22)

The umbrella spec's P3 line ("same deposit machinery, payload scope widened") predates two findings from code exploration on `main@266cd823`:

1. A backfill mechanism **already exists**: `ChannelLogEngine::request_backfill()` fires a zenoh queryable get on `harmony/community/{id_hex}/{channel_id_hex}/since/{hlc}` (`community_channel_log_engine.rs:667-679`); every member device declares the matching queryable (`event_loop.rs:5449`); replies are wire-identical to live broadcasts and enter the normal inbound path (`process_inbound_packet`) where the replay tracker dedupes.
2. Community history is **multicast**: any online device of any member holds the log and can serve it. Pull-on-demand beats sender-push here — there is no single recipient whose butler must be found.

**D22:** P3a is built on the existing queryable pull path. The P1 deposit machinery, the P2 outhold side-table, pkarr advertisement, and the butler-set are **not touched**. The "butler" property — the author's own siblings serving history while the author is offline — falls out of every-device queryable serving, which already exists.

What is missing today, and what P3a builds: **triggers** (nothing invokes `request_backfill` on join — hence ZEB-403), a **retry latch** (a one-shot fire-and-forget query loses when no holder is online), and **verification parity** (backfilled events must face the same gates as live ones).

## 3. Decisions

| # | Decision |
|---|---|
| **D19** | P3 is decomposed: **P3a (this spec)** = channel-log backfill; **P3b (later cycle, own spec)** = group-DM butler fan-out + co-community admission. P4 sealed relay unchanged. |
| **D20** | New joiners receive **full history** (subject to retention/tombstones as they exist; none today). Bounded windows and per-community configurability are explicitly later upgrades on the same plumbing. |
| **D21** | No-holder case = **eventual + retry**: debounced re-request until satisfied; converges when any holder appears. Truly disjoint fleets remain P4's problem. |
| **D22** | Pull via the existing queryable path; no deposit/outhold/pkarr involvement (see §2). |
| **D23** | **One backfill path, three triggers** (§4.1): join completion (`since: None`), engine start/reconnect (`since: local watermark`), new-channel discovery (`since: None`). ZEB-403 is implemented by the first trigger and closed by this work. |
| **D24** | Retry latch is **in-memory**, per `(community, channel)`. Satisfied = at least one holder **completes** a reply, including a complete-but-empty one (a served "nothing" is an answer; an unanswered query is not). Backoff 30 s doubling to a 10-minute cap, retrying at cap while the engine runs. Restart re-requests from scratch — idempotent and cheap, so no persistence. |
| **D25** | **Verification parity:** backfilled events enter through the same `process_inbound_packet` path as live events — ZEB-399 author-auth, replay-tracker dedupe, and membership-at-HLC (`snapshot_at`) gating. If the membership-at-HLC check is missing from the shared path it is added **to the shared path**, never as a backfill-only fork. |
| **D26** | **No new wire formats.** Backfill replies reuse the live broadcast shape; no new pinned fixtures. The only new wire surface is the (existing) queryable key expression. |

## 4. Components

### 4.1 Backfill triggers — one function, three call sites

A single entry point (per community engine): `schedule_backfill(channel_id, since: Option<Hlc>)`.

1. **Join completion** (the ZEB-403 case): after a community's membership materializes post-redeem, fire for every channel in the channel-config CRDT with `since: None` → full history (D20).
2. **Engine start / reconnect:** per known channel, fire with `since: <highest locally-persisted HLC for that channel>` → missed-posts catch-up. Same call, non-None watermark — this is what makes it "one backfill path, not two." "Reconnect" includes mid-session transport recovery: if the channel-log subscriber has a re-subscribe/recovery hook, that hook also resets the latch to unsatisfied and re-fires with the current watermark (events published during the outage would otherwise be missed until the next restart). Whether such a hook exists is verified at plan time; if none does, engine-start-only is the v1 behavior and the gap is noted in the plan.
3. **New-channel discovery:** when a channel appears in the channel-config CRDT for which no local log exists (created while this device was offline), fire with `since: None`.

### 4.2 Retry latch

Per `(community_id, channel_id)`, in-memory:

- States: `Unsatisfied { next_retry_at, backoff }` → `Satisfied`.
- A request is **satisfied** when ≥1 holder completes a reply (zenoh get completion from at least one responder), including zero-event completions (D24). If the zenoh API surface makes per-responder completion awkward, the plan may approximate with "any backfill-tagged inbound event OR an explicit query-completion callback" — the design requirement is only: *no reply at all ≠ satisfied*.
- While unsatisfied: re-request at `30s, 60s, 120s, … cap 600s`, indefinitely while the engine runs.
- **Consciously accepted gap:** one holder may itself hold partial history, so a satisfied latch does not prove completeness. The engine-start watermark trigger re-runs on every restart, healing gaps over time. Per-event completeness proofs (Merkle/range digests) are deferred (§7).

### 4.3 Verification parity

Backfilled events face exactly the gates live events face (D25): author-auth against membership (ZEB-399), replay-tracker dedupe, membership-at-HLC `snapshot_at` gating. Plan step verifies the membership-at-HLC check exists on the shared path and adds it there if not.

**Step zero (plan gate):** verify the ChannelKey does **not** rotate on membership change — a joiner must be able to decrypt pre-join packets. If rotation exists, D20's full-history promise additionally requires key-history delivery; that is a scope change requiring a design revisit, not something to improvise in the plan.

## 5. Data flow

```
JOIN:      redeem → membership materialized → for each channel:
           schedule_backfill(ch, None) ──zenoh get──► any online holder device
           ◄── replies (wire-identical to live) ── streamed
           → process_inbound_packet → author-auth + membership@HLC + dedupe
           → ChannelLog append → UI event (messages appear; no new UI)

RECONNECT: engine start → per channel: schedule_backfill(ch, Some(local max HLC))
           → same path; replay tracker drops already-held events

NO HOLDER: zero replies → latch Unsatisfied → backoff re-request → converges
           when any holder (including the author's own siblings) comes online
```

## 6. Failure modes

| Failure | Behavior |
|---|---|
| No holder online | Latch unsatisfied; backoff re-request (30 s → 10 min cap) while the engine runs; converges when any holder appears (D21). |
| Holder serves partial history | Satisfied this round; the engine-start watermark trigger re-requests on every restart, healing gaps over time (D24 accepted gap). |
| Backfilled event fails author-auth or membership-at-HLC | Rejected by the shared verification path; warn + telemetry counter. Same as a bad live event — never a backfill-specific bypass (D25). |
| Duplicates (overlapping holders, re-requests) | Replay tracker dedupes — already proven on the live path; replies are wire-identical (D26). |
| Joiner cannot decrypt pre-join packets | Impossible if the step-zero key-rotation gate passes; if observed anyway, surfaces as decrypt-failure telemetry, not silent loss. |
| Reply storm on large histories | Bounded by existing per-packet size limits; replies stream and append incrementally — no full-history buffering. |

## 7. Non-goals

- **Group-DM butler** (fan-out deposits, co-community admission proof) → P3b, own spec/cycle.
- **Sealed relay / fully disjoint fleets** → P4, unchanged.
- **Attachments / CAS blobs in channels** — `SignedChannelEvent::Post.body` is a `String` today; nothing to backfill beyond text.
- **Bounded or per-community-configurable history depth** — later upgrade on the same plumbing (D20).
- **Per-event completeness proofs / true anti-entropy** — watermark re-sync approximates it at alpha scale (D24).
- **UI work** — backfilled messages flow through the existing channel-log → UI event path.
- **Persisted latch state** — restart re-requests; idempotent (D24).

## 8. Testing

- **Unit:** latch state machine (unsatisfied→satisfied; empty-but-complete reply satisfies; backoff schedule; cap behavior); watermark computation from the local log.
- **Two-engine integration:**
  1. *Pre-join (ZEB-403):* A posts → B joins later → B receives the pre-join message — reproduces the exact ZEB-366 observation.
  2. *Reconnect catch-up:* B offline during N posts → reconnects → receives exactly the missed N, no duplicates.
  3. *Eventual convergence:* B requests while A offline → retries → A comes online → B converges.
  4. *Verification:* a backfilled event from a non-member (or post-membership-change) author is rejected.
- **No new wire fixtures** (D26).

## 9. Ticket disposition

- This work **implements ZEB-403**; the P3a PR closes it (plain-text ticket references only in the PR body — no closing keywords; the title/branch cascade is expected for ZEB-418 and gets the usual post-merge reopen since P3b/P4 remain).
- P3b (group-DM butler) gets its own ticket, filed after this spec commits.
- ZEB-423 (dm-inbox-v1 payload-cap parity) remains an independent backlog item.
