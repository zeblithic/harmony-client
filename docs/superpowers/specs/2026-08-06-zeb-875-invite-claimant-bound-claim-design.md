# ZEB-875: claimant-bound atomic single-use invite claim — design

**Ticket:** [ZEB-875](https://linear.app/zeblith/issue/ZEB-875) — *Invite redeem: ack/rollback leg (Tier 2) + reverse phantom JoinCountersign member (Tier 3) — residual after ZEB-874 Tier 1.*

**Scope decision (2026-08-06):** After brainstorming + two source-verified recon passes, the ticket's literal "ack/rollback leg" is **not** the fix. Ship a **host-side claimant-bound atomic single-use claim** instead — one mechanism, no wire-protocol change, no CRDT retraction. The joiner→host **ack leg** is considered-and-declined (fragile under dual-channel delivery — see Non-goals). The **Tier 3** phantom-member tombstone is split to **[ZEB-876](https://linear.app/zeblith/issue/ZEB-876)** (its value drops to "reverse a benign authorized-absent member" once this lands).

Base: `main` @ `e815be49`. Branch: `zeb-875-invite-redeem-ack-rollback`.

---

## Problem (verified against HEAD e815be49)

Community invite-only redemption has three separable gaps; recon confirmed all three on current source:

1. **No atomic single-use claim (the real security gap).** `unregister_invite` only stops *advertising* the pkarr handle (`pkarr_invite_publisher.rs:65-68` → `PkarrPublisher::unregister` `state.remove` + `cancelled` flag; **no relay delete, no CAS**); the relay PUT lingers to TTL. Redemption verify (`community_membership.rs:4272-4276`, `verify_event` P6) only checks the **requesting actor's** prior state. So two *distinct* actors racing the same untargeted invite can **both** redeem.
2. **Lost-invite-on-buffered-write (the actual Tier-2 residual).** ZEB-874 moved the burn to *after* a successful `write_len_prefixed_cbor` (`iroh_invite_acceptor.rs:526-527` write → `:538-539` burn), closing the *write-fails* case. But QUIC `write_all` returning `Ready` ≠ joiner processed it (small writes buffer). On a buffered-but-unprocessed write, Tier 1 still burns, and the legitimate joiner can't retry.
3. **Phantom member (Tier 3).** The auto-counter-sign inserts a `JoinCountersign` into the host CRDT immediately (`community_state_sync.rs:2270`) and it materializes the joiner `Joined` (`community_membership.rs:3380`, `:3430-3446`); the `JoinCountersign` materialize arm is non-mutating/monotonic (`:3500-3504`) with **no retraction/tombstone kind**.

### Why "ack → rollback" is the wrong shape

The countersign reaches the joiner over **two** channels: the iroh stream *and* CRDT/Zenoh state-sync (`community_invite.rs:2355-2360`, joiner has an independent oneshot wake on `pending_redemptions`). So "no stream ack" ≠ "didn't join." A host that rolled back (re-register invite + tombstone membership) on a missing ack could **kick a member who actually joined via sync** — strictly worse than the original bug. Rollback is unsafe here; an ack could only *confirm*, never *reverse*.

### The key enabling facts (recon)

- **The claim is already-durable state.** `MembershipEventKind::PendingJoin` carries the **full `InviteToken`** including its 64-byte `sig` (`community_membership.rs:298-302` → `community_invite.rs:229-258`, `sig: [u8;64]` at `:257`); every `SignedMembershipEvent` also carries the verified `actor` (`:605`). This log is persisted (`crdt.cbor`/`replay.cbor`, `community_state_sync.rs:21/55`, unconditional persist `:2773`) and CRDT-synced. So **"who claimed token X" is a pure predicate over events that already exist** — no new field, no new persisted structure, durable across restart. (`JoinCountersign` carries only `target_event_id` `:313-321`, pointing back to the `PendingJoin`.)
- **Claimant identity is trustworthy only *after* verify.** `decode_packet` is a pure hash-consistency check (`community_invite.rs:2129`), **not** a signature verify. The authoritative `verify_envelope_sig` (`:2171`) + `verify_packet_pure` (`:2181-2197`) run *inside* `handle_unicast`. Gating a claim on the acceptor-level *unverified* actor would let a forged packet **grief-claim a token under a victim's identity**, locking the real joiner out. The claim MUST run after verify.
- **Same-actor retry already re-delivers the countersign** (`community_invite.rs:2307` `InsertOutcome::AlreadyKnown` → `Ok(())`; acceptor unconditionally re-polls + re-writes the existing countersign `iroh_invite_acceptor.rs:483-518`; burn idempotent `:533-535`). The claim must preserve this.
- **Concurrency template exists.** `try_consume_friend_token` (`pkarr_invite_publisher.rs:141-157`) is a single-`std::sync::Mutex`, check-and-mutate with **no `.await` held across the decision**, with an exactly-one-winner concurrency test (`:404-446`). We reuse the *discipline*, adapted from remove-on-consume (in-memory) to record-claimant (durable/derived).

---

## Design

### The claim rule (pure predicate over raw `PendingJoin` events)

An invite token sig is **claimed by the first actor to have a `PendingJoin` event for it.** On each redeem, after full verify, before the local insert:

- **No prior `PendingJoin` with this `invite_token.sig`** → requesting actor is the first claimant → **proceed** (insert → auto-counter-sign → deliver).
- **Prior `PendingJoin` with this sig, `actor == requesting actor`** → **idempotent retry** → proceed (downstream `AlreadyKnown` + acceptor re-poll re-delivers the existing countersign — unchanged behavior).
- **Prior `PendingJoin` with this sig, `actor != requesting actor`** → **reject** with a new `CommunityInviteVerifyError::InviteAlreadyClaimed` → the counter-sign hook never fires for the loser.

Scanned over **raw** committed events, not materialized roster — so the claim is **permanent** and robust to a later `Leave`/`Kick` or the 30-day `PendingJoin` materialize expiry (`community_membership.rs:967`, `:2351-2368`). The claim keys on the token sig alone, so it composes with targeted invites (`invitee_hint`) with no special-casing — `verify_packet_pure` still independently enforces the hint.

### Enforcement point

Inside `community_invite::handle_unicast`, in the window **after** verify + self-eligibility (`community_invite.rs:~2197`, after `verify_packet_pure` and the self-eligibility checks `:2244-2257`) and **before** `engine_arc.insert_local_event_with_pubs(...)` (`:~2295`), keyed on `(signed.invite_token.sig → verified join_event.actor)`.

**Both invite-admit branches are covered.** `handle_unicast` has two paths that insert a membership-admitting event from an invite token — the ZEB-254 `is_pending_join_shape` branch (`community_invite.rs:2275-2320`) and the LEGACY `Join`+countersig branch (`:2321-2396`). The claim gates **both**. The implementer verifies whether the legacy branch is still reachable from any live minter; if it is provably dead, an unreachability assertion test substitutes for a duplicated guard — but absent that proof, both branches carry the claim.

### Atomicity & durability

The **check-then-insert must be indivisible** against concurrent redeems (the acceptor spawns a task per `accept_bi`, so two joiners race the same community engine). Serialize the *scan-for-conflicting-`PendingJoin`-by-sig* and the *insert* under the **same lock that already guards event insertion** (the membership engine's write path) — no `.await` held across the claim decision, matching the `try_consume_friend_token` discipline. Preferred shape: an atomic claim-checked insert seam on the engine (e.g. a method that, under its insert lock, scans committed events for a matching `invite_token.sig`, branches absent / same-actor / different-actor, and inserts only in the first two cases, returning a `ClaimOutcome`). The exact signature is a plan-level decision after the implementer confirms the engine's locking; the **invariant** the spec fixes is: *no two distinct actors can both pass the claim for one sig, and the winning claimant is decided under the insert lock.*

**Durability is free:** the predicate reads persisted, synced `PendingJoin` events, so a host restart re-derives the identical verdict. No in-memory index to rebuild or lose. (Cost: an O(events) scan per redeem — redeem is rare and off the hot path, so this is irrelevant; no index needed. Single source of truth over a parallel structure that could drift.)

### Error surface (host-side telemetry; no distinct joiner message — and why)

The rejection is enforced and *diagnosed* host-side, not delivered to the losing joiner as a distinct message — because delivering one would require the wire change we declined. On **any** `handle_unicast` error the acceptor writes **no response** and closes (the burn is gated on a *successful* write, `iroh_invite_acceptor.rs:529-539`, ZEB-874). So the losing racer hits a response-read failure/timeout and maps to the **existing generic** `RedemptionOutcome.status = "inviter_unreachable"` (`lib.rs:62010/62021`) — the same status any transport failure yields. A *distinct* "invite already used" message would mean writing a typed rejection payload before close, i.e. the joiner→host protocol change that is a **non-goal** here.

That is acceptable: the security property (**exactly one actor joins**) holds regardless of the loser's message, and the legitimate claimant (same actor) always succeeds with `"joined"`. Diagnosability lives host-side:
- New engine-level `LocalInsertError::InviteAlreadyClaimed { winner }` returned by the atomic precheck.
- New `CommunityInviteVerifyError::InviteAlreadyClaimed` (with its paired `reason_tag()` arm `"community_invite_already_claimed"` — the enum has an exhaustive `reason_tag` match, so the arm is mandatory) that the PendingJoin branch maps the engine error to, plus a `tracing::warn!` at the reject so operators see the claim firing in the field. No leaked debug string reaches a user (the honest-phrasing discipline from ZEB-872 still applies to anything user-facing).

### The burn is unchanged

The post-write `unregister_invite` (`iroh_invite_acceptor.rs:538-539`, ZEB-874) stays exactly as-is — it remains the advertise-stop finalizer. The claim is the **enforcement** layer above it. On a same-actor retry the claim passes and the burn is an idempotent no-op; the loser is rejected before any insert, so no burn fires for them. Ordering stays: **claim (under insert lock) → verify-gated insert → auto-counter-sign → deliver → burn.**

---

## Correctness argument

- **Single-use for distinct actors:** two actors A, B racing one sig serialize under the insert lock; whichever inserts its `PendingJoin` first fixes the claimant; the other reads a conflicting-actor `PendingJoin` and is rejected before insert. Exactly one `Joined`.
- **Legitimate retry always succeeds:** the claim binds to A; even after a buffered-but-unprocessed write burned the advertisement, A re-redeems (it holds the invite payload, and connect uses the payload's routing, not DHT re-resolution), the claim sees `actor == A`, proceeds, and the acceptor re-delivers the existing countersign. The invite is never lost to its rightful claimant.
- **After-verify placement** prevents a forged/unsigned packet from grief-claiming a token under a victim's `actor` (the claim only ever records a signature-verified actor).
- **Restart-safe:** the verdict is a function of persisted CRDT events; a restart mid-race yields the same winner.
- **Second-order check:** the new reject path (`InviteAlreadyClaimed`) short-circuits *before* the insert, so it introduces no partially-applied state and no new event; it cannot itself strand a `PendingJoin` or fire a countersign. No new invariant is violated by the fix.

---

## Honest boundary — what this closes / leaves

- ✅ **Concurrent double-use** (distinct actors) — closed.
- ✅ **Lost-invite-on-buffered-write** (the actual Tier-2 residual) — closed.
- 🔸 **Phantom member** reduces to a **benign** case: an *authorized* claimant who committed a `JoinCountersign` but hasn't converged yet (converges on any reconnect via stream or sync). The unauthorized ghost is gone.
- ⛔ **Ack leg** — declined (see Non-goals). **Tier 3 tombstone** — split to [ZEB-876](https://linear.app/zeblith/issue/ZEB-876).

---

## Testing

Rust, `cargo nextest`, deterministic. No wall-clock races.

Enforcement is proven at the **engine seam** and the **`handle_unicast` production path** — the two layers where the claim actually lives — not at the transport layer (the claim is transport-independent).

1. **Engine exactly-one-winner under concurrency** (structure ported from `try_consume_friend_token_exactly_one_winner_under_concurrency`, `pkarr_invite_publisher.rs:404-446`): two concurrent `insert_local_claim_bound_pending_join` calls with *distinct* actors on one token sig → exactly one `Inserted`, the other `Err(LocalInsertError::InviteAlreadyClaimed { winner })`. Assert `won_a ^ won_b`.
2. **Engine sequential + same-actor idempotent + different-sig:** A claims → `Inserted`; B same sig → `InviteAlreadyClaimed{winner:A}`; A same sig again → `AlreadyKnown` (no new event); a *different* sig → `Inserted`.
3. **Engine restart-safety:** build state with A's `PendingJoin`, drop the engine, rebuild from the persisted event set, then a claim-bound insert for B on the same sig is still rejected (proves the verdict is a function of persisted events, no in-memory index).
4. **`handle_unicast` distinct-actor rejection** (production verify+claim path): drive two `handle_unicast` calls with distinct-actor signed invite packets for the same token → first `Ok(())`, second `Err(CommunityInviteVerifyError::InviteAlreadyClaimed)`; a same-actor second call → `Ok(())` (idempotent retry preserved). This is the authoritative distinct-actor proof — it exercises the real verify → claim → map path, transport aside.
5. **E2E regression** (`tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs`): a same-actor retry over the real iroh handshake still reaches `outcome.status == "joined"` (the claim doesn't break the happy path / retry over real transport). A full distinct-actor *network* e2e (a second joiner racing over iroh) is **deliberately not added**: the rejection is enforced in `handle_unicast`/the engine (both covered above) and is transport-independent, and the losing racer only ever sees the generic `"inviter_unreachable"` status (no wire change) — so a network race would add substantial harness setup for zero new *enforcement* coverage. Noted here so the omission is explicit, not silent.
6. **Legacy branch:** the sig-scan precheck is structurally inapplicable to the bare-`Join` shape (only `PendingJoin` carries an `invite_token`), and no live minter emits bare `Join` into `handle_unicast` (verified: `lib.rs:39511` emits `PendingJoin` for invite-only). The legacy branch keeps its behavior with an added unreachability `warn`/`debug_assert` documenting that a claimant-bound single-use invite should never redeem through it.

Full local gate before PR: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit` + `npx vitest run`.

---

## Non-goals

- **No wire-protocol / ack-leg change.** Declined: dual-channel countersign delivery (iroh stream + CRDT sync) means a missing stream-ack cannot safely drive rollback (would risk kicking a member who joined via sync). If ever revisited, an ack is observability-only, not a rollback driver. Recorded on [ZEB-876](https://linear.app/zeblith/issue/ZEB-876).
- **No CRDT retraction / tombstone** for a committed `JoinCountersign` (Tier 3 → [ZEB-876](https://linear.app/zeblith/issue/ZEB-876)).
- **No change to dual-channel countersign delivery** or to the post-write burn.
- **No change to targeted-invite (`invitee_hint`) semantics** — the claim composes with the existing hint check.

## Global constraints

- Rust MSRV **1.91** (CI `msrv` job). Frontend Node 20+.
- Cargo commands from `src-tauri/`; `--locked` and `--all-targets` are load-bearing; `--features test-fixtures` required for integration tests.
- No new deterministic-nonce crypto exposure; no keychain construction reachable from tests.
- Error copy: honest, user-facing, no leaked internal/debug strings (ZEB-872 discipline).
