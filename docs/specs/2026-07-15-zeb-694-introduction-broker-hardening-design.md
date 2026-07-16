# ZEB-694: Introduction-Broker Hardening — Design

**Ticket:** ZEB-694 (child of ZEB-376 / follow-up to PR #474)
**Branch:** `zeb-694-intro-broker-hardening` (off `main` `be3444ca`)
**Status:** design approved 2026-07-15; one bundled harmony-client PR.

**Goal:** Close the two deferred hardening items from the ZEB-376 (Friends Phase 2b introduction broker) bot review — restructure the introduction rate limiter so quotas key on authenticated identities and don't collide across protocol roles, and make an AskMe introduction accept survive a failed dial instead of burning the staged offer.

**Architecture:** Two node-local, independent changes. Part 1 splits the single `IntroRateLimiter` across the authentication boundary it currently straddles: a pre-auth flood shield keyed on the connection's iroh-authenticated endpoint identity, plus post-auth per-role quotas keyed on the verified owner. Part 2 reorders the accept path from consume-then-dial to peek → dial → consume-only-on-`Linked`, with an in-flight guard and a TTL on staged offers. Neither part changes any wire format.

**Tech stack:** Rust (tokio, iroh QUIC transport), Svelte 5 frontend, Tauri IPC.

## Global Constraints

- **No wire-format change.** The connection shield reads transport identity (`conn.remote_id()`), not a new frame field; the offer store, in-flight guard, and TTL are all process-local. The `zeb375_pex_fixtures` and `zeb376_intro_fixtures` byte-pinned fixtures MUST remain byte-identical. Any diff to those files is a regression, not a re-pin.
- **Benign-ack on shed, no oracle.** Every rate-limiter rejection (any tier) funnels to the existing `self.write_ack(&mut send).await` benign-ack path — the sender cannot distinguish "shed" from "accepted." No new error variant reaches the wire.
- **Fail-safe ordering preserved.** Pre-auth admission still runs strictly before `authenticate_introduce_request` / `verify_introduction` (cheap load-shedding). Post-auth quotas run strictly after those succeed.
- **Memory stays bounded.** Every keyed map retains the ZEB-376 Task 13 8192-cap two-pass eviction (stale-prune then oldest-evict to a 3/4 low-watermark). No unbounded growth from spoofed or rotated keys.
- **Durability HARD-RULE unchanged.** Part 2 does not alter the `Linked`-gated durability in `complete_introduction` (`notify_dirty` + Case-D reconcile + `friend-list-changed` emit stay exactly where they are). We only change *when the offer is consumed* relative to that result.
- **Gates:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --features test-fixtures ...`; `tsc`; `vitest`.

## Non-goals

- No change to the introduction wire protocol, the policy truth table (Open/FriendsOfFriends/AskMe/Closed), or the `expected_peer` binding that authenticates the resulting link.
- No change to the outbound pre-authorization store (`PendingOutboundIntroductions`) — that is a separate, already-hardened one-shot store.
- ZEB-693 **Gap 1** (the `peer_intro_policy` / `friend_auto_accept` IPC wrapper seams) is explicitly out of scope and stays on ZEB-693. Only ZEB-693 **Gap 2** (a testable seam into the introduction-accept branch) is folded in here, because Part 2's own verification requires it.

---

## Part 1 — Rate limiter: two-tier, role-separated

### Current state (the two defects)

`IntroRateLimiter` lives in `src-tauri/src/friend_intro.rs` (struct `:594`, `admit` `:702-745`). It exposes one method:

```rust
pub fn admit(&self, key: OwnerAddr, subject: OwnerAddr, now_ms: u64) -> Result<(), &'static str>
```

keyed on `OwnerAddr` values pulled straight from the (unauthenticated) decoded frame. The acceptor (`src-tauri/src/iroh_pex_acceptor.rs`) holds one shared `Arc<IntroRateLimiter>` (field `:140`, built `:192`) and calls it in both arms of `serve(&self, conn: &Connection)` (`:441`):

- **Broker/requester arm** (`:539-557`): `admit(ir.from_addr, ir.target, now)` — *before* `authenticate_introduce_request` (`:578`).
- **Target/voucher arm** (`:654-683`): `admit(intro.voucher, intro.subject, now)` — *before* `verify_introduction` (`:711`).

Two defects:

- **(a) Unauthenticated keying (CodeRabbit, Major).** Because `admit` runs pre-auth on frame-supplied `OwnerAddr`, a spoofer can (i) flood `(victimVoucher, *)` to fill a legitimate voucher's sliding window / dedupe map so that voucher's *real* introductions get shed, or (ii) rotate spoofed keys to evade the per-key quota.
- **(b) Role collision (Greptile, P1).** The same per-`OwnerAddr` window map is keyed by requester in the broker arm and by voucher in the target arm. Owner `O`'s request traffic (O asks this node to broker) and O's vouch traffic (O vouches for an introduction *to* this node) collide in one 20/hr budget: 20 requests silently shed an unrelated, legitimate vouch within the hour.

### New structure

Decompose `friend_intro.rs`'s limiter into two lock-free, independently-testable primitives plus a thin container that owns a single `Mutex`.

**Primitive `KeyedSlidingWindow<K>`** — the per-key `VecDeque<u64>` window half of today's `admit`, generalized over `K: Copy + Eq + Hash`, owning its own cap and window length:

```rust
struct KeyedSlidingWindow<K> {
    max: usize,
    window_ms: u64,
    windows: HashMap<K, VecDeque<u64>>,
}
impl<K: Copy + Eq + Hash> KeyedSlidingWindow<K> {
    fn admit(&mut self, key: K, now_ms: u64) -> bool; // prune → cap check → push → evict; true = admitted
    fn evict(&mut self, now_ms: u64);                                     // 8192-cap two-pass (from evict_windows)
}
```

**Primitive `KeyedDedupe<K>`** — the `last_seen` TTL half of today's `admit`, generalized:

```rust
struct KeyedDedupe<K> {
    ttl_ms: u64,
    last_seen: HashMap<K, u64>,
}
impl<K: Copy + Eq + Hash> KeyedDedupe<K> {
    fn is_duplicate(&self, key: K, now_ms: u64) -> bool;   // last_seen within ttl_ms
    fn record(&mut self, key: K, now_ms: u64);             // insert + 8192-cap two-pass (from evict_last_seen)
}
```

The two `evict` bodies are direct moves of the existing `evict_windows` / `evict_last_seen` (`friend_intro.rs:620-682`), generalized over the key type — same constants `MAX_WINDOW_KEYS` / `MAX_DEDUPE_ENTRIES = 8192` and same two-pass (stale-prune → `select_nth_unstable` oldest-evict to 3/4 low-watermark).

**Container `IntroRateLimiter`** holds one lock over an `Inner` with five sub-structures — one pre-auth window, and one window+dedupe pair per post-auth role:

```rust
pub struct IntroRateLimiter { inner: Mutex<Inner> }
struct Inner {
    conn:         KeyedSlidingWindow<[u8; 32]>,                  // Tier 1, pre-auth
    req_window:   KeyedSlidingWindow<OwnerAddr>,                 // Tier 2, requester role
    req_dedupe:   KeyedDedupe<(OwnerAddr, OwnerAddr)>,
    vouch_window: KeyedSlidingWindow<OwnerAddr>,                 // Tier 2, voucher role
    vouch_dedupe: KeyedDedupe<(OwnerAddr, OwnerAddr)>,
}
```

Public API (three methods, mapping 1:1 to the two tiers):

```rust
// Tier 1 — pre-auth flood shield. Un-spoofable key = the connection's iroh endpoint id.
pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str>;
// Tier 2 — post-auth quotas. Separate namespaces so the roles never share a budget.
pub fn admit_requester(&self, requester: OwnerAddr, target: OwnerAddr, now_ms: u64) -> Result<(), &'static str>;
pub fn admit_voucher(&self, voucher: OwnerAddr, subject: OwnerAddr, now_ms: u64) -> Result<(), &'static str>;
```

Each post-auth method preserves today's exact three-step sequence so `last_seen` is only recorded on a fully-admitted call: `is_duplicate(pair)?` → `window.admit(owner)?` → `dedupe.record(pair)`. `admit_connection` is window-only (no dedupe — a flood shield doesn't dedupe subjects). The old `admit` method is removed.

### Constants (`friend_intro.rs`)

- Reuse `INTRO_PER_VOUCHER_WINDOW_MS = 3_600_000` (1h) and `INTRO_DEDUPE_TTL_MS = 300_000` (5min) for both post-auth roles' windows/dedupe.
- Reuse `INTRO_PER_VOUCHER_MAX = 20` as the per-owner-per-role cap (rename-neutral: keep the constant, it now means "per authenticated owner per role per hour").
- Add `INTRO_PER_CONNECTION_MAX: usize = 40` — the Tier-1 per-endpoint cap over the same 1h window. Generous vs 20/role because one iroh endpoint may legitimately host or relay for several owners; a genuine flood from one endpoint is still shed.

`IntroRateLimiter::new()` wires each sub-structure with its cap/window/ttl. For test ergonomics add `IntroRateLimiter::with_caps(conn_max, per_owner_max, window_ms, dedupe_ttl_ms)` so tests can drive tiny caps deterministically; `new()` delegates to it with the production constants.

### Call-site changes (`iroh_pex_acceptor.rs`, inside `serve`)

`conn: &Connection` is already in scope at both arms; the authenticated remote endpoint key is `conn.remote_id()` (this codebase's iroh spelling — cf. `zenoh_iroh_transport.rs:74`, `tunnel_task.rs:123`, and every sibling acceptor), 32 bytes via `*conn.remote_id().as_bytes()`. It is *not* referenced today.

**Broker/requester arm** (`:539-557`):
1. Replace the pre-auth `admit(ir.from_addr, ir.target, now)` with `admit_connection(*conn.remote_id().as_bytes(), now)` at the same point (still before `authenticate_introduce_request`). On `Err` → warn + `write_ack` (benign ack), unchanged.
2. *After* `authenticate_introduce_request(&ir, self.self_owner, now_secs)` succeeds (`:578`), add `admit_requester(ir.from_addr, ir.target, now)`. On `Err` → warn + `write_ack` (benign ack). Now keyed on the authenticated requester.

**Target/voucher arm** (`:654-683`):
1. Replace the pre-auth `admit(intro.voucher, intro.subject, now)` with `admit_connection(*conn.remote_id().as_bytes(), now)` (still before `verify_introduction`). In this arm the connecting endpoint is the *deliverer* (F dialing X) — a correct, un-spoofable per-endpoint flood key.
2. *After* `verify_introduction(...)` succeeds (`:711`), add `admit_voucher(intro.voucher, intro.subject, now)`. On `Err` → warn + `write_ack` (benign ack). Keyed on the verified voucher.

Use one `wall_now_ms()` per arm (compute once, pass to both the connection and the post-auth admit) so a single frame is stamped consistently.

### Security properties (why this closes (a) and (b))

- **(a):** The pre-auth tier keys on the *connection's own* iroh identity, which iroh authenticates in the QUIC/TLS handshake — a spoofer cannot fill a victim's window because the key is the attacker's transport identity, and cannot cheaply rotate it (each new key is a fresh endpoint the flood shield still counts, and the 8192-cap eviction bounds the map). The 20/hr window + dedupe now run only post-auth, so they only ever see identities that produced a valid signature — an unauthenticated spoofer never reaches them.
- **(b):** Requester and voucher have disjoint window+dedupe namespaces; O's requests and O's vouches no longer share a budget.
- **Residual (accepted, documented):** a Sybil flood of *distinct* iroh endpoints each does one cheap pre-auth-shed-or-fail-auth unit of work; the expensive work (verify then dial/relay) stays gated behind authentication, and per-endpoint cost + memory are bounded. This is DoS-degradation, not a bypass — consistent with the ZEB-694 framing.

---

## Part 2 — Accept path: consume-on-`Linked` + in-flight guard + TTL

### Current state

`accept_friend_request_impl` (`src-tauri/src/lib.rs:54612-54756`), IntroductionOffer branch (`:54669-54749`):

1. `store.has_offer(&addr)` gate (`:54669`) — non-consuming peek, used only to validate that handles are present.
2. `store.take_offer(&addr)` (`:54700`) — **irreversibly consumes** the offer.
3. `complete_introduction(...)` dials and returns `Result<AddFriendOutcome, String>` (`lib.rs:55628`, `Ok(outcome)` `:55741`), where `AddFriendOutcome::Linked { .. }` (`:55344`) is the success discriminant it already matches on internally to gate durability (`:55696`).
4. The result is discarded: `result.map(|_outcome| ())` (`:54748`) — a `Pending`/`Unreachable` outcome (dial failed / reachability stale) is reported to the IPC as `Ok(())`, *and the offer is already gone*.

There is no in-flight guard; the only concurrency defense is the one-shot `take_offer`, with an acknowledged peek/take race (`:54696-54699`).

### New store methods (`src-tauri/src/friend_requests.rs`)

Add to `PendingFriendRequests` (`Inner` at `:65-71` gains one field):

```rust
// Non-consuming clone of a staged offer plus its received_at_ms (both StoredIntroductionOffer and
// ReachabilityAnnouncePayload derive Clone). The received_at lets the accept path apply the TTL
// without a second accessor. Returns None if the entry is absent or a plain LinkRequest.
pub fn peek_offer(&self, subject: &OwnerAddr) -> Option<(StoredIntroductionOffer, u64)>;

// In-flight guard. `Inner` gains `accepting: HashSet<OwnerAddr>`.
pub fn try_begin_accept(&self, subject: OwnerAddr) -> bool; // test-and-set under the lock: false if already accepting
pub fn end_accept(&self, subject: &OwnerAddr);              // remove marker
```

An RAII guard clears the marker on every exit path (including early returns / panics):

```rust
pub struct AcceptInFlightGuard { store: Arc<PendingFriendRequests>, subject: OwnerAddr }
impl Drop for AcceptInFlightGuard { fn drop(&mut self) { self.store.end_accept(&self.subject); } }
```

`end_accept` is sync (lock + `HashSet::remove`), so `Drop` needs no async.

### TTL on staged offers (`friend_requests.rs`)

Offers already carry `received_at_ms` (`PendingInbound`, `:60`). Add:

```rust
pub const INTRODUCTION_OFFER_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000; // 7d, matches intro.at / reachability freshness bound
fn is_offer_expired(received_at_ms: u64, now_ms: u64) -> bool { now_ms.saturating_sub(received_at_ms) >= INTRODUCTION_OFFER_TTL_MS }

// Remove every IntroductionOffer older than the TTL. Returns count swept (for tests/logging).
pub fn sweep_expired_offers(&self, now_ms: u64) -> usize;
```

`sweep_expired_offers` is called from `list_pending_friend_requests_inner(store)` (`lib.rs:54531`, the `_inner` seam behind the `list_pending_friend_requests` IPC) before it projects the DTOs, so the UI stops showing dead offers. It is also the authoritative check at accept time (below). It only sweeps `IntroductionOffer` entries — plain `LinkRequest` entries have their own lifecycle and are untouched.

### New accept flow (`lib.rs`, IntroductionOffer branch)

Replacing steps 1-4 above (handle-snapshot logic at `:54622-54668` unchanged):

1. **Begin guard.** `if !store.try_begin_accept(addr) { emit friend-list-changed; return Err("This introduction is already being accepted.") }`. Bind an `AcceptInFlightGuard { store: Arc::clone(&store), subject: addr }` so the marker clears on any return.
2. **Handle validation** (as today) — if the owner-state handles are unavailable, return `Err(OWNER_NOT_LOADED_MSG)` *without consuming* (guard clears the marker).
3. **Peek, not take.** `let Some((offer, received_at)) = store.peek_offer(&addr) else { emit; return Err(OWNER_NOT_LOADED_MSG) }`.
4. **TTL check.** `if is_offer_expired(received_at, now)` → `store.take_offer(&addr)` (drop the dead entry), emit, `return Err("This introduction has expired — ask them for a fresh one.")`.
5. **Dial.** `let result = complete_introduction(offer.subject, offer.reachability, ...).await;` (same args as today).
6. **Consume only on `Linked`.**
   - `Ok(AddFriendOutcome::Linked { .. })` → `store.take_offer(&addr)` (consume; safe now), emit `friend-list-changed`, `return Ok(())`.
   - `Ok(AddFriendOutcome::Pending | Unreachable)` → **do not consume** (offer stays staged for retry), emit, `return Err("Couldn't reach them right now — the introduction is saved, try Accept again later.")`.
   - `Err(e)` → **do not consume**, emit, `return Err(e)` (already user-presentable via the tunnel/dial error text).
7. The `AcceptInFlightGuard` drops at function exit, clearing the marker in every branch.

The IPC contract stays `Result<(), String>`: `Ok(())` == linked; any `Err` == not linked, with a specific message. Because a non-`Linked` accept leaves the offer staged and emits `friend-list-changed`, the request row re-renders and remains actionable — retry works.

### Frontend (`src/lib/friend-service.ts`, `src/lib/components/FriendsPanel.svelte`)

The accept handler already surfaces a rejected `Result` as an error toast (Tauri error extraction: `e instanceof Error ? e.message : String(e)`). Two adjustments:

- Ensure the accept handler does **not** optimistically remove the request row before/independent of `friend-list-changed`; on `Err`, the row must remain so the user can retry. (If it currently removes optimistically, drive removal solely from the refreshed `list_pending_friend_requests` after the `friend-list-changed` event.)
- The toast copy for the non-linked / expired messages comes straight from the backend `Err` string — no new i18n keys required.

### ZEB-693 Gap 2 (folded in)

Steps 1-7 above are only verifiable if the introduction-accept branch is reachable from a test. Today `accept_friend_request_impl` / `complete_introduction` are `pub(crate)` and unreachable from the external `tests/` crate (the ZEB-693 Gap 2 finding). We add an **in-crate `#[cfg(test)]`** seam that drives the branch with injected handles and a stubbed/short-circuit dial so we can assert, per outcome: offer consumed iff `Linked`; offer retained on `Pending`/`Unreachable`/`Err`; the in-flight guard rejects a concurrent second accept and never double-dials; expired offers are dropped with the expired error. Implementing this closes ZEB-693 Gap 2; ZEB-693 is trimmed to Gap 1 only.

---

## Files touched

| File | Change |
|---|---|
| `src-tauri/src/friend_intro.rs` | Extract `KeyedSlidingWindow<K>` + `KeyedDedupe<K>`; restructure `IntroRateLimiter` (2 tiers, 3 methods, `with_caps`); add `INTRO_PER_CONNECTION_MAX`; remove old `admit`. |
| `src-tauri/src/iroh_pex_acceptor.rs` | Both arms: `admit_connection` pre-auth + `admit_requester`/`admit_voucher` post-auth. |
| `src-tauri/src/friend_requests.rs` | `peek_offer`; `accepting` set + `try_begin_accept`/`end_accept` + `AcceptInFlightGuard`; `INTRODUCTION_OFFER_TTL_MS` + `is_offer_expired` + `sweep_expired_offers`. |
| `src-tauri/src/lib.rs` | Rework IntroductionOffer branch of `accept_friend_request_impl` (guard → peek → TTL → dial → consume-on-`Linked` → distinguishable return); call `sweep_expired_offers` in `list_pending_friend_requests_inner`; in-crate `#[cfg(test)]` accept-branch coverage. |
| `src/lib/friend-service.ts`, `src/lib/components/FriendsPanel.svelte` | Keep request row on non-linked accept; surface backend message. |

No wire-format files (`tests/wire_format/zeb37{5,6}_*`) change.

## Testing strategy

**Primitives (`friend_intro.rs` unit tests):** `KeyedSlidingWindow` — cap enforced, window prune drops stale timestamps, `evict` two-pass fires at the 8192 boundary to the 3/4 watermark. `KeyedDedupe` — duplicate within TTL rejected, past TTL admitted, `evict` two-pass at the boundary.

**Limiter (`friend_intro.rs` unit tests, via `with_caps` for tiny deterministic caps):**
- **Role independence (Greptile regression):** fill `admit_requester` to its cap, then an `admit_voucher` for the same owner still admits.
- **Connection shield:** flood `admit_connection(idA, ..)` to its cap → shed; `admit_connection(idB, ..)` still admits.
- **Pre-auth cannot poison post-auth:** post-auth methods are only reached by the acceptor after auth — assert at the limiter level that requester/voucher windows are untouched by `admit_connection` calls (disjoint state).

**Acceptor coverage (no bespoke flood harness):** `serve(&self, conn: &Connection)` has no unit harness for real iroh connections (the existing `iroh_pex_acceptor::tests` drive the pure decision fns, not `serve`), so a per-connection flood test at the acceptor would need a real two-endpoint integration harness of marginal value over the limiter unit tests above. Acceptor-level coverage of the shed/role-independence behavior is therefore split across two layers: the shed and role-independence *decisions* are proven by the limiter unit tests (`admit_connection`/`admit_requester`/`admit_voucher`, above), and the acceptor's wiring of those decisions at the correct auth boundary is proven by the 3-node e2e `introduction_broker_roundtrip` staying green (happy path through both arms).

**Accept path (`lib.rs` in-crate `#[cfg(test)]`, ZEB-693 Gap 2):** peek-not-consume (offer present after a `Pending` dial); consume-on-`Linked` (offer gone after `Linked`); in-flight guard (concurrent second accept returns the in-progress error, single dial); TTL expiry (stale offer dropped + expired error; `list_pending_friend_requests_inner` filters it).

**Frontend (`vitest`):** accept returning a rejected promise keeps the request row and shows the message.

## Acceptance criteria

- [ ] Pre-auth admission is keyed on `conn.remote_id()`; the 20/hr window + dedupe run only post-authentication; requester and voucher use disjoint windows/dedupe (no cross-role collision, no spoofed-key poisoning of a victim's post-auth quota). Old `admit` removed.
- [ ] All keyed maps retain 8192-cap eviction; benign-ack-on-shed preserved at every tier.
- [ ] A failed/stale AskMe accept dial leaves the offer staged (consumed only on `Linked`); a concurrent second accept is rejected by the in-flight guard and does not double-dial; offers older than 7d are swept and rejected with a clear message.
- [ ] The introduction-accept branch has direct in-crate automated coverage (ZEB-693 Gap 2 closed; ZEB-693 trimmed to Gap 1).
- [ ] `zeb375`/`zeb376` wire fixtures byte-identical.
- [ ] Gates green: fmt, clippy `-D warnings` `--all-targets --features test-fixtures`, nextest, MSRV, tsc, vitest.
