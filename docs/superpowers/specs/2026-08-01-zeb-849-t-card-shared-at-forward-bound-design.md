# ZEB-849 T-CARD — forward-bound `shared_at` on profile cards (design)

**Ticket:** ZEB-849 (ZEB-831 wall-clock threat model, finding **C4 CRITICAL** + sibling **C10**).
**Series:** third fix after ZEB-846 T-GOV (#581) and ZEB-847 T-OWNER (#582). Same
`clock_trust` reject-at-boundary machinery; same "reject, never clamp, for a
replicated newer-wins register" rule (ZEB-847 converge lesson).

## Threat

A `ProfileCardBroadcast` carries `shared_at: Hlc`, minted and **signed by the
card owner's own device**. `verify_card` (`profile_card_broadcast.rs:210`) bounds
display-name/status length, the enrollment cert, and the Ed25519 signature — but
**never** `shared_at.wall_ms`. Both the in-memory cache (`insert_verified`) and
the disk store (`PersistentCardStore::upsert`) resolve by
`Hlc::is_strictly_newer_than`, whose primary key is `wall_ms`.

So a card with `shared_at.wall_ms = <far future>` wins forever and pins the
owner's chosen `display_name` / `avatar_cid` / `profile_page_root` on **every
peer**. The store is disk-persisted with **deliberately no TTL** and is **not
re-verified on load**, so the poison survives restarts — "recovery only by
deleting the cache file." Cross-user poisoning is already blocked
(`owner_id == expected_owner`), so the attacker is the card's **own** skewed or
compromised device; the victims are all its peers. Classification:
**POISON-SQUAT / identity spoofing**, FAIL-OPEN.

Key asymmetry that shapes the fix: because the store is newer-HLC-wins, a
future-dated poison can **never be out-stamped** by an honest card at
`wall_ms ≈ now`. An ingest bound stops *new* poison but cannot dislodge poison
already resident on disk. Those need different treatment (L1 vs L2 below).

## Global constraints

- One house constant: `clock_trust::MAX_FORWARD_SKEW_MS` (5 min), control tier.
  Never introduce a new constant.
- **Reject, never clamp** (ZEB-847). These are replicated newer-wins registers;
  clamping writes a receiver-dependent value and still fails open on a static
  compare.
- **A bad LOCAL clock must never drop honest state** (fail-open). Every bound is
  measured against the *receiver's own* clock and disables itself when that
  clock is unreadable — never substitutes `0` (which would reject every real
  card). In this subsystem the unreadable-clock sentinel is already `now_secs == 0`
  (production passes `iroh_friend_acceptor::wall_now_secs()` =
  `wall_now_ms()/1000` with `.unwrap_or(0)`).
- **Never destroy at-rest cards via a load-time write-back** (view-not-store
  rule; ZEB-831 slow-clock-purge failure mode). L2 suppresses from the read view
  and leaves disk untouched.
- Units: `shared_at.wall_ms` is milliseconds; `now_secs` is seconds. Convert
  `now_secs * 1000` before comparing. Sub-second truncation only tightens the
  bound by <1 s inside a 300 000 ms window — never rejects an honest present card.

## Fix

### L1 — ingest bound (core)

In `verify_card(card, now_secs)`, after the length checks and before/with the
cert+signature work, reject when the shared-at stamp is implausibly future:

```rust
// now_secs == 0 ⇒ unreadable local clock (wall_now_secs().unwrap_or(0)) ⇒ apply-all.
if now_secs != 0
    && crate::clock_trust::reject_future(
        card.shared_at.wall_ms,
        now_secs.saturating_mul(1000),
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    )
{
    return Err(CardVerifyError::SharedAtTooFarInFuture);
}
```

New error variant `CardVerifyError::SharedAtTooFarInFuture`.

This is the **single write chokepoint**: the only production `store.upsert`
caller and the only live-cache `insert_verified` are both downstream of this
`verify_card` (`event_loop.rs:3034`). A rejected card reaches **neither** cache.

### L2 — at-rest read gate (non-destructive)

For poison persisted before this patch (which honest cards can never out-HLC),
gate the store's **read surface**, not its storage:

- `get(&owner)` → delegates to `get_with_now(&owner, clock_trust::receiver_now_ms())`.
- `display_names_by_owner()` → delegates to
  `display_names_by_owner_with_now(clock_trust::receiver_now_ms())`.
- The `_with_now(now_ms: Option<u64>)` inner seams suppress any entry with
  `clock_trust::wall_exceeds_forward_skew(entry.shared_at.wall_ms, now_ms)` from
  the returned value. `None` ⇒ apply-all (unreadable clock shows everything).

**Non-destructive:** the in-memory map and the on-disk file are untouched. The
poison entry stays resident (inert, suppressed from every read); it ages out
naturally under the existing LRU soft cap. This respects the store's deliberate
no-TTL and freeze-on-unreadable invariants and the view-not-store rule.

Effect at the resolution point (`ProfileCardCache::get_cached`): with the store's
poison suppressed, the `(live, stored)` match sees `stored = None` and returns
the honest live card.

### L3 — C10 sibling (in-memory membership broadcast)

`ProfileMembershipBroadcastCache::on_sample` (`profile_broadcast.rs:581`) applies
the same unbounded newer-wins on `broadcast.shared_at`. Thread a `now_secs`
parameter (prod caller `event_loop.rs:2861` passes `wall_now_secs()`), and before
the newer-wins apply reject a future-dated stamp the same way (`0` ⇒ apply-all).
Lower severity (in-memory, session-scoped, no disk), fixed here so no known
sibling is left behind. Reuse the existing `Replay` rejection path or add a
dedicated `FutureSkew` outcome — implementer's call, pinned by a test either way.

## Tests (discrimination pairs — series discipline)

Every layer ships both halves: poison is stopped **and** an honest in-range
newer card still wins (proving the bound does not over-reject), plus a fail-open
pin.

- **L1**: (a) future-dated card (`wall_ms = now + 1 yr`, real `now_secs`) →
  `Err(SharedAtTooFarInFuture)`; (b) in-range newer card verifies and an older
  in-range card verifies too (both accepted); (c) `now_secs == 0` ⇒ the same
  future-dated card **verifies** (fail-open pinned).
- **L2**: (a) store seeded with a poison entry → `get`/`display_names_by_owner`
  with a real `now` omit it; (b) an in-range entry is always returned; (c)
  `now = None` ⇒ poison shown (apply-all).
- **L3**: (a) `on_sample` with future `shared_at` + real `now` → rejected; (b)
  in-range newer still replaces; (c) `now_secs == 0` ⇒ apply-all.

## Out of scope (explicit)

- Full signature re-verify on load (layering; revocation is a separate concern
  per the store's existing module doc).
- A TTL (the store deliberately has none).
- Destructive on-load pruning (slow-clock purge risk).
- Tracing/metrics on the new reject sites — folds under the already-filed
  **ZEB-855** (uniform observability across all `clock_trust` reject boundaries).
