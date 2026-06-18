# ZEB-495 (ZEB-340 Part 2) — Device-introduction event: >1 enrolled device per owner per community

- **Issue:** [ZEB-495](https://linear.app/zeblith/issue/ZEB-495) (parent [ZEB-340](https://linear.app/zeblith/issue/ZEB-340) Part 2; grandparent ZEB-217)
- **Status:** design 2026-06-18 — core approved (direction confirmed by Jake); **emit-path fork pending a steer**
- **Predecessors:** ZEB-339 (community membership signs+verifies with the enrolled device key #2 + `EnrollmentCert`), ZEB-492 (fleet KeyTree distributed to cert-only devices at pairing), ZEB-372 (real device X25519 in `PubKeyBundle`)

## Problem

A community member's per-owner state already carries a **set** of enrolled device keys
(`MemberState.enrolled_device_keys: BTreeSet<[u8;32]>`, `community_membership.rs:1444-1451`),
and the entire verify path already accepts more than one. But **no event ships that adds a
second device of the same owner**, so in practice an owner is single-device-per-community: a
paired second device can read the community (it has the KeyTree) but cannot author messages or
publish state that other members accept, because its device key is not in any member's
`enrolled_device_keys`. This blocks the core ZEB-169 promise — "my ~12 devices all recognised as
me" — at the community surface.

## Key finding (forensic map, 2026-06-18)

**The verify path is already multi-key-correct.** Every consumer of `enrolled_device_keys`
iterates the full set and accepts the first key that verifies:

- `resolve_enrolled_signer` — `community_membership.rs:1294-1315` (steady-state signer)
- `verify_countersig` — `community_membership.rs:1186-1208`
- `verify_invite_token_sig_with_enrolled` — `community_membership.rs:1325-1344`
- `verify_publisher_sig` — `community_state_sync.rs:3096-3120` (state-root publish auth)
- `verify_channel_event` — `community_channel_log.rs:746-760` (message author auth)

So once a second key is present in the set, messages and events signed by **either** device
verify with **zero further changes**.

**The only gap is population.** The materialize `Join` arm no-ops when the owner is already
`Joined` (`community_membership.rs:1848-1855`, `should_refresh = false` for `Joined`), so a plain
second-device `Join` is dropped and its cert key never inserted. Nothing else emits a key-adding
event.

## Design — the device-introduction primitive (fork-independent)

A new membership event kind that the second device self-signs and that, on merge, **adds the
device's key to its owner's existing `MemberState`** without disturbing status, `joined_at`, or
power.

### Unit 1 — `MembershipEventKind::DeviceAnnounce` (new variant)

- Add a unit variant `DeviceAnnounce` to `MembershipEventKind` (`community_membership.rs:83-322`),
  adjacently-tagged with a free single-char wire code (free set per the map: `e h o s t v w z`).
- It carries no body: the introduced device's identity is the `EnrollmentCert` on the
  `SignedMembershipEvent.enrollment` field (`community_membership.rs:436-437`), exactly as `Join`
  carries it. The cert's `device_pubkeys.classical.ed25519_verify` is the key being added.
- Signed by the **second device's own** enrolled device key (#2) via the existing
  `sign_event_with_identity` path; the caller attaches the second device's own cert to
  `enrollment` after signing (mirrors the `Join` attach at `lib.rs:20132` / `:21664`).

### Unit 2 — `verify_event` acceptance

In the signer-resolution match (`community_membership.rs:2701-2709`), add `DeviceAnnounce` to the
**identity-introducing** arm so the signer is resolved via `enrolled_key_from_cert` (the same
function `Join`/`PendingJoin` use). That already enforces:

- `cert.verify(event.at)` — Master signature valid, not expired (`community_membership.rs:1273`)
- **Master issuer only** — non-`Master` rejected (`:1277-1282`); Quorum certs are ZEB-340 Part 3
- `cert.owner_id == event.actor.0` (`:1283`) — the cert binds the introduced device to the
  acting owner

Then add one authorization rule for `DeviceAnnounce` in the per-kind section: **the actor must
already be a `Joined` member** in `prior_state` (else reject with a new
`DeviceAnnounceForNonMember` error). This is the whole authorization story — a valid Master-signed
cert for an already-admitted owner is sufficient; no admin countersign is required (the admin
vouched for the *owner*, not per-device; adding another of that owner's own master-attested
devices introduces no new principal). In invite-only communities `DeviceAnnounce` therefore
**bypasses** the PendingJoin/countersign gate by construction (it is not a `Join`/`PendingJoin`).

### Unit 3 — `materialize` insertion arm

Add a `DeviceAnnounce` arm to `materialize_with_now` (`community_membership.rs:1832+`):

```text
DeviceAnnounce =>
    if let Some(member) = members.get_mut(&event.actor)        // owner must exist
        if member.status == Joined
            if let Some(cert) = &event.enrollment              // defensive; verify_event ensured it
                member.enrolled_device_keys.insert(cert.device_pubkeys.classical.ed25519_verify)
```

`get_mut`-and-insert inherently preserves every other field (clone-preserve discipline is
automatic). Idempotent: re-announcing an already-present key is a no-op `BTreeSet::insert`. The
cert is ingested without re-verification, consistent with the documented `Join`-arm security
invariant (`community_membership.rs:1865-1875`) — `verify_event` is the sole cert gate.

### Unit 4 — tests (fork-independent)

- `materialize_records_enrolled_device_key_from_device_announce` — mirror of
  `materialize_records_enrolled_device_key_from_join_cert` (`community_membership.rs:6387`): a
  `Joined` owner + a `DeviceAnnounce` carrying a second device's Master cert ⇒ the second key
  lands in `enrolled_device_keys` and `status`/`joined_at`/power are unchanged.
- `verify_event_accepts_device_announce_from_joined_owner` and
  `verify_event_rejects_device_announce_from_non_member`.
- `both_enrolled_devices_verify_after_announce` — build a `MaterializedMembership` with two keys
  for one owner (seed: `joined_prior_two_members`, `community_membership.rs:10316`) and assert a
  steady-state event signed by **either** device passes `resolve_enrolled_signer`, and a channel
  post signed by either passes `verify_channel_event`.
- `device_announce` wire-format round-trip + a pinned CBOR fixture (mirrors the existing
  membership wire-fixture tests).

This core (Units 1–4) is identical regardless of the emit-path decision below and is implemented
first.

## Emit-path fork (needs a steer; default = Option A)

The introduction event must reach other members' community engines. It is gated en route by
`verify_publisher_sig` (`community_state_sync.rs:3310`), which authorises the **whole zenoh
state-root publish** against the publisher's materialized `enrolled_device_keys` — **with no cert
path**. The second device's key is not yet in that set (that is exactly what `DeviceAnnounce`
adds), so the second device **cannot self-publish** its own introduction: chicken-and-egg. The
per-event cert gate (`enrolled_key_from_cert`) runs only *inside* a blob that has already passed
`verify_publisher_sig`.

### Option A — relay via an already-enrolled device (recommended)

The second device constructs and signs the `DeviceAnnounce` (with its own key + cert) and
deposits it into a new owner-private **fleet dataset** (`community-device-intro`, on the existing
`FleetSyncEngine` substrate — the second device already has the KeyTree via ZEB-492). An
already-enrolled device of the same owner, subscribed to that dataset, drains each pending intro
and calls `insert_local_event` on the matching community engine. That device's key **is** enrolled,
so its next state-root publish carries the `DeviceAnnounce` and passes `verify_publisher_sig` on
every peer; each peer then accepts the event via `enrolled_key_from_cert` (Unit 2) and inserts the
second key (Unit 3). Thereafter the second device's own publishes pass `verify_publisher_sig`
directly.

- **Pros:** no wire-format change; no change to the security-critical publish gate (it stays the
  single source of truth — the materialized enrolled set); reuses the fleet-sync substrate and the
  established `admin_bootstrap` relay pattern; future per-device revocation (removing a key from
  the set) is naturally respected.
- **Cons:** liveness coupling — an already-enrolled device of the owner must come online to relay
  (consistent with the always-on butler model, ZEB-418). Until then the second device can read but
  not yet author in that community.

### Option B — cert-exemption at the publish gate

Add an optional `EnrollmentCert` field to `CommunityRootPublishPayload` (`community_state_sync.rs`
~220) and a cert fallback in `verify_publisher_sig`: if the publisher's signing key is not in the
enrolled set but the payload carries a valid Master cert with `owner_id == publisher_addr` for a
`Joined` owner, accept. The second device self-publishes directly.

- **Pros:** no relay, no liveness coupling; any master-attested device is autonomous immediately.
- **Cons:** wire-format change; widens the security-critical publish gate to trust a cert directly
  rather than the materialized set, which complicates future device revocation (a revoked device's
  cert still verifies structurally unless an explicit revocation check is added) — a new invariant
  to get right.

### Recommendation

**Option A.** It keeps the publish gate's trust model intact, requires no wire change, and reuses
existing substrate; the liveness cost is acceptable and aligns with the butler model. Option B is
the better end-state if relay-liveness proves limiting, but it is a heavier, security-sensitive
change best taken deliberately. Building the core (Units 1–4) does not commit to either; the
emit-path units below are written for Option A and will be revised if the steer is B.

### Unit 5 (Option A) — `community-device-intro` fleet dataset + relay consumer

- A new `FleetSyncEngine`-backed dataset holding `{community_id, signed_device_announce_bytes}`
  entries (a small append/LWW set keyed by `(community_id, device_id)`), wired in `lib.rs`
  start_node like the other fleet consumers.
- A drain consumer that runs on any enrolled device: for each pending intro whose `community_id`
  matches a community this device is enrolled in, decode the `SignedMembershipEvent` and
  `insert_local_event` it into that community's engine; on success, tombstone the dataset entry.

### Unit 6 — runtime trigger (second device self-introduces)

After community state sync materializes on the second device, for each community where the owner
is `Joined` but this device's enrolled key is absent from its own `MemberState`, construct and
deposit a `DeviceAnnounce` (Option A) — once per `(device, community)`, idempotent. Natural seam:
the community engine's post-merge hook (the `insert_event` choke point at
`community_state_crdt.rs:294` / the sync engine's applied callback).

## Out of scope

- ZEB-340 Part 1 (unify DM + owner-state signing onto device #2 — re-keying `OwnerDeviceCache`),
  Part 3 (Quorum-issued device certs), Part 4 (inert `admin_identity_pub` / legacy
  `admin_bootstrap` cleanups). Each is a separate ticket/PR.
- Per-device **revocation** from a community (removing a key from the set) — a distinct feature.
- Cross-WAN pairing (ZEB-197 deferred v3) and the two-machine live proof (needs AVALON/Ildwyn).

## Security analysis

- A `DeviceAnnounce` can only **add** a key to an **already-admitted** owner's set, and only a key
  the owner's **master key** vouched for (Master-signed cert, `owner_id == actor`). It cannot
  change status, power, or membership, so it grants no escalation: the worst a forged/replayed
  attempt can do is fail `enrolled_key_from_cert`.
- `verify_membership_signer` binds `signer.owner == event.actor`, so a cert for owner X cannot
  authorize an announce acting as owner Y.
- Option A changes no auth gate. Option B widens `verify_publisher_sig`; its cert path must
  re-assert `owner_id == publisher_addr` and `Joined`, matching `enrolled_key_from_cert`.

## Testing

- Rust unit/integration per Unit 4 (verify + materialize + two-key + wire fixture).
- Gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings`, `cargo nextest run --locked -p harmony-app --features test-fixtures` (scope to `-E 'test(community)'` during dev; full `--all-targets` for the final sweep).
- The full two-device live flow (pair → second device authors in a shared community, visible to a
  third member) is the manual / two-machine validation (AVALON/Ildwyn), tracked on ZEB-495.

## Acceptance

After a second device pairs into an existing owner identity (ZEB-494 path), and the owner is
already a member of community C, the second device's key is added to the owner's `MemberState` in
C, and a message or state-update authored by **either** device verifies for every other member of
C — with both device keys visible in the materialized membership.
