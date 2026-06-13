# ZEB-424 — SP2 P3b: Group-DM butler (fan-out deposits + co-membership admission)

**Status:** draft for review (design settled with Jake 2026-06-12; one wire-level
refinement vs. the approved sketch, called out in D28).
**Parent:** ZEB-418 SP2 Butler. Spec lineage: SP2 umbrella
(`2026-06-09-zeb-418-sp2-butler-design.md`, §4 as amended) → P2 D11–D18 →
P3a D19–D26 → **this doc D27–D34**.
**Branch:** `zeb-424-group-dm-butler`.

## Goal

A group-DM message to an offline co-member gets deposited with that member's
butler (always-on device) exactly like a 1:1 DM does today — even when the
sender and the recipient are **not direct friends**, which group membership
does not require.

## Context (what already works, verified on main 2026-06-12)

The P1/P2 machinery is space-agnostic almost everywhere:

- `send_dm` already fans out for `SpaceKind::GroupDm`: one `OutboxEntry` with
  `recipient_owners` (all members except self, sorted), one shared encryption
  + `message_cid` (`dm_outbox.rs:574–715`, `derive_recipients` at 2278).
- Candidacy is already per-recipient: `AttemptState` keyed
  `(entry_id, recipient)`, `DEPOSIT_NOACK_WINDOWS = 2`, `DeliveryStatus::
  {Pending, Partial, Complete, Expired}` (`dm_outbox.rs:295–304`,
  `drain_phase_c` 1040–1234).
- `ButlerDepositRequest` already carries `space_id` + `message_cid` on the
  sender side (`butler_deposit.rs:273–283`).
- The P2 outhold keys `{space_id_hex}:{message_cid_hex}` — content-addressed,
  so an N-recipient fan-out is **one** outhold row (`dm_outhold.rs:33–50`).
- Butler-set resolution is already per-recipient (reachability cache →
  pkarr fallback, freshness window 15 min; `butler_deposit.rs:468–530`).

**The one real gap:** the P1 deposit acceptor admits only friend-`Active`
senders (`iroh_butler_acceptor.rs:398–408`, spec §4 step 1). A group-DM
co-member who is not a friend is rejected before decrypt, so their deposits
can never land.

## Decisions

### D27 — Admission = local-knowledge co-membership check (no proof primitive)

The butler is one of the recipient's own enrolled devices and already holds
the owner's replicated owner-state CRDT (`ProdButlerDepositCtx.crdt_state`,
the same state step 1 reads `friend_graph` from). Group-DM admission is a
second arm of step 1, read from that local state under the same lock:

```text
step 1 (admission), extended:
  1a. friend_graph[sender_owner] == Active            → admit  (today's path)
  1b. else if ∃ space ∈ owner_state.spaces:
        space.kind == GroupDm
        ∧ space.left_at.is_none()
        ∧ sender_owner ∈ space.members                → admit
      (self_owner ∈ members holds by construction —
       a space in OUR owner-state that we haven't left is ours)
  else                                                → uniform reject
```

No new signatures, no new trust roots, no presented proof. P4 (sealed relay)
sits **outside** the fleet, cannot reuse a local-knowledge check, and designs
its own presented-proof primitive when it comes up; only the admission-step
shape is shared.

Rejects stay uniform on the wire (spec §4: the stream just closes — no
oracle distinguishing "not a friend" from "not a co-member").

### D28 — No wire-frame change: admission checks ANY shared GroupDm (refinement vs. the approved sketch)

The design sketch approved on 2026-06-12 showed `space_id` added to the
deposit frame and an exact-space admission check. Recon against main forces
a refinement:

1. `DepositFrame` is canonical-CBOR with **strict decode** (unknown/trailing
   data rejected) and a byte-pinned wire fixture
   (`butler_deposit.rs:107–128`, fixture at 598–606). Adding a field is a
   flag-day between fleet devices on different builds — and the
   just-shipped pinned-coordination-instance setup (ZEB-446) makes
   mixed-version fleets the *expected* steady state, not an edge case.
2. `space_id` already travels **inside the sealed payload**: the inner
   `DmCidNotifySigned` binds it, and post-decrypt verification + ingestion
   reuse the normal DM path, which validates the actual space claim.

So admission checks *"does the sender share **any** live GroupDm space with
my owner?"* pre-decrypt, with zero wire change. The trust class is right for
what step 1 is for — an anti-spam/anti-DoS storage gate in front of decrypt:

