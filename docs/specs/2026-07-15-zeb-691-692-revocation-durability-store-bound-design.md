# ZEB-685 tail — RevocationPush durability rung + bounded revoked-device store

**Tickets:** ZEB-691 (durability rung for `RevocationPush` delivery to offline
friends) + ZEB-692 (bound the grow-only `revoked_dm_devices` store). Both are
ZEB-580 (S3) children, following the DM-only device-revocation cutoff merged in
zeblithic/harmony-client#471.

**Goal:** close the two residuals from #471 — (691) make a device revocation
reach an *offline* DM-only friend automatically on reconnect, and (692) bound
the friend-scoped `revoked_dm_devices` CRDT so it cannot grow without limit.

**Ship shape (decided):** ONE bundled PR ("ZEB-685 tail"). ZEB-691 rides the
**butler hold rung** (`DmInboxDoc` — the friend's own always-on fleet, no
community gate). The community-relay rung is out of scope (it only reaches
shared-community friends, already covered by `DeviceRetire` — see §Part B rung
choice + §Out of scope).

---

## Global Constraints

- **MSRV 1.91 / toolchain 1.94.1.** `BTreeSet::pop_last` (stable 1.66) is
  available and is the deterministic "drop largest" primitive for the cap.
- **`revoked_dm_devices` is union-merged, NOT LWW** (`owner_state_sync::
  merge_remote_into_local`, line ~370). Any bound on it MUST be a *convergent
  function of the merged set*, never a one-shot delete — a plain `remove` on one
  device is re-inflated by the next sync from a sibling that still holds the
  entry. This is the load-bearing invariant for ZEB-692.
- **`DepositPayload` / `DmInboxEntry` extensions are additive `Option<Vec<u8>>`
  fields** with `#[serde(default, skip_serializing_if = "Option::is_none", with =
  "serde_bytes")]` — the established backward-compatible pattern (ZEB-483
  `invite_packet`, ZEB-505 invite-only). New field follows it exactly;
  pre-existing butlers and older snapshots ignore the absent key.
- **Security is re-verified on the receive side regardless of transport.**
  `handle_revocation_push` (dm_outbox.rs) verifies the Master cert
  (`verify(None)`), trust-binds `revocation.owner_id == enrollment.owner_id ==
  expected_owner`, and binds `enrollment.device_id == revocation.target`. The
  butler carries a sealed blob it pre-validates but is not the recipient of; the
  durability rung MUST NOT weaken this — the butler pre-validates with the same
  extracted `verify_revocation_push` core, and the recover path feeds the full
  `handle_revocation_push`.
- **`notify_dirty` on a genuine local insert.** A `RevocationPush` applied on the
  receive side must call the owner-state `SyncEngine`'s `notify_dirty()` iff it
  was a fresh insert, or it is neither persisted nor replicated (the #471
  durability fix). The recover path inherits this contract.
- Gates: `cargo fmt --all --check`, `cargo clippy --all-targets --features
  test-fixtures -D warnings`, `cargo nextest run --all-targets --features
  test-fixtures`, MSRV `cargo check`, `tsc --noEmit` + vitest (no frontend change
  expected here).

---

## Part A — ZEB-692: bound `revoked_dm_devices`

### Problem

`OwnerState::revoked_dm_devices: BTreeMap<OwnerAddr, BTreeSet<[u8;32]>>`
(owner_state_crdt.rs:84) is grow-only. A friend holds their **own** master key,
so they can mint + revoke arbitrarily many synthetic devices and push each as a
valid `RevocationPush`; each accepted key unions into `revoked_dm_devices[friend]`
forever, is persisted (`owner_state_crdt.cbor`), and replicated to your sibling
devices. Bounded to established friends (the trust-bind rejects third parties),
so it is a *friend-abuses-you resource* issue, not a stranger DoS, and not a
cutoff bypass — purely unbounded storage/replication growth. Hence Low priority.

### Design — two convergent bounds

Both bounds are deterministic functions of the merged set/state, so every device
independently reaches the same result and they survive union-merge.

**1. Per-owner cap.** New const
`MAX_REVOKED_DM_DEVICES_PER_OWNER: usize = 256` (a real fleet is single-digit
devices; 256 is a generous DoS backstop). Enforced by keeping the **smallest-N
by byte order** — `BTreeSet` is ordered, so "keep N smallest" (`pop_last` to
evict the greatest until `len <= N`) is a deterministic set→set function, hence
convergent under union.

Applied at BOTH mutation sites:
- `owner_state_crdt::apply_revoked_dm_device` (owner_state_crdt.rs:905) —
  after `insert`, evict down to the cap. Return semantics: `bool` still means
  "a new, *retained* key was added" (drives `notify_dirty`). If the inserted key
  is itself evicted by the cap (it was larger than N existing keys), return
  `false` — no net change to persist.
