# ZEB-434: Community-state reconnect catch-up — design

- **Date:** 2026-06-10
- **Ticket:** ZEB-434 (channels created while a member is offline stay invisible until the creator next reboots)
- **Status:** approved (Jake, 2026-06-10 design session)
- **Base:** `origin/main` `cf066c0e`
- **Related specs:** `docs/specs/2026-06-10-zeb-418-sp2-p3a-channel-backfill-design.md` (the BackfillLatch pattern this design mirrors; its §9 transport-recovery follow-up is implemented here)

## 1. Problem

Live repro (2026-06-10, Koya ↔ Ildwyn, community "Zeblithic 260610 Test2"): a channel created while one member's process was down never appeared for that member — not on their reboot, not after 20+ minutes of healthy live sync — until the *creator's* process restarted and (incidentally) republished. Channel-log backfill (P3a) healed message history for *known* channels in 5.1 s; the channel *set* had no equivalent.

### Root cause (verified in code)

Community-state CRDT propagation is **push-only with no memory**:

1. Mutations set a dirty flag; a 200 ms debounce fires `publish_root_now`, which publishes the full encrypted state root on zenoh `harmony/community/{id_hex}/state-root-v1` (`community_state_sync.rs:2230-2293`, adapter `event_loop.rs:6567`).
2. A zenoh `put` with zero subscribers delivers to nobody and still **clears the dirty bit** — publish-into-the-void counts as published.
3. There is **no queryable** on community state (nothing for a reconnecting member to pull from), **no boot republish** (`spawn_engine_inner_now` loads from disk without marking dirty, `community_state_sync.rs:4190`; the shutdown final-flush is dirty-gated, `community_state_sync.rs:2376-2386`), and **no anti-entropy timer**.
4. Net effect: state published while a member is offline is unreachable to them until the **next mutation anywhere in that community** triggers a fresh full-root publish. In a quiet community, that is indefinite.

The mint engine already solves its analogue with a boot-hook flush ("emit the local snapshot shortly after startup", `lib.rs:3533-3552`); the community boot spawn loop (`lib.rs:4678`) has no such hook.

Severity is broader than channels: **kicks, power changes, joins, and config changes go stale the same way** — the channel set is just where it was observed.

The same boot-races-link-establishment weakness exists in mail sync: `query_mail_root("startup")` fires exactly once at boot (`event_loop.rs:2599`) and logs "no responder — live push will catch up on next gateway publish" with no retry. Confirmed the only other one-shot startup query in the codebase.

## 2. Goals

- A member who was offline during a community-state publish heals **from their own side**, deterministically, shortly after their engine spawns.
- A creator whose mutations were published into the void (or never published) re-seeds peers after their own boot.
- Peers that stayed up across an in-session link partition heal when the link recovers.
- The mail-root startup query stops permanently missing on the boot race.
- No new wire formats; full verification parity with the live push path.

## 3. Non-goals (out of scope)

- Multi-hop topology-change detection (only direct peer links are observed; see D6 limitation).
- State-root paging / partial-state sync. Full-root size growth with event count is a pre-existing property of the full-state CRDT, unchanged by this design.
- Vote-log catch-up audit (separate ticket if cross-WAN testing shows a gap).
- Frontend changes — `channel-config-updated` and the membership delta path already deliver everything once the backend merges events.

## 4. Design decisions

**D1 — Pull plane on the state-root keyexpr.** Each community engine gets a zenoh **queryable** on `harmony/community/{id_hex}/state-root-v1` (the query plane of the same keyexpr the pub/sub adapter uses; no parameters — root exchange is full-state, there is no `since`). Declared by `event_loop` next to the channel-log backfill queryable (`event_loop.rs:7075`), with the responder closure handed over via an extension of `CommunityAdapterRequest` (`event_loop.rs` / `lib.rs:4704`), mirroring P3a's `read_for_query` hand-off (`community_channel_log_engine.rs:1710`).

