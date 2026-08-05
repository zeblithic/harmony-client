# ZEB-827: Rendezvous-beacon membership binding — design

**Ticket:** ZEB-827 (Medium) — a community rendezvous beacon record carries no binding
between its published *transport* identity and community *membership*. The member gateway
dial (ZEB-824) currently admits any beacon that verifies under the community epoch key
(open-join parity). This design adds a principled, offline-verifiable proof that the
beacon's transport identity belongs to a Joined member.

**Predecessor:** `docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md`
§5c (the withdrawn membership gate and the epoch-envelope interim posture) and its ZEB-827
follow-up note.

**Decisions of record (Jake, 2026-08-05):**
- **Binding shape — Approach A:** the beacon's community **device** signing key vouches for
  its **transport** identity; the vouch rides in the client-controlled rendezvous routing
  blob; the resolver verifies it offline against materialized membership. No cross-repo
  (`harmony-pkarr` / `harmony`) change.
- **Trust policy — strict:** a beacon without a valid membership vouch is **rejected**, not
  admitted under epoch-envelope trust. This supersedes the ZEB-824 §5c interim posture.

---

## 1. The gap (verified against source)

A rendezvous beacon record (`harmony_pkarr::PkarrRoutingRecord`) proves two things and binds
neither to membership:

1. **Outer BEP44 signature** — the writer holds the community **epoch key** (the slot keypair
   is derived from it). This is an *admission-layer* proof: anyone in the community's epoch —
   or anyone who has breached backward-secrecy and holds the epoch key — can write any slot.
2. **Inner Ed25519 signature** (`inner_sig`, over `(routing_blob, harmony_identity_pub,
   announced_at_ms, valid_until_ms)`) — the writer holds the private key for the **transport
   identity** `harmony_identity_pub` (X25519‖Ed25519, 64 bytes), verified in
   `PkarrRoutingRecord::verify_inner_sig`.

Nothing ties that transport identity to a Joined member. The two identity notions are
**independent keys from independent stores**, confirmed in source:

| notion | key / hash | where it lives | preimage |
|---|---|---|---|
| **transport identity** | `harmony_identity::Identity` (X25519‖Ed25519); its `address_hash` | node-identity **file** (`identity::load_or_generate`, `lib.rs:4154`) | `SHA256(x25519 ‖ ed25519)[:16]` (composite, includes encryption key) |
| **community device key** | `device_signing_key` (Ed25519); enrolled via `EnrollmentCert` | OS **keychain** (`owner_state.rs:457` `KEYCHAIN_DEVICE_SK`) | (device `PubKeyBundle::identity_hash` = `SHA256(CBOR{ed25519, ml_dsa})[:16]`, signing-only) |
| **master owner identity** | `PubKeyBundle::identity_hash()` = `OwnerAddr` | keychain (master seed) | `SHA256(CBOR{ed25519, ml_dsa})[:16]`, signing-only |

The transport Ed25519 (`harmony_identity_pub[32..64]`) and the enrolled community-device
Ed25519 are **different keys** — the naive check `harmony_identity_pub[32..64] ∈
MemberState.enrolled_device_keys` cannot pass. (The ZEB-824 §5c gate failed for a related
reason: it compared the transport *composite address-hash* against *master* identity-hashes,
two truncated-SHA256 notions with disjoint preimages.)

### What a resolving member holds offline (the verification anchor)

A member on a rebuilt node (ZEB-824's target case: has persisted `crdt.cbor`, no addrbook /
session) holds, purely from replicated + persisted state:

- **Materialized membership** — `MaterializedMembership.members: BTreeMap<OwnerAddr,
  MemberState>` (`community_membership.rs:1815`), keyed by **master** `OwnerAddr`. Each
  `MemberState` carries `enrolled_device_keys: BTreeSet<[u8;32]>` (Ed25519 device **verify**
  keys) and `revoked_device_keys: BTreeSet<[u8;32]>` (remove-wins tombstones). These keys are
  extracted **only from validated `EnrollmentCert`s** on each member's `Join`.
- The full `EnrollmentCert` for every co-member (on the persisted `SignedMembershipEvent`s in
  the `VerifiedLog`).

So the resolver can decide, offline, "is Ed25519 key `D` an **effective enrolled device** of
some Joined member?" — `D ∈ (member.enrolled_device_keys \ member.revoked_device_keys)` for
some member with `MemberStatus::Joined`. This is the whole membership predicate, and it needs
no `owner_device_cache` (which is populated by peer interactions and is **empty for a
never-connected member** — verified: no `owner_device_cache` write exists on any membership
path).

---

## 2. The binding (Approach A)

