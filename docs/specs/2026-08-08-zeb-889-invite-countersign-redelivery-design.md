# ZEB-889 — Joiner-side mint reuse on retry (zombie-invite fix) — Design

**Ticket:** ZEB-889 (Low). Predecessors: ZEB-874 (#619, defer-burn), ZEB-875 (#621, claimant-bound claim), ZEB-888 (#643, single-use materialization invariant).

**Status:** design approved (Jake, 2026-08-08). Approach revised during implementation prep — see "Why not host-side re-delivery".

## Problem

ZEB-874 defers the single-use invite burn to *after* the countersign is handed to the transport, specifically so a legitimate joiner whose countersign **delivery** failed can retry against the still-live invite. But the retry cannot succeed, and the invite becomes a permanent, unredeemable zombie.

Failure trace (untargeted or targeted single-use invite; joiner B, iroh handshake path):

1. B redeems over iroh via `connectivity_redeem_invite_iroh_inner`. It mints a redemption up-front (`mint_redemption`, which **randomizes** the 16-byte event id), producing PendingJoin `P1`, and dials the host. The host inserts `P1`, the auto-counter-sign hook commits `CS1` (targeting `P1.id`), and B materializes `Joined` **host-side**.
2. The countersign **delivery** to B fails (write / io-timeout / dropped connection; or the acceptor `CountersignTimeout`s before delivery). Per ZEB-874 the burn fires only *after* delivery, so the invite stays live. B's dialer surfaces `inviter_unreachable`; B persists **nothing** locally.
3. B retries. `connectivity_redeem_invite_iroh_inner` mints **fresh** → `P2` with a new random id. The host inserts `P2` → `verify_event` **P6** (`community_membership.rs:4394-4398`) rejects it (B is already `Joined`) → `EngineRejected` → the acceptor writes no response and does not burn.
4. Every retry repeats step 3 identically. The pkarr invite handle (community invites carry no `expires_at`) is never unregistered: the original actor is P6-blocked, every *other* actor is ZEB-875-claim-blocked. Nobody can ever redeem it.

## Root cause

The retry is **not a retransmit** — it mints a brand-new PendingJoin id each time. The host already re-delivers a countersign for a *retransmit*: `iroh_invite_acceptor.rs:534` — *"Fires on both Inserted and AlreadyKnown (a retransmit re-delivers the countersign)."* An `AlreadyKnown` insert (same `bootstrap_join.id`) returns `Ok(())` (`community_invite.rs:2676`) and the acceptor's poll then finds `CS1` (which targets that id) and delivers it. The joiner never exercises this path because it never re-sends the same id.

The joiner also **requires** the countersign to target its own current mint, in two places, so a countersign for any *other* id is rejected:

- the dial reader: a response that is not "a JoinCountersign for our bootstrap_join.id" → `inviter_unreachable`;
- `redeem_invite_inner_with_overrides`: `pre_delivered_countersign` with `target_event_id != minted.bootstrap_join.id` → hard error (`lib.rs:40985-41002`).

## Approach — reuse the minted redemption on retry (joiner-side, in-memory)

Cache the `MintedCommunity` produced for an invite, keyed by the **invite-token signature** (`InviteToken.sig`, `[u8; 64]`), in a process-lifetime map on the community registry (co-located with its existing per-redemption tracking). At the mint site in `connectivity_redeem_invite_iroh_inner`, **reuse** the cached mint if present instead of minting fresh; only mint (and store) on the first attempt for that token. On a `joined` outcome, evict the entry.

Then a retry re-sends the **same** `P1` (same id/bytes). The host's existing `AlreadyKnown`-retransmit path re-delivers `CS1` (which targets `P1.id`), both joiner guards pass (the countersign targets the reused mint), B inserts `P1`+`CS1` → `Joined`, and the acceptor's idempotent burn unregisters the invite. **No host-side or wire-format change.**

Why caching the whole `MintedCommunity` is correct: `mint_redemption` is a pure function of `(payload, self_owner, signing_key, enrollment_cert, join_hlc)`; its only non-deterministic outputs are the random event id and the tracker-reserved `join_hlc`. Reusing the cached value reproduces `P1` byte-for-byte — including its `at` (HLC) — which is exactly what a retransmit needs. The cache key `InviteToken.sig` is unique per invite, so a cached mint is only ever reused for the *same* invite payload.

### Why not host-side re-delivery (the originally-approved direction)

Re-delivering the host's existing `CS1` cannot converge the joiner: `CS1` targets `P1`, but a retry minted `P2`, so both joiner guards above reject it — and B lacks `P1`'s bytes, so `CS1` alone can't materialize B even if accepted. Making it work would require the host to re-deliver **both** `P1` and `CS1` (a wire-format change from one event to two) *and* relaxing both joiner guards to adopt that pair over B's fresh mint — more invasive and two-sided. Reusing the mint keeps the fix on one side and changes no protocol.

### Why in-memory (not disk-persisted)

Proportionate for a Low-priority fix: it closes the zombie for **in-process** retries — the immediate-retry / "try via local network" case ZEB-874's defer-burn exists to enable. A restart-then-retry falls back to today's behavior (LAN / CRDT state-sync recovery is still available, per the ticket's own caveat). Disk persistence would add a durable store + serialization + lifecycle for marginal additional coverage; deliberately out of scope.

## Fix shape

### 1. `MintedCommunity`: derive `Clone` — `lib.rs`

Add `#[derive(Clone)]` (all fields — `SpaceId` (Copy), `EpochKey`, `Space`, `SignedMembershipEvent` — are `Clone`). Needed so the cache can hand back an owned copy.

### 2. In-flight-redemption mint cache — the community registry

A `tokio::sync::Mutex<HashMap<[u8; 64], MintedCommunity>>` field on the registry that already owns `register_pending_redemption` / `take_pending_redemption` (same lock-discipline as `root_fetch_shutdowns`), initialized empty in `CommunitySyncRegistry::new`, with three async methods next to the pending-redemption helpers (~`community_state_sync.rs:5850`):

- `get_redemption_mint(&self, token_sig: [u8; 64]) -> Option<MintedCommunity>` — clones out the cached mint under the lock if present.
- `store_redemption_mint(&self, token_sig: [u8; 64], mint: MintedCommunity)` — inserts.
- `evict_redemption_mint(&self, token_sig: &[u8; 64])` — removes.

Split into get/store (rather than one `reuse_or_store(FnOnce)`) because the mint site reserves its HLC via an **async** call — a sync closure can't await it. The get-then-store window admits a benign race: two *concurrent* redeems of one token could both miss and both mint (last store wins). That is the sequential-retry scenario's non-case, and even concurrently it creates no worse state than two concurrent redeems do today (each mint is a valid claim for the same actor; the host dedups by id). Keying on the registry (not a new `connectivity_redeem_invite_iroh_inner` parameter) avoids widening that already-large signature and co-locates the state with the existing redemption bookkeeping.

### 3. Reuse at the mint site — `connectivity_redeem_invite_iroh_inner` (`lib.rs`)

At step "8'. Reserve HLC + mint" (`lib.rs:62816-62844`), gate the reserve-HLC + `mint_redemption(...)` block on a cache check: `if let Some(m) = community_registry.get_redemption_mint(token.sig).await { m } else { /* reserve join_hlc + mint_redemption(...) */ store_redemption_mint(token.sig, m.clone()).await; m }`. On a cache hit no fresh HLC is reserved and no new id is generated — the reused `P1` is retransmitted verbatim.

### 4. Evict on success — `connectivity_redeem_invite_iroh_inner`

`community_registry` is *moved* into `redeem_invite_inner_with_overrides` at `lib.rs:63153-63171`, so capture `let registry_evict = Arc::clone(&community_registry);` and `let token_sig = token.sig;` before that call. In the `Ok(dto)` arm, when `dto.status == "joined"` (the invite is now burned; no further retry needed), `registry_evict.evict_redemption_mint(&token_sig).await`. A never-completing redemption's entry lingers for the process lifetime — bounded (one `MintedCommunity` per distinct invite redeemed this session) and acceptable.

## Testing

1. **Unit — registry cache:** `store_redemption_mint` then `get_redemption_mint` returns the stored mint; `get` on an unstored token sig is `None`; distinct token sigs are independent; `evict_redemption_mint` drops the entry so a subsequent `get` is `None`.
2. **Unit — reuse reproduces the id:** storing a mint and reading it back yields a `MintedCommunity` with identical `bootstrap_join.id` and `bootstrap_join.at` (and a fresh `mint_redemption` for the same inputs produces a *different* id — pinning that the cache, not determinism, is what makes a retry a retransmit).
3. **Integration — the zombie is redeemable on retry:** model on `invite_not_burned_when_handshake_fails_after_insert`. Seed the host with B's committed `P1` + host `CS1` and the still-live invite handle (the "first attempt succeeded host-side but delivery failed" state), and pre-store the mint that produced `P1` in B's registry cache. Drive one `connectivity_redeem_invite_iroh_inner` for the same invite URL: assert B reuses the cached mint (sends `P1`, not a fresh id), the host's `AlreadyKnown` path re-delivers `CS1`, B's outcome is `joined`, and the invite handle is now unregistered. Contrast with the existing negative test (delivery-failure leaves it live) — this proves the *recovery* leg ZEB-874 intended.
4. **Regression — different actor still refused:** a redeem from a distinct actor A2 on the already-claimed token still returns an error / non-joined and writes no countersign (ZEB-875), confirming the cache — which is per-`(process, token)` and only ever hands B back B's own mint — does not widen the single-use claim.

## Safety (ZEB-888 single-use invariant preserved)

The cache only ever lets **actor B retransmit B's own PendingJoin** for an invite B already redeemed. It creates no new claim and no new countersign; the host's canonical-claimant set (ZEB-888) is untouched. A different actor's redeem still mints its own distinct PendingJoin and is refused by the ZEB-875 claim exactly as today. No behavior changes on the host or the wire.

## Non-goals

- No change to `verify_event`, P6, or any CRDT materialization rule; no host-side or wire-format change.
- No disk persistence (restart-then-retry keeps today's behavior).
- Does not address ZEB-876 (reversing a committed countersign) — orthogonal and deferred.