**D2 — Replies are fresh-encoded through the engine's single-writer task.** The queryable handler does not encode; it sends a oneshot request into a new `select!` arm in `internal_task`, which runs the same encode path as `publish_root_now` (canonical CBOR → deterministic-nonce blob encrypt for ContentId dedup → wire encrypt → `next_hlc` advance → persist-on-success) and replies with the bytes. The HLC doubles as replay protection, which makes packet production a serialization point — routing through the single-writer task means publish, flush, and query-serve can never disagree about clock state. *Rejected alternative:* caching the last-published bytes — goes stale across failed publishes and replays one HLC to multiple queriers.

**D3 — `RootFetchLatch`.** A sibling of `BackfillLatch` in `channel_backfill.rs` sharing the backoff constants (base 30 s, cap 600 s, doubling per consecutive no-reply, retry forever at cap; driver exits on engine shutdown). No `since`/paging fields. **Satisfied = ≥1 reply received** (transport-level — parity with P3a D24's "an unanswered query is NOT satisfied"); zero replies = no responder = backoff retry.

**D4 — Replies ingest via the existing inbound path.** The per-community fetch driver issues the zenoh `get` (`ConsolidationMode::None`, 10 s timeout — same as the mail-root query, mirroring the P3a query-request driver at `event_loop.rs:7145`) and forwards every reply payload into the engine's existing `subscriber_tx` → `handle_incoming_publish`: decrypt, `RootHlcTracker` replay guard, publisher-membership verification, event-ID dedup (`community_state_crdt.rs:299`), delta emission. No new wire format; verification parity inherited, not rebuilt (P3a D25 analogue). Multiple responders produce multiple idempotent merges.

**D5 — Boot flush for community engines.** In the boot-time spawn loop (`lib.rs:4678`), after the adapter request is enqueued: spawn a delayed task calling `registry.flush_now(&space_id)` (`community_state_sync.rs:4415`), mirroring the mint boot-hook (new `COMMUNITY_BOOT_FLUSH_DELAY_MS` constant with the same value as `mint_sync::DEFAULT_BOOT_FLUSH_DELAY_MS`; errors logged + ignored). The flush is **unconditional** — the dirty bit does not survive restarts and clears on publish-into-the-void, so dirty-gating would defeat the purpose. Receivers dedup (`AlreadyKnown` → tracker-only persist), so the cost is one root publish per community per boot. Join/create paths already flush (`lib.rs:18454`); boot was the only gap.

**D6 — Transport-epoch watch channel.** The event loop's existing 5 s peer refresh (`event_loop.rs:2940-2951`) currently overwrites `direct_peer_zids`; it now diffs. Any **new zid** (zenoh zids are per-session, so a rebooted peer always appears new) bumps a `tokio::sync::watch::Sender<u64>` "transport epoch". Subscribers: community root-fetch drivers, channel-log `BackfillLatch` drivers, the mail-root driver. *Limitation (documented):* only direct peer links are observed; a topology change two hops away does not trigger. The rebooted-peer case always produces either a new direct zid or a fresh boot-flush publish, so the dominant paths are covered.

**D7 — Re-arm semantics.** On a watch bump, a driver resets its backoff to base and issues a query immediately — unless it sent a query within the last **60 s** (per-driver cooldown, so a flapping link cannot storm). This applies both to satisfied latches (which become unsatisfied until the re-query completes) and to latches mid-backoff (a new peer is exactly the signal that retrying now is worthwhile — don't wait out a 600 s delay).

**D8 — P3a reset wiring.** Add `BackfillLatch::reset()` (the hook P3a's spec §9 defined as follow-up but never wired) and subscribe the channel-log backfill drivers to the transport-epoch watch with D7 semantics. This closes P3a's deferred transport-recovery item for channel logs in the same pass.

**D9 — Mail-root retry driver.** Replace the one-shot spawn at `event_loop.rs:2599` with a latch-driven driver using the same backoff and watch subscription. Asymmetry vs. community roots, made explicit: an **empty-payload reply is a valid answer** ("no mail yet" sentinel) and satisfies the latch; `Ok(None)` (zero responders) does not and retries. `MailSync::refresh_now` (manual) is unchanged.

**D10 — Error handling.**
- *No responders:* retry forever at 600 s cap; drivers exit on engine/node shutdown.
- *Garbage or unverifiable replies:* dropped by the existing inbound path with the existing `community-state-sync-degraded` reporting (`community_state_sync.rs:2337-2354`). The latch may satisfy on transport receipt of a bad reply; the re-arm hook and next-boot retry are the backstop. This is the same trust model as live push — a malicious peer could equally publish garbage on the pub/sub plane.
- *Confidentiality:* replies are the same membership-key-encrypted ciphertext already broadcast on the pub/sub plane; non-members can't decrypt either. Serving via query adds no new exposure.
- *HLC/persist discipline:* encode-for-query advances `next_hlc` and persists inside the single-writer arm, preserving the publish path's "never advance the tracker unpersisted" rule (`community_state_sync.rs:2276-2293`).
- *Boot-flush failure:* logged, ignored (mirrors mint; the pull plane is the recovery for a failed push).

**D11 — Testing.**
- *Unit:* `RootFetchLatch` state machine — backoff doubling and cap, satisfy-on-reply, re-arm resets backoff, 60 s cooldown suppression (pure functions, mirrors existing `channel_backfill` tests; wall-clock-free per the logical-time testing rule). Mail-root empty-payload-vs-no-responder discrimination.
- *Engine-level:* two-engine test — A holds `ChannelCreate` events, drive A's query-serve arm, feed the reply bytes to B's inbound, assert B's materialized channel set (mirrors existing two-engine community tests in `community_state_sync.rs`).
- *Integration:* the repro shape — channel created while B's engine is down, B spawns, pull heals the channel set; boot flush publishes on spawn and a live peer ingests; a watch bump re-arms a satisfied latch and respects cooldown.
- *Pin:* query replies parse as standard root publishes — format identity with the live push packet, asserting "no new wire format" stays true.

**D12 — Scale posture.** Pull is on-demand and bounded (per spawn + per re-arm with cooldown); no periodic broadcast is introduced. This is the scalable primitive; the boot flush is one publish per community per boot.

## 5. Components

| Component | Where | Change |
|---|---|---|
| Query-serve arm | `community_state_sync.rs` `internal_task` | New `select!` arm; oneshot request → fresh-encode → reply bytes |
| `RootFetchLatch` + driver | `channel_backfill.rs` (+ driver wiring) | New latch struct; per-community fetch driver |
| State-root queryable | `event_loop.rs` (next to `:7075`) | Declare queryable; route to engine oneshot |
| `CommunityAdapterRequest` | `event_loop.rs` / `lib.rs:4704` | Carry the query-serve channel |
| Boot flush | `lib.rs:4678` spawn loop | Delayed `registry.flush_now` per community, mint pattern |
| Transport-epoch watch | `event_loop.rs:2940` | Zid-set diff → `watch<u64>` bump |
| `BackfillLatch::reset()` | `channel_backfill.rs` + driver | P3a §9 hook, wired to watch |
| Mail-root driver | `event_loop.rs:2599` region | One-shot → latch-driven retry + watch |

## 6. Data flow — the repro, replayed

Member boots → community engines spawn → root-fetch driver queries `state-root-v1` → any online member's queryable fresh-encodes and replies → reply flows through `handle_incoming_publish` (decrypt, replay guard, membership check, dedup) → `ChannelCreate` delta → `channel-config-updated` → channel-log engine spawns → P3a backfill pulls message history. Channel + full history visible within seconds of boot, healed entirely from the member's side.

Creator-offline direction: creator boots → boot flush publishes the root unconditionally; independently, members observe the creator's new zid → re-arm → pull. In-session partition: link recovery surfaces new zids on both sides → both re-arm → push and pull converge.

## 7. Follow-ups (not in this PR)

- Multi-hop topology-change detection, if cross-WAN testing shows the direct-link signal is insufficient.
- Vote-log catch-up audit under the same lens.
- State-root paging if full-root publishes become large enough to matter (pre-existing concern).