Introduce a **membership vouch**: the beacon's own community device key `D` signs a statement
binding its transport identity `T`. The resolver checks the vouch, then checks that `D` is an
effective enrolled device of a Joined member. Because `D`'s **private** key is required to
produce the vouch, an attacker holding only the epoch key cannot forge it.

### 2.1 Vouch structure

A client-side type (no `harmony-pkarr` / `harmony` change):

```rust
/// Proof that the beacon's transport identity belongs to a Joined member,
/// signed by the beacon's enrolled community device key.
struct MembershipVouch {
    version: u8,                 // MEMBERSHIP_VOUCH_V1 = 1
    device_vk: [u8; 32],        // D: the beacon's enrolled Ed25519 device verify key
    issued_at_ms: u64,
    valid_until_ms: u64,
    sig: [u8; 64],              // Ed25519 over the canonical signed tuple below, by D
}
```

The signed preimage is a **domain-separated** canonical-CBOR tuple:

```
sig = Ed25519_sign(
    D_sk,
    domain_tag = b"harmony.rendezvous.membership-vouch.v1"
    ‖ canonical_cbor([ community_id: bstr(16),
                       transport_identity_pub: bstr(64),   // == record.harmony_identity_pub
                       issued_at_ms: u64,
                       valid_until_ms: u64 ]) )
```

- **Domain tag** prevents cross-protocol reuse of `device_signing_key` signatures (the key
  also signs relay/enrollment artifacts).
- Binding **`community_id`** scopes the vouch to this community (a vouch minted for community
  X is not replayable as a beacon in community Y).
- Binding **`transport_identity_pub`** ties the vouch to exactly this beacon's transport
  identity — the resolver rejects a vouch whose bound `T` ≠ the record's `harmony_identity_pub`.
- **`issued_at_ms` / `valid_until_ms`** give the vouch its own freshness window,
  independent of (and additional to) the pkarr record's TTL.

### 2.2 Wire carriage

The vouch rides in the **rendezvous slot's routing blob**, which is client-controlled on both
publish and resolve and is already covered by the pkarr **inner** signature (the blob is the
first element of the inner-signed tuple) and the outer BEP44 signature. No pkarr record field
and no signed-tuple change.

The rendezvous routing blob becomes a **superset** of the existing
`ReachabilityAnnouncePayload`, carrying an additional `vouch` field, such that:

- **(a)** it is covered by the existing pkarr inner-sig unchanged;
- **(b)** legacy bare-`ReachabilityAnnouncePayload` decoders still extract reachability (so an
  *old* resolver keeps dialing a *new* beacon during rollout — graceful degradation on the
  read side).

Preferred mechanism: an additive `#[serde(flatten)]`-embedded reachability payload plus a
`vouch` key on the same CBOR map, so a bare decode ignores the unknown `vouch` key. **Task 1
must pin this with a round-trip test and a legacy-decode-compat test**, because ciborium's
`flatten` has known edge cases; if it proves fragile, the fallback is a versioned nested
wrapper (`{ an: ReachabilityAnnouncePayload, vc: MembershipVouch }`) with **both** client
rendezvous decoders (member-dial and open-join) updated to unwrap it. Either way this is a
client-only change to the rendezvous blob format.

**Scope guard:** only the rendezvous publisher emits the vouch. The `blob_builder` closure is
shared across the Case-C member-keyed and Case-D friend publishers (`lib.rs:9635-9664`);
those must **not** gain the vouch (it is meaningless there and would bloat their records). The
rendezvous publisher gets its own vouch-carrying blob path.

### 2.3 Resolver verification (strict)

In the member gateway dial path (`IdentifiedSlotResolver` /
`community_gateway_dial_driver`), for each candidate slot, after the existing self-filter and
pkarr inner-sig/freshness/anti-rollback checks:

1. Decode the routing blob; extract `reachability` and `vouch`. **No `vouch` ⇒ reject**
   (strict) — the slot reads as unusable; the escalating batch driver falls through to other
   slots exactly as it does for a self-owned slot.