- `owner_state_sync::merge_remote_into_local` (owner_state_sync.rs:370-376) —
  after the union `extend`, evict each touched owner's set down to the cap.

**2. GC-on-de-friend** (the real-world growth vector: stale entries for people
you've unfriended). Done convergently via the friend-status, not a bare delete:
- `unfriend_inner` (lib.rs:52972) — right after applying the `Revoked`
  tombstone (lib.rs:53016), `state.revoked_dm_devices.remove(&peer_addr)` for the
  immediate local free (we already hold the lock).
- `merge_remote_into_local` — `friend_graph` is merged (line ~353) BEFORE the
  `revoked_dm_devices` union (line ~370). After the union, **prune any owner
  whose merged friend status is `Revoked`**. Because `friend_graph` converges
  (LWW; `Revoked` is a permanent tombstone that a stale `Active` cannot resurrect
  without a strictly-newer HLC), the status-keyed prune converges: once the
  `Revoked` tombstone reaches every device, each independently drops that owner's
  set and it stays dropped. Prune condition = **present-and-`Revoked`** only
  (leave absent-owner entries alone — defensive; the cap bounds them and an
  unfriended contact is always present-as-tombstone, never absent).

**Ordering in `merge_remote_into_local`:** friend_graph merge (existing, ~353)
→ revoked_dm_devices union (existing, ~370) → **cap each touched set** → **prune
present-and-Revoked owners** (new, after 376).

### Seams (Part A)

- `src-tauri/src/owner_state_crdt.rs` — const; `apply_revoked_dm_device` cap.
- `src-tauri/src/owner_state_sync.rs` — cap + prune after the union loop.
- `src-tauri/src/lib.rs` — `unfriend_inner` local GC.
- No wire/persist change — `revoked_dm_devices` already threads through
  `CrdtFileV2` + both `From` impls (owner_state_persist.rs:129/144/161).

### Tests (Part A)

- `apply_revoked_dm_device` caps at N; inserting past N evicts the greatest and
  returns `false` for a would-be-evicted key.
- Merge cap: two devices each with N distinct keys → union → both converge to the
  same N-smallest set (idempotent re-merge is a no-op).
- Merge prune: an owner `Revoked` in the merged friend_graph has its
  revoked-set dropped; re-merging a sibling snapshot that still lists it does NOT
  re-inflate (convergence).
- `unfriend_inner` drops the local revoked-set for the peer on the `Revoked`
  transition; a `present-and-Active` owner is untouched.

---

## Part B — ZEB-691: durability rung via the butler hold (`DmInboxDoc`)

### Problem

`RevocationPush` is a bare DM control frame delivered best-effort over the live
friend tunnel (`push_revocation_to_friends` →
`send_packet_to_owner_tunnels`). A friend **offline at revocation time is never
re-delivered to** automatically — only a manual re-revoke (the `AlreadyRevoked
{ is_self: false }` retry arm, #471) or a shared-community `DeviceRetire` feed
reaches them.

### Rung choice — butler, NOT community-relay (corrected 2026-07-15)

Grounding the send clients settled which rung actually closes the gap:

- The **community-relay deposit** (`IrohCommunityRelayDepositClient`) fans out
  `for community in communities` (community_relay_prod.rs ~1063) — it only
  reaches **friends who share a community** with you. But those are exactly the
  friends the community `DeviceRetire` feed **already** covers. So the relay rung
  is redundant for the DM cutoff and does **not** close the actual gap.
- The **butler deposit** (`IrohButlerDepositClient::deposit`, butler_deposit.rs)
  resolves the recipient's **own** butler set from their reachability record
  (`resolve_async_with_source(recipient_owner)` + `freshest_butler_set_by_source`)
  with **no community gate**. This is the rung that reaches a **pure DM-only
  offline friend** (their own always-on fleet device holds the deposit), at
  parity with how chat DMs reach offline friends.

So `RevocationPush` rides the **butler hold** (`DmInboxDoc`), the friend's own
fleet. The butler unseals + keys by inner content, so this needs a synthetic key
(the ZEB-505 `:invite` / ZEB-668 `:r` precedent).

### Design — butler rung

- **Wire:** add `revocation_push: Option<Vec<u8>>` (serde `rp`) to BOTH
  `DepositPayload` (butler_deposit.rs:196) AND `DmInboxEntry` (dm_inbox_crdt.rs)
  — additive, backward-compatible (§Global Constraints). Carries the signed
  `RevocationPush` frame bytes (`DmPacket::RevocationPush`, `decode_packet`
  round-trips — from #471's `build_revocation_push_packet` / `encode_packet`).
  A revocation deposit sets `cidnotify_packet = None`, `storage_blob = empty`,
  `invite_packet = None`, `revocation_push = Some(frame)`.
- **Shared verify core.** Extract `verify_revocation_push(expected_owner,
  revocation, enrollment) -> Result<[u8;32] /* ed25519 */, DmReceiveError>` from
  `handle_revocation_push` (dm_outbox.rs:2412) — steps 1–3 (master `verify(None)`,
  trust-bind both `owner_id`s to `expected_owner`, `enrollment.verify(0)`,
  `enrollment.device_id == revocation.target`, return the bridged ed25519).
  `handle_revocation_push` then calls it + does step 4 (store + projection). The
  butler acceptor calls it for pre-validation. DRY: one authority for the checks.
- **Send:** in `push_revocation_to_friends` (owner_commands.rs:1188), alongside
  the existing best-effort live-tunnel send of `wire`, build a
  `ButlerDepositRequest` carrying `revocation_push: Some(wire)`,
  `cidnotify_packet = None`, `invite_packet = None`, `message_cid = None`, a
  synthetic `entry_id` (`OutboxEntryId([0;16])` — the butler keys on inner
  content, not `entry_id`, and the direct deposit does not ride the outbox retry
  loop, so it is inert) and a zero `space_id`, and deposit via the **butler**
  deposit client (`IrohButlerDepositClient`, resolves the friend's own butler set
  — no community gate). The client is snapshotted from `NodeState` (new
  `butler_deposit_client` field, set at the same site the outbox client is —
  lib.rs:9800). NOTE: this bypasses `push_deposit_candidate` (that helper only
  builds requests from outbox entries; a revocation is not one), so no send-side
  invite guard needs relaxing. The one required client change is the **ack
  binding**: `IrohButlerDepositClient::deposit` computes `expect_cid` — today
  `INVITE_ONLY_DEPOSIT_MARKER` when `message_cid` is `None` — and must instead
  expect `REVOCATION_DEPOSIT_MARKER` when `revocation_push.is_some()`, or the
  revocation ack mismatches and reads as a failed deposit. Best-effort:
  `SkippedNoFreshButlerSet` / failure leaves the live-tunnel push + manual
  re-revoke as the fallback. The `AlreadyRevoked { is_self: false }` retry arm
  re-drives this deposit too.
- **Butler acceptor** (`handle_deposit_core`, iroh_butler_acceptor.rs:650-791):
  new match arm for a revocation-only payload (`cidnotify_packet = None` +
  `revocation_push = Some`, matched **before** the invite `None` branch that today
  returns `BadPayload`). Reject a non-empty `storage_blob` (mirrors invite-only).
  Decode `revocation_push`; **pre-validate** the certs (D7: never persist+ack a
  forgery) via `verify_revocation_push(OwnerAddr(frame.sender_owner), …)` — this
  also binds `revocation.owner_id == frame.sender_owner` (the authenticated
  depositing friend revokes only their OWN device). Persist keyed by a new
  `DmInboxDoc::revoke_key(&frame.sender_owner, &revocation.target)` =
  `"revoke:{owner_hex}:{device_hex}"` (first segment `revoke` ≠ 32-hex space, so
  it cannot collide with `space:cid` / `space:invite`). Ack with a fixed
  `REVOCATION_DEPOSIT_MARKER` + zero `space_id`.
- **Recipient recover** (the `DmInboxDoc` sweeper — `dm_inbox_ingest`): new arm —
  if `entry.revocation_push` is `Some`, decode it, feed `handle_revocation_push`
  with `entry.sender_owner` (the butler-authenticated depositor) as
  `expected_owner`, the runtime `crdt_state`, and the `RevokedDeviceProjection`.
  On `Ok(inserted)` with `inserted`, call the owner-state `notify_dirty` hook.
  **This hook is required** (#471 lesson): a revocation has no re-delivery path
  that re-applies on restart once the deposit is ingested, so the recover MUST
  persist it via `notify_dirty`, unlike the CidNotify/invite recover arms that
  lean on re-delivery. Thread the hook onto `ProdDmInboxIngestCtx`.
- **Trigger — no new retry driver.** The existing `DmInboxDoc` sweeper recovers
  automatically on boot/reconnect, so an offline friend picks the revocation up
  on its next sweep. Satisfies "reached automatically on reconnect, without a
  manual re-revoke."

### Coverage & rationale

This gives revocations **parity with chat-DM durability to a DM-only friend's own
fleet**: any offline friend with a reachable butler (their always-on device)
gets the revocation automatically on reconnect. It reuses the proven butler
deposit + inbox-sweep machinery, adds no new persistent retry state, and
re-verifies security end-to-end (butler pre-validation + recover-side
`handle_revocation_push`). A fully-dark friend with no reachable butler stays on
the existing manual re-revoke fallback.

### Seams (Part B)

- `src-tauri/src/butler_deposit.rs` — `DepositPayload.revocation_push` field;
  `ButlerDepositRequest.revocation_push` field; `REVOCATION_DEPOSIT_MARKER` const;
  `IrohButlerDepositClient::deposit` branches `expect_cid` + the built
  `DepositPayload` on `revocation_push`. (All `DepositPayload` literal
  constructors across the crate gain `revocation_push: None`.)
- `src-tauri/src/dm_inbox_crdt.rs` — `DmInboxEntry.revocation_push` field;
  `DmInboxDoc::revoke_key`. (All `DmInboxEntry` literal constructors gain
  `revocation_push: None`.)
- `src-tauri/src/dm_outbox.rs` — extract `verify_revocation_push` from
  `handle_revocation_push`.
- `src-tauri/src/iroh_butler_acceptor.rs` — `handle_deposit_core` revocation arm
  (pre-validate via `verify_revocation_push` + `revoke_key` + reject non-empty
  blob + `REVOCATION_DEPOSIT_MARKER` ack); carry `revocation_push` into the
  persisted `DmInboxEntry`.
- `src-tauri/src/dm_inbox_ingest.rs` — `DmInboxIngestCtx::apply_revocation` trait
  method + prod impl (locks `crdt_state`, feeds `handle_revocation_push`, marks
  dirty on fresh insert); `ingest_pending` revocation arm BEFORE the
  `cidnotify_packet.is_none()` invite-only branch (dm_inbox_ingest.rs:169), with
  `ingested_by.insert` + `changed = true` like the invite-only arm;
  `ProdDmInboxIngestCtx` gains a `notify_owner_state_dirty:
  Option<Arc<dyn Fn() + Send + Sync>>` hook (`revoked` projection already present).
- `src-tauri/src/lib.rs` — new `NodeState.butler_deposit_client` field set at
  lib.rs:9800; thread `notify_owner_state_dirty` into `ProdDmInboxIngestCtx`
  (lib.rs:5167-5183) from `owner_state_engine_for_dirty` (lib.rs:4856), building
  the closure like the tunnel drain (lib.rs:9672-9676).
- `src-tauri/src/owner_commands.rs` — `push_revocation_to_friends` snapshots the
  butler client + issues a butler deposit per Active friend alongside the tunnel
  send; same on the `AlreadyRevoked` retry arm.

### Tests (Part B)

- `DepositPayload` / `DmInboxEntry` round-trip with `revocation_push` set; legacy
  payloads (no `rp`) decode to `None`; a `revocation_push`-only shape (no
  cidnotify, no invite, empty blob) round-trips.
- `DmInboxDoc::revoke_key` de-collides against `key` (`space:cid`) and
  `invite_key` (`space:invite`).
- `verify_revocation_push` accepts a valid master-signed pair and rejects a
  third-party `owner_id`, a target/enrollment mismatch, and a self-issued cert
  (mirrors the #471 `handle_revocation_push` tests, now on the extracted core).
- Butler `handle_deposit_core` on a revocation payload pre-validates + persists
  under `revoke_key`, and rejects a forged revocation (owner ≠ `frame.sender_owner`)
  fail-closed without persisting.
- Recipient sweeper on a `revocation_push` entry feeds `handle_revocation_push`
  with `entry.sender_owner`, applies to the CRDT, and marks owner-state dirty on
  a fresh insert (idempotent re-sweep marks nothing).
- Send: `push_revocation_to_friends` issues a butler deposit per Active friend
  carrying the `revocation_push` request (mock deposit client asserts the shape).

---

## Out of scope (explicit)

- **Community-relay rung (`RelayHoldDoc`) for revocations.** It reaches only
  friends who share a community — already covered by the community `DeviceRetire`
  feed, so it is redundant for the DM cutoff. Deferred; the butler rung closes the
  actual DM-only gap. Can be added later for full dual-rung chat-DM parity.
- **Sender-side persistent deposit-retry driver.** The butler deposit is
  best-effort at revoke time; a fully-dark friend with no reachable butler is
  covered by the existing manual re-revoke + shared-community `DeviceRetire`.
  No new persisted "pending revocation" set / retry loop (YAGNI for Low-pri).
- No frontend change.

## Testing strategy

Unit + integration Rust tests per §Tests above, mirroring #471's test style
(real Master certs via `RecoveryArtifact::from_seed` / `sign_master`,
`PubKeyBundle::classical_only`). No new networked/iroh test — the butler
acceptor arm and the recover sweeper are exercised in-process by feeding a
decoded payload / `DmInboxEntry` with a mock-authenticated `sender_owner`,
matching the existing `iroh_butler_acceptor` / `dm_inbox_ingest` test harnesses.
