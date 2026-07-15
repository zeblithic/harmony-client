# ZEB-685 — DM-only device-revocation cutoff (ZEB-580 S3, friend-scoped RevocationCert propagation)

**Status:** design approved 2026-07-15 · **Owner:** Koya · **Epic:** ZEB-580 (S3, out-of-epic tail)
**Builds on:** `docs/specs/2026-07-13-zeb-580-dm-signing-migration-design.md` (§N1, §5). S1 (PR #465), S2 (PR #466), S2-guard (PR #467) merged.
**Scope decision (Jake, 2026-07-15):** core N1 only. Both N3 residuals (quorum-via-DmInvite `inviter_signer_certs`; tunnel-rebuild cert-less #2 TOFU hardening) are **deferred** — the fleet is master-issued so neither bites, and the TOFU concern is orthogonal to cutoff soundness (the cutoff keys on the ed25519 present in any cached #2 identity).

## 1. Problem

S2 built the DM revocation cutoff: `RevokedDeviceProjection { by_owner: owner → {revoked #2 ed25519 keys} }`, fed from community `revoked_device_keys`, read by three §5.2 cutoff sites in the DM receive path (`dm_outbox.rs:3380` CidNotify, `:2493` invite, `:2239` ack). But the projection only learns revocations from **communities the receiver shares with the sender** (spec Fact B: the revocation fact is siloed per-community, no cross-owner aggregate). A **DM-only contact** — someone you message directly with no shared community — has no substrate carrying the revocation, so S2 cannot block a DM from that contact's revoked device.

**Goal (N1):** when owner A revokes device D, A pushes a master-signed `RevocationCert` (+ the paired `EnrollmentCert`) to each friend B over the friend-DM tunnel; B verifies and stores it friend-scoped, and the existing §5.2 cutoff then rejects DMs signed by D — for DM-only contacts, closing the gap.

## 2. Key facts (from the seam map)

- **`RevocationCert`** (pinned `harmony-owner` dep, `certs/revocation.rs`): `{ version, owner_id[16], target[16], issued_at, issuer, reason, signature }`. **`target` is the device's `device_id[16]`, NOT its ed25519 key.** Master-signed via `sign_master(master_sk, master_pubkey, target, issued_at, reason)`; verified via `verify(Some(master_vk))`.
- The **projection keys on ed25519[32]**, which the cert does not carry. The community path bridges `[16]→[32]` by shipping the paired `EnrollmentCert` in `DeviceRetire { revocation, enrollment }` and extracting `enrollment.device_pubkeys.classical.ed25519_verify` (`community_membership.rs:2838`). **S3 mirrors this: the friend push carries both.**
- **No ongoing friend-message channel exists** (the friend ALPN handshake is one-shot). The ongoing owner→owner path is the **DM tunnel** (`IrohTunnelDmTransport`), which has **butler-deposit durability** for offline recipients. The DM layer already carries non-chat **control frames** (`CidNotify`, `Ack`, `DmInvite`) alongside chat content.
- **`FriendGraph { friends: BTreeMap<OwnerAddr, FriendEntry> }`** is a sub-CRDT embedded in the owner-state CRDT (`owner_state_crdt.rs:76`), persisted in `owner_state.cbor` and **replicated across the user's own devices** (LWW per entry). Mutated via `apply_friend_update`.
- **Revoke trigger:** `revoke_device_inner` (`owner_commands.rs:867`) mints the cert (`cert_for_feed`, `:1009`) and fires the existing vine-feed (`:1059`) and community-retire (`:1138`) hooks. The S3 friend-push hook belongs alongside these.

## 3. Design

### 3.1 New DM control frame: `RevocationPush`

Add a control frame (sibling to `DmInvite`) carrying `{ revocation: RevocationCert, enrollment: EnrollmentCert }`. It rides the existing DM tunnel transport + butler-deposit path — durable to offline friends — and is **not** a chat message (no Space, no message-log entry, never rendered), exactly as `DmInvite` seeds identity without being a chat message. Additive wire per the team rule (no version byte; matches the `revoked_device_keys` / `inviter_enrollment` precedent).

### 3.2 Send side (A revokes device D)

In `revoke_device_inner`, after `cert_for_feed` is minted and alongside the existing hooks: enumerate `state.friend_graph.friends`; fetch device D's `EnrollmentCert` from `state.enrollments` (the owner's own enrollment map); for each friend, enqueue a `RevocationPush { revocation: cert_for_feed, enrollment }` over the friend's DM tunnel. Best-effort + durable (butler-deposit); a friend offline at revoke time receives it on reconnect. No new broadcast primitive — per-friend send over the existing transport.

### 3.3 Receive side (B) — verify, trust-bind, store

On `RevocationPush` ingest (a new arm in `ingest_dm_packet`, sibling to `apply_invite`):
1. **Verify** `revocation.verify(Some(master_vk))` — Master-issued, `master_pubkey.identity_hash() == revocation.owner_id`, signature valid. Reject `SelfDevice`/`Quorum`-issued pushes (out of scope; core is master-issued).
2. **Trust-bind** `revocation.owner_id == the sending friend's owner` AND `enrollment.owner_id == that same owner`. A friend may only revoke **their own** devices in B's view — no relaying third-party revocations (prevents projection-injection/griefing).
3. **Bridge + bind** `revocation.target == enrollment.device_id`; extract `ed25519 = enrollment.device_pubkeys.classical.ed25519_verify`.
4. **Store friend-scoped, union-merged:** union `ed25519` into a friend-scoped revoked-set in the owner-state CRDT. **Correctness constraint:** this set MUST merge by **union** (grow-only), NOT by `FriendEntry`'s per-entry `learned_at` LWW — otherwise two of B's own devices each receiving a *different* revocation would LWW-clobber and drop one. So it is NOT a plain field on the LWW `FriendEntry`; it is a union-merged structure (mirroring the community `revoked_device_keys: BTreeSet<[u8;32]>` union-merge). Because it lives in the owner-state CRDT, all of B's own devices converge on the full (unioned) cutoff set. See Q1 for the exact shape.
5. Do **not** deliver to the chat UI; do **not** ack as a message.

Malformed cert, wrong issuer, owner mismatch, or `target != device_id` → drop (same disposition as a signature failure).

### 3.4 Cutoff integration (zero change to the three check sites)

Feed the friend-scoped revoked ed25519 keys into the **same** `RevokedDeviceProjection.by_owner` map:
- **On receive:** after step 4, `union_from_members([(friend_owner, &{ed25519})])` (or equivalent single-key union) into the live projection.
- **On boot-replay:** alongside the existing community feed (`lib.rs:7930`), also union every friend's `revoked_device_ed25519` set into the projection, keyed by that friend's owner — mirroring `feed_revoked_from_materialized`.

The projection stays sticky/monotonic. The three §5.2 cutoff sites (`dm_outbox.rs:3380/2493/2239`) are unchanged — they already read `by_owner`.

## 4. Security & edge cases

1. **No third-party injection.** §3.3 step 2 binds the revocation's owner to the sending friend, so A cannot push a revocation for owner C into B's projection.
2. **Authenticity.** Only a valid Master-signed cert (A's master over device D's `device_id`) is accepted; the paired enrollment must chain to the same owner and target the same device.
3. **Sticky.** The friend-scoped set is monotonic within the projection (matches S2); un-revoke is not modeled.
4. **Offline durability.** The push rides butler-deposit, so a friend offline at revoke time cuts off on reconnect. If the deposit never lands (both offline forever), the cutoff simply doesn't apply for that pair — no worse than today; the community path is the belt-and-suspenders when a shared community exists.
5. **Self-revocation not pushed to friends via #2** — a `SelfDevice`-issued revocation (the device revoking itself) is not a master attestation; §3.3 accepts only Master-issued. Master-revoke is the load-bearing case (a compromised device won't self-revoke).
6. **Idempotent.** A re-delivered `RevocationPush` (butler replay) unions an already-present ed25519 → no-op.

## 5. Testing strategy

- **Unit — RevocationPush verify/trust-bind:** master-issued accepted; wrong-owner (friend≠revocation.owner) rejected; `target != enrollment.device_id` rejected; SelfDevice/Quorum rejected; idempotent re-apply.
- **Unit — friend-scoped store + projection feed:** applying a RevocationPush unions the ed25519 into `by_owner[friend]`; `is_revoked` then true; boot-replay from persisted `FriendGraph` re-seeds it.
- **Unit — cutoff wiring:** a DM signed by the revoked #2 from a DM-only contact is rejected at the CidNotify site once the friend-scoped revocation is present (was accepted before).
- **Integration (co-located, no fleet needed):** two co-located serve nodes, friend handshake, A revokes device D → B receives the push → B's projection cuts off a subsequent D-signed DM. Runs solo on Koya.
- Gates: `cargo fmt`, `clippy --locked --all-targets --features test-fixtures -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit` / `npx vitest run` if any TS touched (likely none — pure backend). Iterative via `scripts/test-select`.

## 6. Non-goals

- N3 residuals (quorum-via-DmInvite `inviter_signer_certs`; tunnel TOFU hardening) — deferred (Jake, 2026-07-15).
- No un-revocation / revocation-expiry modeling (sticky, matches S2).
- No new broadcast transport — per-friend DM-tunnel push reusing butler-deposit.
- No honesty-copy change: S2 already narrowed the dialog; once this ships, a follow-up can widen the copy to "…including contacts you only message directly," but the copy update is out of this slice's core (file if desired).

## 7. Open questions (settle in the plan)

- **Q1 — storage shape (must be union-merged, per §3.3.4):** the friend-scoped revoked set cannot be a plain LWW field. Two candidate shapes, both union-merged: **(a)** a new sibling map in the owner-state CRDT — `revoked_dm_devices: BTreeMap<OwnerAddr, BTreeSet<[u8;32]>>` — with a union-merge arm in the owner-state CRDT merge (cleanest; keyed exactly like the projection, decoupled from `FriendGraph`'s LWW); **(b)** a set field on `FriendEntry` given a **custom union merge** carved out of the per-entry LWW (couples to `FriendEntry`, needs a merge-semantics exception). **Lean: (a)** — it mirrors the projection's `by_owner` shape, needs no `FriendEntry` merge exception, and boot-replay is a direct map iteration. Confirm the owner-state CRDT has a clean seam to add a union-merged sub-map (mirror how `owner_device_cache` or a community-CRDT grow-set is merged). Settle in the plan by reading the owner-state CRDT merge fn.
- **Q2 — send enqueue seam:** exact `RevocationPush` send call (reuse the `DmInvite` enqueue path in `DmOutbox` vs a dedicated control-send). Settle against the `dm_envelope`/`DmOutbox` control-frame code in the plan.
