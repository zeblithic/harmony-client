# ZEB-743 — Converge friend Case-D resolve onto the core rendezvous kernel

**Goal:** Migrate the harmony-client friend Case-D resolve path off its hand-rolled
`resolve_window` loop onto the already-shipped core `harmony_pkarr::rendezvous`
driver, so both rendezvous consumers (community + friend) go through one kernel.

**Architecture:** Client-only, single PR. No core change, no pkarr-pin bump.

**Tech stack:** Rust, `harmony-pkarr` (core, pinned `80f6d80` — already contains the
kernel), tokio, chacha20poly1305 (friend payload seal), pkarr 3.10.0.

## Background

ZEB-571 Tier-1 item 1 (DHT-slot rendezvous). The audit described this as a
greenfield extraction, but premise verification against current `main`
(client `3637f337`, pkarr pin `80f6d80`) found the reusable kernel **already
shipped** in core `harmony_pkarr::rendezvous` (harmony PR #283, 2026-06-27):
the `SlotResolver` trait, the escalating-batch first-responder-wins
`resolve_rendezvous_with` driver, `PkarrSlotResolver`, `slot_for_advertiser`,
and the config/outcome types. **Community already consumes it**
(`community_rendezvous.rs:112`). The module-in-`harmony-pkarr` placement is
correct and final (a separate `harmony-rendezvous` crate would be a pin-conflict
trap against the client's independent `harmony-pkarr@80f6d80` pin → two pkarr
sources → `PkarrResolver` type-unification failure). No iroh-style version skew:
pkarr is transport-decoupled, `pkarr = 3.10.0` in both revs, byte-identical
across the pin gap.

The one un-migrated consumer is friend Case-D resolve.

## The change

`pkarr_friend_publisher.rs::resolve_friend_case_d` (~:138) today builds the
3-epoch-window resolve keys, calls `resolver.resolve_window(&keys)` (a
`join_all` returning the freshest record by `announced_at_ms`), then loops the
window to unseal. Replace that body with the core driver — friend becomes a
thin `PkarrSlotResolver` consumer, mirroring community:

```rust
let window = epoch_tolerance_window(now_ms);         // [e-1, e, e+1]
let secret_for_decode = secret;                      // (Copy/Clone the 32-byte secret into the closure)
let resolver = PkarrSlotResolver::new(
    Arc::clone(pkarr),
    PkarrCase::Friend,
    secret.to_vec(),                                 // ikm
    Arc::new(move |_slot: u16, epoch: u64| case_d_info(epoch, friend_owner)), // N=1: slot ignored
    move |blob: &[u8]| window.iter()
        .find_map(|&e| open_case_d_payload(&secret_for_decode, e, blob).ok()), // decode: try the window
);
let outcome = resolve_rendezvous_with(
    &resolver, now_ms,
    &RendezvousResolveConfig { batch_curve: vec![1], per_batch_deadline: FRIEND_RESOLVE_DEADLINE },
).await;
Ok(outcome.payload)
```

The friend info-layout (`case_d_info(epoch, friend_owner)` = `epoch_be(8) ‖
owner_id(16)`) is supplied via `info_for` so the kernel derives the same
`case_d_resolve_key` it does today. `PkarrCase::Friend` selects the friend salt.
`batch_curve: vec![1]` gives the single-slot shape (the kernel's
`friend_shape_single_slot_curve_resolves` test proves slot 1 is never probed).

### Design decisions

- **Record selection = first-responder-wins** (Jake, 2026-07-23). The driver
  returns the first `(slot, epoch)` probe that yields a decoded payload;
  friend's old `resolve_window` returned the freshest-by-timestamp. They differ
  only in the brief epoch-boundary window where two records are live — and only
  matter if the friend's routing changed right at the boundary (rare,
  self-healing next tick). Community already accepted this. No `Freshest`
  strategy is added to the core driver.
- **Decode seam = client-side window unseal.** The kernel's decode closure is
  `Fn(&[u8]) -> Option<P>` (epoch-blind), but friend's payload is
  ChaCha20Poly1305-sealed with the epoch bound into the AAD. Rather than a
  cross-repo change threading the epoch into the core closure, friend's decode
  tries the 3 window epochs (`find_map` over `open_case_d_payload`) — wrong-epoch
  attempts fail instantly on AAD mismatch; N=1 so ≤ 9 trivial unseal attempts.
  Keeps this client-only.
- **`FRIEND_RESOLVE_DEADLINE = 3000 ms`** (a named const in the friend module):
  the per-batch deadline. Friend's old `resolve_window` had no explicit deadline
  (it bounded on the pkarr resolver's own per-query timeout). Set ≥ community's
  2500 ms so the deadline never cuts a slow-but-valid resolve before the
  resolver's own per-query timeout in the common case; the implementer should
  confirm pkarr's per-query timeout and bump this if it's higher than 3000 ms.
- **Error handling (deliberate unification):** the old `resolve_friend_case_d`
  propagated a pkarr resolve error as `Err(String)`; the core driver swallows
  per-probe failures into `None`, so the new body always returns `Ok(payload)`
  and yields `Ok(None)` (not `Err`) on a transient all-probes-failed pkarr
  failure. This matches the community path (also driver-based, no error
  propagation) and is correct for a best-effort resolve whose callers already
  treat `Ok(None)` as "not reachable this round." Keep the function's
  `Result<Option<…>, String>` signature (callers are unchanged); it simply never
  returns `Err` from the resolve step anymore.

## Out of scope (unchanged)

- The four resolve call sites (`lib.rs:59241/59372/59711/59910`) — only
  `resolve_friend_case_d`'s internals change; its signature stays.
- The publish/claim lifecycles (`PkarrFriendPublisher`, `sync_case_d_handles`,
  community's `refresh_slot`) — consumer-specific, not rendezvous-kernel.
- Friend keying + `seal_case_d_payload`/`open_case_d_payload` — friend-specific
  encode/decode, stay client-side (the decode closure calls into them).
- The `slot_for_advertiser` ranking — N/A for N=1 friend; never invoked.

## Testing

Behavior-preservation anchors (must pass unchanged):
- `pkarr_friend_publisher.rs`: `case_d_publish_then_resolve_round_trip` (full
  publish→resolve over `MockPkarrRelay`, unseal recovers the raw blob),
  `register_then_unregister_friend_slot`, `sync_case_d_handles_registers_only_active_with_secret`.
- `friend_rendezvous.rs`: the keying/seal tests are untouched (keying path
  unchanged).

New test:
- A multi-epoch/boundary resolve: publish under epoch `e`, resolve at a `now_ms`
  whose window includes `e`, assert the payload is recovered (exercises the
  window-unseal decode through the driver). Optionally a two-live-records case
  asserting *a* valid record is returned (first-responder semantics — not
  asserting which, to avoid a timing-dependent test).

## Risks

- **Record-selection change** (accepted): staler-but-valid record at epoch
  boundary — negligible, self-healing.
- **Deadline behavior**: introducing `per_batch_deadline` where there was none
  could change timeout behavior under a slow relay. Mitigate by setting the
  deadline generously (≥ community's 2500 ms) so it never fires before the
  resolver's own per-query timeout in the common case.
- No wire/format change: the published record, keys, and seal are all unchanged;
  only the resolve driver differs. Friend↔friend interop with un-upgraded peers
  is unaffected (this is a resolve-side-only change; nothing on the wire moves).

## Cross-repo

None — client-only. No `harmony-pkarr` change, no pin bump.