- A co-member who lies about which space a blob belongs to gets caught at
  inner verify / ingest (the normal path), exactly like any malformed
  deposit.
- Storage abuse by an admitted-but-malicious co-member is bounded by the
  existing caps regardless of admission flavor
  (`INBOX_PER_SENDER_CAP = 64`, `INBOX_GLOBAL_CAP = 1024`,
  `DEPOSIT_MAX_FRAME_BYTES = 256 KiB`).

A **frame-level** (pre-decrypt) exact-space check buys nothing those layers
don't already provide, and costs a wire-format migration. If P4's
presented-proof work later wants space-scoped *frames*, it versions the ALPN
(`harmony/butler-deposit/v2`) there.

> **D28.1 amendment (security follow-up, post-review):** the paragraph above
> is right that a *pre-decrypt, frame-level* exact-space check would cost a
> wire migration — but it conflated that with exact-space binding in general.
> The `space_id` is available **post-decrypt** (it travels inside the sealed
> payload), so an exact-space bind for the co-member path is achievable with
> **zero wire change**. The "shares any" gate alone lets an admitted co-member
> of group A deposit a packet whose inner `space_id` names an unrelated space
> B (which they are not in): inner-verify still passes (they legitimately
> signed it), so it is **persisted + acked**, then ingestion — which re-checks
> space membership — rejects it forever, leaving an un-ingestible entry that
> pins an inbox slot for the full `INBOX_TTL_MS` (30 days). The per-sender cap
> bounds one sender, but "anyone you share any group with" is a far wider
> trust set than friends, and the un-ingestible entries are *stickier* than
> legitimate traffic (never GC'd on ingest). So the co-member path now adds a
> **post-decrypt** bind (see D28.1 below) — the originally-approved sketch's
> exact-space intent, for free. See D28.1.

### D28.1 — Post-decrypt co-member space bind (no wire change)

After the inner CidNotify is decrypted and verified (its signing device bound
to `frame.sender_owner`), and **before** persist/ack, the **co-member** path
requires that the deposit's own `signed.space_id` is a live `GroupDm` in the
butler's replicated `OwnerState.spaces` whose members contain BOTH this owner
and the sender — the same check ingestion (`verify_cidnotify_admission`)
applies, moved earlier so the un-ingestible entry is never written.

- New ctx method `space_live_group_dm_co_member(space_id, sender_owner) ->
  bool`, prod-implemented via `space_is_live_group_dm_co_member_in` (the
  exact-space sibling of `shares_live_group_dm_in`, same single lock).
- The pre-decrypt "shares any" gate stays as the cheap stranger-filter in
  front of decrypt; the post-decrypt bind is the authoritative one. A failing
  bind rejects `NotAuthorized` (the same uniform, no-oracle close).
- **The friend path is intentionally NOT space-bound here.** Friendship is the
  authorization for 1:1 DM deposits and is unchanged by ZEB-424; a latent
  "malicious friend deposits for an unrelated space" parity question exists but
  is pre-existing, out of this PR's blast radius, and left for a follow-up if
  desired (friends are a small curated trusted set).

### D29 — Acceptor surface: one new ctx method, one new (local-only) reject variant

- `ButlerDepositCtx` gains
  `async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool`.
  `ProdButlerDepositCtx` implements it with a linear scan of
  `owner_state.spaces` under the **same single lock acquisition** pattern
  step 1 uses today (spaces count is small — tens, not thousands; no index
  needed, and a derived index would add CRDT-merge invalidation hazards for
  zero measured win).
- `DepositReject` gains `NotAuthorized` (collapsing "not a friend AND not a
  co-member"). The enum never crosses the wire (rejects close the stream
  uniformly; it exists for counters/tests), so this is not a format change.
  `NotFriend` is **renamed/absorbed** into `NotAuthorized` since the gate is
  now "friend OR co-member" — a deposit that fails both is rejected for the
  compound reason. Counter continuity note goes in the changelog comment.

### D29.1 — Cert trust anchor for co-members: derive the master from the owner id (load-bearing; not in the approved sketch)

Grounding the plan surfaced a gap the sketch glossed: step 2 (the
`EnrollmentCert` master-pin) currently binds the cert's issuing master to
the **friend-graph-pinned master** (`cert_master == friend_master`). A
non-friend co-member has no pinned master, so that check can't run for
them — yet the cert must still be anchored to *something*, or admission
would persist a blob behind a cert with an attacker-chosen master.

The anchor already exists and needs no new state: **the owner id is the
hash of the master key bundle**.
`friend_graph::owner_id_from_master_ed25519(master) -> OwnerAddr` is the
derivation, and `iroh_friend_acceptor`'s own test
(`master_ed25519_from_cert_matches_owner_id`) pins the invariant
`owner_id_from_master_ed25519(cert_master) == cert.owner_id`. So a
Master-issued cert for `sender_owner` is cryptographically valid for that
owner iff `owner_id_from_master_ed25519(cert_master) == sender_owner` —
collision-resistance of the identity hash makes a forged-master cert for a
victim's owner id infeasible.

Decision: keep the **friend path byte-for-byte unchanged** (it still pins
`cert_master == friend_master`, which — because the pinned master was
itself validated by this same derivation at friend-time — is equivalent to
but stricter-by-defense-in-depth than the derived check). Add a **parallel
co-member branch** that does the derived check
`owner_id_from_master_ed25519(cert_master) == frame.sender_owner`. The
admission step therefore yields a small two-variant verdict —
`Friend(friend_master)` vs `CoMember` — that step 2 consumes to pick the
master-binding rule. Everything downstream (frame sig, decrypt, inner
CidNotify verify against `owner_device_cache`, atomic persist) is identical
for both. This keeps the security-critical friend path untouched (no
re-review needed there) and makes the co-member path purely additive.

A non-Active **friend** who is *also* a live co-member (e.g. a revoked
friend still in a shared group) falls through to the co-member branch and
is admitted on group membership via the derived anchor — group membership
is independent of friend status, and the stale revoked-friend master is
deliberately not consulted.

**Defense-in-depth, confirmed during implementation:** `EnrollmentCert::
verify()` (run in step 2 *before* the master binding) already rejects
`hash(master) != owner_id` (the `EnrollmentCertInvalid` taxonomy in
`community_membership.rs`). Since step 2 also requires `cert.owner_id ==
sender_owner`, any cert reaching the binding match necessarily satisfies
the derived check — so the co-member branch (like the long-standing friend
pin) is belt-and-suspenders, not a live rejection path for a well-formed
cert. We keep both explicit pins anyway: they make each path's trust anchor
self-evident and stay correct if `verify()`'s internal owner_id↔master
binding is ever refactored away. This is also why a forged-master rejection
is **not unit-tested** (D34) — such a cert cannot pass `verify()` via the
real cert-minting helpers, so the test would be unreachable; the friend-pin
regression test + integration coverage exercise the binding instead. The
security guarantee for co-member admission rests on exactly this: a valid
Master cert for `sender_owner` can only be minted by `sender_owner`'s own
master.

### D30 — Churn semantics: eventual consistency, both windows accepted

Membership changes propagate to the butler via normal owner-state CRDT sync
(`harmony/owner-state/v1` fleet dataset). Two transient windows exist and
are **accepted**:

- **Stale-admit:** sender was removed from the group, butler's replica
  hasn't synced → deposit admitted. Harmless beyond bounded storage: the
  blob is sealed to the recipient, ingest validates against current state,
  and caps bound the volume. Window closes at next CRDT sync.
- **False-reject:** sender was just added, butler hasn't synced → deposit
  rejected. Self-healing: the sender's existing backoff/retry machinery
  (BASE 5 s → CAP 5 min, expiration 30 days) retries long past any
  realistic sync lag.

No new mitigation machinery. This matches the SP2 umbrella's posture that
the butler is an availability optimization, not a correctness layer.

### D31 — Per-recipient mixed-state semantics: confirm, don't change

For an N-recipient entry with mixed states (some acked, some deposited, some
pending), the intended behavior is what P2 already encodes — P3b's job is to
**prove** it with tests, not to change it:

- Candidacy fires independently per `(entry_id, recipient)` (no-ack ≥ 2
  windows, or transient-error + ≥ 1 failure).
- A deposit to recipient R's butler counts as *deposited* for R only; direct
  acks from other recipients neither suppress nor trigger R's candidacy.
- `DeliveryStatus::Partial`持continues to reflect direct acks only; deposits
  do not flip an entry to `Complete` (the butler is a relay, not the
  recipient).
- The outhold row (one per `{space_id}:{message_cid}`) is GC'd on terminal
  status (`Complete`/`Expired`) and orphan-graced 10 min — unchanged; the
  single-row-for-N property is exactly why no per-recipient outhold work is
  needed.

Any deviation discovered while writing these tests is a finding to fix in
its own right (and possibly its own ticket), not silently absorb.

### D32 — Butler-set resolution at fan-out scale: bounded, existing machinery

Worst case is 15 recipient lookups per entry (GroupDm caps at 16 members
incl. self — `owner_state_types.rs:1736–1759`; note the ticket said "2–15",
the code says **3–16**, code wins). Resolution already goes through the
reachability cache with pkarr fallback per recipient; lookups happen only
for recipients that reach deposit candidacy (offline peers), not all N.
No batching/parallelization work in P3b — if live testing shows pkarr-miss
storms, that's a measured follow-up, not speculative machinery now.

### D33 — Caps and quotas: unchanged

Per-sender (64) and global (1024) inbox caps, 256 KiB frame cap, outhold
16 MiB dataset cap all stay. A group of 15 co-members can collectively hold
more inbox slots than one friend could, which is proportionate to the trust
the owner expressed by joining the group. Revisit only with evidence.

### D34 — Test plan

Unit (acceptor, mock ctx — extend `iroh_butler_acceptor.rs` tests):
- co-member + not-friend → admitted (the P3b headline case);
- friend-Active + not-co-member → still admitted (1a regression);
- neither → `NotAuthorized`, and the reject path stays pre-decrypt
  (extend the existing call-order probe so `shares_live_group_dm` is
  consulted only after `lookup_friend` misses, and `decrypt` is never
  reached on reject);
- co-member of a **left** group (`left_at = Some`) → rejected;
- co-member of a `Dm`/`Channel`/`Community` space only → rejected
  (kind gate);
- **post-decrypt space bind (D28.1):** co-member shares SOME live group DM
  (step 1 admits) but the deposit's own `signed.space_id` is NOT a live
  GroupDm with both members → `NotAuthorized` **after** decrypt, **before**
  persist/ack (call-order probe: `decrypt` ran, no `persist:` event); plus a
  direct `space_is_live_group_dm_co_member_in` helper test that the bind
  matches ONLY the named space (rejects a different space the sender shares);
- **co-member cert anchor (D29.1):** *intentionally not unit-tested* — the
  derived check is unreachable as a rejection because `cert.verify()` already
  binds `hash(master) == owner_id` (see D29.1 "defense-in-depth"); a forged
  cert can't pass `verify()` via the real minting helpers. The binding is
  covered by the friend-pin regression + the integration path instead;
- **friend path unchanged:** a friend-Active sender whose cert master
  mismatches the *pinned* `friend_master` → `BadCert` (regression that the
  friend branch still pins, not the derived check).

Unit (outbox, `dm_outbox.rs` tests):
- N-recipient entry, mixed states: A acked, B deposit-candidate, C pending
  → only B gets a `ButlerDepositRequest`; entry stays `Partial`;
- deposit outcome for B does not mutate A/C `AttemptState`.

Integration (extend `tests/butler_deposit_integration.rs`):
- end-to-end group-DM deposit: non-friend co-member sender → butler admits,
  persists, acks; recipient ingest delivers (reuses the existing two-engine
  harness with a GroupDm space seeded in the butler's owner-state);
- non-member sender → stream closes, nothing persisted;
- **co-member into an unrelated space (D28.1):** co-member of some group
  deposits a packet whose inner `space_id` is one they are not a member of →
  `NotAuthorized`, nothing persisted (the post-decrypt bind closes the
  un-ingestible-entry inbox-slot-pinning vector).

Wire pin: `deposit_frame_wire_fixture_pinned` must pass **unchanged** — the
no-wire-change property of D28 is itself an assert.

## Non-goals

- **P4 sealed relay** and any presented-proof primitive (D27/D28 rationale).
- **Community-membership admission.** Channel traffic syncs via P3a
  backfill, not DM deposits; "co-community" admission has no consumer today.
- **1:1 `Dm` spaces via co-membership.** A `Dm` space implies a friend edge;
  if the friendship is revoked, deposits stopping is current, intended
  behavior.
- **Butler-set lookup batching / pkarr parallelization** (D32).
- **Group-DM *inbox* capacity rebalancing** (D33).

## Rollout

Single PR on `zeb-424-group-dm-butler`: acceptor extension + tests. No
migration, no flag — admission widens monotonically, old senders are
unaffected, and a butler on an older build simply keeps rejecting non-friend
co-members until updated (the sender's retry machinery already tolerates
that, per D30's false-reject analysis).
