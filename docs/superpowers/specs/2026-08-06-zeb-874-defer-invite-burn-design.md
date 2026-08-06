# ZEB-874 Tier 1: defer the invite burn to post-delivery — design

**Ticket:** [ZEB-874](https://linear.app/zeblith/issue/ZEB-874) — *Invite redeem is fire-and-forget: host burns the single-use invite + records a phantom member on ANY post-commit join failure.*

**Scope decision (Jake, 2026-08-05):** Tier 1 of the three options surfaced during brainstorming. Move the single-use-invite burn so it fires only after the countersign response has been handed to the transport, instead of on the host's local insert. No wire-protocol change. The ack leg (Tier 2) and the phantom-member reversal (Tier 3) are explicitly **out of scope** and remain on ZEB-874 for a future revisit.

## Problem

The invite-redeem acceptor burns the single-use invite the moment the joiner's `PendingJoin` inserts locally, *before* it writes the countersign back to the joiner:

- `community_invite::handle_unicast` calls `unregister_invite(&signed.invite_token.sig)` on `InsertOutcome::Inserted` (two sites: new-shape PendingJoin `community_invite.rs:2303-2308`, legacy `:2374-2379`).
- Only afterward does `iroh_invite_acceptor::handle_invite_handshake_inbound` poll for the auto-minted `JoinCountersign` and write it back (`:518`), then best-effort `conn.closed()` (`:844`).

So any failure between the insert and a successful response write — `CountersignTimeout`, `ResponseTooLarge`, an io-timeout, or the connection being lost at write time — leaves the invite consumed for a join that did not complete. The legitimate joiner then cannot retry without the host minting a fresh invite.

**Interaction with ZEB-833 (merged):** the mint-side guard now refuses to publish an invite whose `admin_bootstrap` wouldn't verify, so the *signature-specific* trigger is already closed. The residual is transport-side post-insert failure.

## Design

Move the burn from `handle_unicast` to the acceptor, gated behind the successful response write.

1. **`community_invite.rs` — `handle_unicast`:** delete both `unregister_invite` blocks and drop the now-unused `pkarr_invite_publisher` parameter. `handle_unicast` becomes purely verify + insert; it no longer decides invite lifecycle. (Verified: `handle_unicast` has exactly one live caller — the acceptor — so no other transport loses its burn. The old `dm_outbox` demux that also called it was removed in ZEB-710.)

2. **`iroh_invite_acceptor.rs` — `handle_invite_handshake_inbound`:** after `self.write_len_prefixed_cbor(&mut send, &countersign).await?` succeeds (`:518-519`), burn the invite:
   ```rust
   if let Some(pubr) = self.pkarr_invite_publisher.as_ref() {
       pubr.unregister_invite(&signed.invite_token.sig).await;
   }
   ```
   Both `signed` (decoded at `:430`) and the publisher field are already in scope. Drop the dead arg at the call site (`:460`) and update the stale field doc (`:207`).

### Why it's correct

- The `?` on the write means a delivery failure returns before the burn — the invite stays live for retry.
- The burn fires on both `Inserted` and `AlreadyKnown` (a retransmit still delivers the countersign, so burning is correct); `unregister` is idempotent (pinned by `unregister_without_prior_register_is_safe` / `unregister_nonexistent_handle_is_safe`), so a double-burn on a retransmit is a safe no-op. Happy-path end-state is identical to today.
- Open-join is untouched — it returns before the invite branch and has no invite token to burn.

### Honest scope boundary

This closes the burn on cases where the host **fails to write the response**. It does **not** close the case where `write_all` succeeds locally (QUIC-buffered) but the joiner never processes it — QUIC write-success ≠ joiner-received. That residual, and the phantom `JoinCountersign` member (a CRDT event already committed; reversing it fights CRDT monotonicity), are Tier 2/3.

**Concurrent double-use is out of scope and unchanged.** `unregister_invite` only stops *advertising* the pkarr handle (the relay's PUT record lingers until TTL); it is not an atomic single-use *claim*, and `verify_event` only checks same-actor prior state. So two distinct actors racing the same untargeted invite could always both redeem — that was true before this change too. Deferring the burn marginally lengthens the window during which advertising continues, but does not introduce a new class of double-use (single-use for distinct actors was already unenforced). The fix is an atomic per-token claim, which is exactly the Tier-2 ack-leg / invite-lifecycle work already scoped on ZEB-874 — intentionally not folded into this Tier-1 change.

## Testing

Invite-handle liveness is observable in the existing two-party harness (`tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs`) via `pkarr_publisher.active_handles()` — the burn = the `invite:{hex(sig)}` handle leaving the active set. (Re-resolving the mock relay is unreliable: `PkarrPublisher::unregister` stops future republishes but does not DELETE the PUT record, which lingers until TTL — the existing untargeted test documents this and uses `active_handles()` as the deterministic signal.)

1. **Regression (deterministic):** the existing untargeted roundtrip already asserts the handle leaves `active_handles()` after a successful join — keep it (proves the moved burn still fires on success) and correct its comment to the new burn location. Add the same assertion to the targeted happy-path test `bob_joins_alice_via_iroh_handshake_option_a`.

2. **Negative (deterministic):** a new test stands up the acceptor with `poll_deadline = 0`. The acceptor's poll loop checks the deadline **before** scanning state, so a zero deadline times out on the first iteration without ever observing a countersign — the async auto-countersign cannot win a race that never runs, making the `CountersignTimeout` deterministic regardless of scheduling. Bob's `PendingJoin` still inserts, then the acceptor returns `CountersignTimeout` **before** the write. The test asserts (a) Bob's outcome is not `"joined"`, (b) Alice's state contains Bob's `PendingJoin` (proving the failure is genuinely post-insert — pre-fix would have burned here), and (c) `invite:{hex(sig)}` **remains** in `active_handles()`. Pre-fix this fails (handle_unicast already burned on insert); post-fix it passes.

   The deadline-before-scan ordering is a small correctness improvement to the acceptor poll loop in its own right (a zero/elapsed budget should not get a free scan); production uses a 10s deadline, so the ordering is a no-op there.

A dedicated transport write-failure test is deliberately **not** added: forcing `write_all` to error over real QUIC is inherently racy (small writes buffer and return `Ready`), and this suite avoids flaky tests. The write-failure branch of the invariant is guaranteed instead by the `?` placement plus the compile-enforced removal of the publisher param from `handle_unicast`.

## Non-goals

- No ack leg / wire-protocol change (Tier 2).
- No phantom-member reversal (Tier 3).
- No change to single-use-invite retry *semantics* beyond "a failed delivery no longer consumes the invite."