2. `vouch.version == MEMBERSHIP_VOUCH_V1` else reject.
3. `vouch`'s bound `transport_identity_pub == record.harmony_identity_pub` else reject
   (the vouch must be for *this* record's transport identity).
4. `issued_at_ms ≤ now ≤ valid_until_ms` else reject (vouch freshness).
5. Verify `vouch.sig` under `vouch.device_vk` over the domain-tagged tuple else reject.
6. **Membership check:** `vouch.device_vk ∈ (M.enrolled_device_keys \ M.revoked_device_keys)`
   for some member `M` with `MemberStatus::Joined` in this community's materialized
   membership (via the driver's existing `CommunitySyncRegistry` engine access). No such
   member ⇒ reject.

Only a slot passing all six is seeded into the `ReachabilityResolver` and kicked. A rejection
is a per-slot decision, not a hard error — the driver records the outcome and ladders/falls
through.

---

## 3. Security analysis

Threat model (inherited from ZEB-824 §5c): the adversary **holds the community epoch key**
(a backward-secrecy breach) but is **not** a Joined member and does not hold any member's
device private key. The goal is to prevent such an adversary from steering a member's dial to
an endpoint of the adversary's choosing.

- **Forgery.** Producing a valid vouch requires signing with a member's device **private**
  key `D_sk`. The epoch key does not yield `D_sk`. The adversary cannot mint a vouch for a
  device key that is in `enrolled_device_keys`. ✔
- **Transport swap.** The vouch binds `transport_identity_pub`; the resolver rejects a vouch
  whose bound `T` ≠ the record's `harmony_identity_pub`. The adversary cannot lift a real
  member's vouch onto a record advertising the adversary's transport identity. ✔
- **Replay of a real beacon.** The adversary can copy a member's real record+vouch verbatim,
  but it points at that member's real endpoint, not the adversary's — the dial goes to the
  legitimate member. Harmless. Freshness (vouch window + pkarr TTL + anti-rollback) bounds
  staleness. ✔
- **Revoked device.** A device revoked in membership is excluded by step 6's `\
  revoked_device_keys`, so a stolen-but-revoked device key cannot vouch. ✔
- **Residual — malicious member.** A *Joined* member (holder of a valid `D_sk`) can sign a
  vouch over an **arbitrary** transport identity, steering a dial to an attacker endpoint.
  This is out of scope: a malicious admitted member is a strictly larger problem, and
  everything post-dial stays gated by per-row / per-event verification (addrbook rows and
  membership events are signature- and enrollment-checked on ingest), so the malicious member
  gains a session attempt, not authority over state. Documented, not closed.

Net: the binding removes exactly the ZEB-824 §5c widening — an **epoch-key holder who is not
a member** can no longer steer a member's dial.

---

## 4. Strict policy and rollout (flag-day)

Strict enforcement (§2.3 step 1) means a beacon without a valid vouch is not dialed. Because
every node is both a beacon and a resolver, rollout has an ordering property:

| resolver \ beacon | old beacon (no vouch) | new beacon (vouch) |
|---|---|---|
| **old resolver** (epoch-envelope) | dials (today's behavior) | dials — bare decode ignores `vouch` (§2.2b) |
| **new resolver** (strict) | **rejects** (no vouch) → falls through | verifies → dials |

So a new strict resolver ignores old beacons; a new beacon is still dialable by old
resolvers. The only degraded window is a strict resolver whose community offers **only**
unproven beacons — it cannot bootstrap via rendezvous until at least one beacon upgrades.

Mitigations / why now is the right time:

- The fleet is small and operator-controlled; upgrade beacons and resolvers together.
- Volunteer / third-party relays — the case that broadens the epoch-key-holder set and makes
  this gap dangerous — have **not** shipped yet, so the transitional cost is near-zero now and
  rises later (the ticket's own "raises when volunteer relays ship" framing).
- The **LAN-scouting fallback** (`HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1`, ZEB-824 §9) remains
  the operational last resort, unchanged.

This is a deliberate flag-day on the rendezvous **admission** decision, consistent with the
ZEB-815 flag-day posture; it does not require a dual-write window.

---

## 5. Publisher changes

`CommunityRendezvousPublisher` (`community_rendezvous_publisher.rs`) today holds only the
transport keypair (`identity_signing_key`, `identity_pub`) and the shared `routing_blob_builder`.
It must additionally receive, at construction (`lib.rs:9658` site):

- the node's community **`device_signing_key: Arc<ed25519_dalek::SigningKey>`** (available at
  boot as `owner_loaded.device_signing_key`), and
- the **`community_id`** for each slot it refreshes (already available to `refresh_slot`'s
  caller as `community_id`).

In `refresh_slot`, before signing the record, the publisher mints a fresh `MembershipVouch`
(one Ed25519 sign per ~7.5-min refresh, negligible) over `(community_id, self.identity_pub,
now, now + REACHABILITY_RECORD_TTL_MS)` and builds the vouch-carrying rendezvous blob (its own
blob path, not the shared `blob_builder` — §2.2 scope guard). The record is then signed by the
transport key exactly as today (`PkarrRoutingRecord::sign_new`), so the inner-sig now covers
the vouch transitively.

A beacon is by construction a Joined member that is a relay advertiser (ZEB-824 Terminology),
so it always has an enrolled `device_signing_key`; there is no "beacon without a device key"
case.

---

## 6. Resolver changes

- **`community_rendezvous.rs`** — the identified rendezvous decode (`IdentifiedSlotResolver` /
  `resolve_rendezvous_identified`) extracts `(reachability, vouch)` and returns them alongside
  `beacon_identity_pub`. The open-join (unidentified) decode continues to extract only
  `reachability` (open-join dials a beacon without a membership check — a joiner has no
  membership to check against — so it stays epoch-envelope; only the *format* unwrap, if the
  fallback nested wrapper is used, is shared).
- **`community_gateway_dial_driver.rs`** — the §2.3 six-step check runs against the driver's
  existing per-community engine (`CommunitySyncRegistry`) materialized membership. A slot that
  fails any step is not seeded; the pass records the outcome and advances the ladder.

---

## 7. Telemetry

Reuse the ZEB-824 `GatewayBootstrapTelemetry` vocabulary. Under strict enforcement the
existing **`rejectedNonMember`** outcome regains its literal meaning: a beacon that verified
under the epoch key but carried **no valid membership vouch** (missing, malformed, stale,
bad signature, or a device key not in any Joined member's effective enrolled set). Today that
string fires only for undecodable identity bytes; this design makes it the primary
membership-rejection signal. Emit a DEBUG log on rejection carrying the specific sub-reason
(no-vouch / bad-sig / unknown-device / stale) so a fleet node is diagnosable without a live
session (the ZEB-804 lesson); the aggregate count surfaces in `network_health_snapshot`'s
`gatewayBootstrap` block as before.

---

## 8. Testing

Unit (client, mock membership + injected clock — the ZEB-824 driver seams already exist):

- **Vouch round-trip:** mint under a device key, verify; tamper each field (device_vk,
  transport_identity_pub, community_id, window, sig) ⇒ verify fails.
- **Wire carriage:** publisher-emitted rendezvous blob decodes to `(reachability, vouch)`;
  **and** a legacy bare-`ReachabilityAnnouncePayload` decode of the same bytes still yields
  reachability (§2.2b compat) — this test pins the flatten-vs-wrapper choice.
- **Resolver strict path:** (i) beacon with a valid vouch whose `device_vk` is a Joined
  member's effective enrolled key ⇒ **seeded**; (ii) **no vouch** ⇒ rejected,
  `rejectedNonMember`; (iii) vouch for a *different* transport identity than the record ⇒
  rejected; (iv) vouch signed by a key **not** in any member's enrolled set ⇒ rejected;
  (v) vouch signed by a **revoked** device key ⇒ rejected; (vi) stale vouch window ⇒ rejected;
  (vii) valid vouch for a member the resolver does **not** know (never synced their Join) ⇒
  rejected + falls through to another slot.
- **Batch escalation:** an unproven slot at index 0 does not block a proven slot at a later
  index (reuse the ZEB-824 mock-relay publish helper).

Integration (in-process, mock pkarr relay): a member node with empty addrbook + scouting off,
against one rendezvous record published **with** a valid vouch under the community epoch key,
seeds the resolver in one `run_one_pass`; the same record published **without** a vouch does
**not** seed and records `rejectedNonMember`.

Live verification (post-merge, not CI): fleet cross-machine heal with a fully-upgraded fleet,
confirming proven beacons dial and telemetry shows zero `rejectedNonMember` among co-members.

Gates: `cargo nextest run --locked --features test-fixtures -E
'test(community_gateway_dial_driver) | test(community_rendezvous) | test(membership_vouch)'`
(plus touched-module suites), `cargo fmt --all -- --check`, CI-exact
`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, and
a final full `--workspace --all-targets` sweep.

---

## 9. Boundaries (deliberately out of scope)

- **Malicious-member dial steering** (§3 residual) — a Joined member vouching an attacker
  endpoint. Bounded by post-dial per-row/per-event verification; its own concern.
- **New member the resolver has never synced** — its beacon is unverifiable offline (the
  resolver doesn't yet know its master is a member). Inherent chicken-and-egg; handled by
  falling through to a known beacon and, in the worst case, the LAN-scouting fallback.
- **Open-join admission** — unchanged (epoch-envelope). A joiner is not yet a member and has
  no membership to prove; the binding is a member↔member property.
- **Cross-repo** — none. The proof lives in the client-controlled blob; `harmony-pkarr`'s
  record and signed tuple, and `harmony-reachability`'s `ReachabilityAnnouncePayload`, are
  untouched.
- **Rendezvous publisher epoch-key wart** (ZEB-824 §9) — untouched; orthogonal.
