# ZEB-930 Parts 2–3 — beacon/pkarr bind seams + boot over-dial — design

**Ticket:** ZEB-930 (parent ZEB-909, R4 epic). Continues ZEB-928, which wired the
admission oracle's `node_id → enrolled_device_key` binding only on the
address-book steady-state feed (`address_book_sync::ingest_verified_row`).

## Problem

The R4 admission oracle enforces bounded-degree dialing: `admit(node_id)` dials a
peer only when the enrolled device key bound to its `node_id` is in the
ring-neighbor "admitted" set. An **unbound** `node_id` **fails open** — it is
dialed unconditionally (`admission_oracle.rs:73`).

Only `address_book_sync` calls `note_enrolled_binding` today. So peers discovered
by other verified paths are dialed with no degree bound until an address-book row
later binds them:

- **Beacon / pkarr rendezvous** (`community_gateway_dial_driver.rs:742`): a
  vouch-verified rendezvous beacon is `seed_from_pkarr`-ed (which fires the
  supervisor kick) with no binding. The member's enrolled key
  (`hit.membership_device_vk`) is in hand — already re-validated against the
  community's enrolled set at `:709` — but never forwarded to the oracle.

- **Boot looseness (Part 3):** durable-CRDT membership replay seeds the resolver
  via `update()` and can fire kicks, but that path never calls
  `note_enrolled_binding`; and any ingest before the oracle installs at
  `event_loop.rs:1652` gets a no-op bind. Both leave `node_id`s failing open
  until a runtime address-book/beacon ingest re-binds them.

## Part 2 — thread the enrolled key through `seed_from_pkarr`

Add `enrolled_vk: Option<[u8; 32]>` to `seed_from_pkarr`. It binds **inside** the
function, immediately before the `update_with_source` that fires the auto-kick:

```rust
pub async fn seed_from_pkarr(
    &self,
    owner_addr: OwnerAddr,
    _device_hash: DeviceIdentityHash,   // device-ADDRESS hash — NOT the enrolled vk
    enrolled_vk: Option<[u8; 32]>,      // signing-identity Ed25519 vk — the OTHER notion
    payload: ReachabilityAnnouncePayload,
) {
    if let Some(vk) = enrolled_vk {
        self.note_enrolled_binding(owner_addr.0, payload.iroh_node_id, vk);
    }
    // …existing PkarrLive update (fires the auto-kick) unchanged…
}
```

**Why thread it through, not call at the site.** `seed_from_pkarr` fires the kick
internally, so binding inside it makes "bind before kick" structurally impossible
to get wrong for this and every future caller, and makes the redundant explicit
kick at `:747` trivially safe (binding already set).

**Two-hash discipline.** `_device_hash` is the device-*address* hash
(`DeviceIdentityHash`, `[u8;16]`); `enrolled_vk` is the enrolled signing-identity
Ed25519 key (`[u8;32]`). They are different notions and must never converge. The
signature keeps both, side by side, so the distinction is visible at the seam. A
placeholder enrolled vk is **never** synthesized — a caller without a
membership-verified key passes `None`.

### Call sites

| Caller | `enrolled_vk` | Rationale |
|---|---|---|
| Beacon rendezvous — `community_gateway_dial_driver.rs:742` | `Some(hit.membership_device_vk)` | Vouch-verified, re-validated vs enrolled set at `:709`. **Closes the gap.** |
| Invite-redeem rung-0 — `lib.rs:64416` | `None` | Joiner is not a member; holds no membership-verified key for the inviter. Fail-open unchanged. |
| Invite-redeem retry-dial — `lib.rs:64574` | `None` | Same. |
| Invite-redeem witness ladder — `lib.rs:64750` | `None` | Same (seeds under the witness's owner). |
| Tests (`reachability_resolver.rs` ×2, `community_gateway_dial_driver.rs:1912`, `pkarr_net` integration) | `None` / `Some` | Existing pass `None`; add a `Some(vk)` binding assertion. |

### Correctness

`bind(owner, node_id, vk)` is scoped by owner (`admission_oracle.rs:87`), so the
beacon bind under `beacon_owner.0` cannot evict a co-resident owner's binding for
a shared `node_id`. When the oracle is disabled (peer mode) or unwired,
`note_enrolled_binding` is a no-op, so behavior is byte-for-byte unchanged there.

## Part 3 — quantify the boot over-dial, fix only if material

Audit the boot ingest seams that feed the resolver and can fire kicks; classify
each binding vs non-binding; determine how long a boot-seeded member fails open
(the window until `address_book_sync` delivers a binding row in steady state).

- **If immaterial** (window closes quickly in steady state): record the verdict
  in the ticket and add a regression guard pinning the ordering /
  fail-open-is-bounded property. No production change.
- **If material**: bind at the durable-CRDT membership-replay seam (the member's
  enrolled key is available there) in this same PR.

The Part 3 materiality verdict is surfaced to the reviewer before any scope
expansion into a fix.

## Testing

1. **Resolver unit (`Some` binds):** enabled oracle, `publish_admitted` a set that
   excludes `vk`; a `node_id` X is fail-open (`admit(X) == true`) before;
   `seed_from_pkarr(owner, _, Some(vk), payload{iroh_node_id: X})`; after,
   `admit(X) == false` — proves the binding landed and flipped classification.
2. **Resolver unit (`None` no-op):** same setup, `None` → `admit(X)` stays `true`.
3. **Beacon-path guard:** a driver-level test that a vouch-verified beacon seed
   binds `node_id → membership_device_vk` in the oracle.
4. Update the three existing `seed_from_pkarr` test/prod call sites to the new
   arity.

## Non-goals

- No change to the invite-redeem fail-open posture (accepted; a joiner has no
  membership-verified key to bind).
- No change to peer-mode behavior.
- The seed-owner "split" tolerated by `freshest_across_owners` (ZEB-824 §9) stays
  out of scope.
