# ZEB-730 — owner→grantee revoke-push wire (design)

**Status:** settled 2026-07-21. Trigger #2 of the received-grant GC, split from
ZEB-727 in the encrypted-file-sharing arc (ZEB-674 → 722 → 725 → 727). ZEB-727
shipped trigger #1 (grantee-local dismiss) + the convergent LWW dismiss-tombstone
substrate; this delivers trigger #2 (owner-revoke propagation), which ZEB-727
could not — it needs a **net-new deposit wire**.

## Problem

An owner's file-share revoke is **invisible to the grantee** today. Verified by
code trace (2026-07-21):

- `revoke_read_impl` (`lib.rs:20567`) tombstones only the owner-local `GrantEntry`
  (ZEB-725 LWW `revoked_at`), `notify_dirty`s to the **owner's own** fleet (Flow
  A), and emits the owner's `"grants-updated"` UI event. It snapshots **no**
  `butler_deposit_client` and sends **nothing** to the grantee — no deposit, no
  frame. (Contrast `grant_read_impl` at `lib.rs:20443`, which *does* deposit.)
- `ingest_grant_push` (`file_sharing.rs:220`) is the **only** writer of
  `received_file_grants`, and it runs only on the **share** path, driven by
  `DepositPayload.grant_push`.
- `DepositPayload` (`butler_deposit.rs:213`) carries exactly one file-sharing
  field — `grant_push` (share only). **No revoke variant exists on the wire.**

So the owner's revoke converges across the owner's own devices (ZEB-725) but the
grantee's `received_file_grants` copy is never pruned. This ticket adds the
owner→grantee revoke signal and routes it into ZEB-727's tombstone.

## The tombstone this plugs into (unchanged from ZEB-727)

`OwnerState.dismissed_received_grants: BTreeMap<[u8;32], u64>` (cid →
dismissed_at_ms; `owner_state_crdt.rs`, serde key `"dg"`). A received grant is
**active iff `received_at > dismissed_at`.** `received_at` is stamped
grantee-local at ingest (`now_epoch_ms()`, never wire-supplied). The merge join +
active-status-first union tie-break + sweep already converge (ZEB-727 design
`docs/specs/2026-07-21-zeb-727-received-grant-gc-design.md`). Trigger #2 adds a
**new signal into this tombstone** — no new GC substrate, no merge change.

## Design

### Wire — a 6th `Option` field on `DepositPayload` (NOT an enum)

`DepositPayload`, `ButlerDepositRequest`, and `DmInboxEntry` are all
**structs-of-`Option`s**: each sub-payload (message / invite / friend-revocation /
grant) is a distinct optional field, and "which variant" is decided everywhere by
**pure-shape guards** — one `Option` is `Some`, all sibling payload `Option`s are
`None` — enforced fail-closed at the butler (`iroh_butler_acceptor.rs`) and
**re-checked** at the recipient sweep (`dm_inbox_ingest.rs`), because a sibling
doc-merge is a trust boundary. There is no enum discriminant.

Add a 6th field, mirroring `grant_push`:

```rust
/// ZEB-730: owner→grantee file-grant revoke. Canonical CBOR of the revoked root
/// ContentId ([u8;32]). The whole DepositPayload is frame-sealed to the
/// recipient, so — unlike grant_push (which per-device-seals a DEK secret) —
/// this carries the CID in the clear inside the seal: there is no secret here
/// the grantee doesn't already hold. Sender is the butler-verified
/// frame.sender_owner, never a payload claim.
#[serde(rename = "gr", default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
pub grant_revoke: Option<Vec<u8>>,
```

Serde key `"gr"` is free within `DepositPayload` (existing keys `cn`/`pl`/`iv`/
`rp`/`gp`; the canonical-CBOR equal-length-key self-check is **per-struct**, so
reusing `gr` — which `ReceivedFileGrant.granter_owner` uses in a *different*
struct — is fine). New constants in `butler_deposit.rs`:
`GRANT_REVOKE_DEPOSIT_MARKER = b"zeb730-grant-revoke"` and
`MAX_DEPOSIT_GRANT_REVOKE_BYTES = 256` (a single 32-byte CID as CBOR is ~35B;
256 is generous headroom, fail-closed on overflow).

**Payload chosen minimal (approved):** the revoked CID only, no per-device
sealing. There is no DEK to protect and the frame seal already encrypts the whole
payload to the recipient owner; per-device sealing would add seal/unseal +
device-key resolution for a value that carries no secret (YAGNI). One revoke =
one `(cid, grantee)` pair = one deposit; batching multiple CIDs is a future wire
evolution, not now.

### Owner send — `revoke_read_impl` deposits a `grant_revoke`

Extend `revoke_read_impl` (`lib.rs:20567`) to, **after** a successful local
`revoke_grant_inner` (`removed == true`), fire a **best-effort** deposit to the
grantee, mirroring `grant_read_impl`'s deposit block (`lib.rs:20512`):

```rust
if let Some(butler) = butler_deposit_client_snapshot {
    let req = ButlerDepositRequest {
        entry_id: OutboxEntryId([0u8; 16]),
        recipient_owner: grantee_owner,          // GrantEntry.grantee_owner (master OwnerAddr)
        space_id: SpaceId([0u8; 16]),
        message_cid: None,
        cidnotify_packet: None,
        invite_packet: None,
        revocation_push: None,
        grant_push: None,
        grant_revoke: Some(encode_grant_revoke(cid)),   // canonical CBOR of the CID
        now_ms,
    };
    let _ = butler.deposit(&req).await;   // best-effort; local revoke already succeeded
}
```

`grantee_owner` is already parsed in `revoke_read_impl` from the IPC
`grantee_address`; `GrantEntry` stores exactly this master `OwnerAddr` (no device
address, no key — `owner_state_types.rs:2531`), which is the same
`recipient_owner` value `grant_read_impl` uses. So routing needs no extra state.
The butler snapshot is taken the same way `grant_read_impl` snapshots
`guard.butler_deposit_client`.

**Best-effort, no retry (documented limitation):** if the grantee's butler is
unreachable at revoke time, the revoke-push is dropped (the same best-effort
contract `grant_read_impl`'s deposit uses). The owner's local revoke still
succeeds. Trigger #1 (manual grantee dismiss) remains the backstop. Adding a
retry/outbox rung for revoke is a separate concern (YAGNI; the manual dismiss
already covers the hygiene need).

### Butler accept — a Friend-scoped, pure-shape `grant_revoke` arm

In `handle_deposit_core`'s sub-payload chain (`iroh_butler_acceptor.rs:660-955`,
inside the `cidnotify == None` block), add a `grant_revoke` arm mirroring the
grant arm (`:828-878`):

- **Friend-scoped:** `if !matches!(admission, Admission::Friend(_)) { return
  Err(DepositReject::NotAuthorizedForScope); }` — a file-grant revoke is
  friend-scoped exactly like the grant.
- **Pure-shape:** reject if `storage_blob` non-empty, or any of
  `cidnotify`/`invite`/`revocation_push`/`grant_push` is `Some`.
- **Size cap:** reject if `gr_bytes.len() > MAX_DEPOSIT_GRANT_REVOKE_BYTES`.
- Key it by `DmInboxDoc::grant_revoke_key(&frame.sender_owner, gr_bytes)`, ack
  with `GRANT_REVOKE_DEPOSIT_MARKER`.

And extend **every other arm's stray-field guards** (message arm `:668-679`,
revocation arm, grant arm `:861-863`, invite arm) to reject a stray
`grant_revoke.is_some()` — the fail-closed completeness that keeps a
one-`Option`-only invariant. Sender-side marker-select
(`butler_deposit.rs:698-717`) gets a parallel
`None if req.grant_revoke.is_some() => (Vec::new(), GRANT_REVOKE_DEPOSIT_MARKER)`
arm and threads `grant_revoke: req.grant_revoke.clone()` into the constructed
`DepositPayload`.

### Recipient sweep — a `grant_revoke` arm → `apply_grant_revoke`

In `ingest_pending` (`dm_inbox_ingest.rs:158-333`), add an arm mirroring the
grant arm (`:224-243`):

```rust
if entry.grant_revoke.is_some()
    && entry.cidnotify_packet.is_none()
    && entry.invite_packet.is_none()
    && entry.revocation_push.is_none()
    && entry.grant_push.is_none()
{
    match ctx.apply_grant_revoke(entry).await {
        Ok(()) => { entry.ingested_by.insert(self_id.clone()); changed = true; }
        Err(e) => { tracing::warn!(...); }
    }
    continue;
}
```

Extend every other arm's guard with `&& entry.grant_revoke.is_none()`. Add
`apply_grant_revoke` to the ingest-context trait (`dm_inbox_ingest.rs:116-126`)
and a prod impl (`:1272-1305`) mirroring `apply_grant_push`.

### Grantee ingest — the authorization check (the security crux)

`apply_grant_revoke` calls a **pure** `file_sharing` seam:

```rust
/// Returns true iff the tombstone advanced or an entry was removed (→ caller
/// notify_dirty + emit). Honors the revoke ONLY when the butler-verified
/// `granter_owner` matches the granter-of-record on the local received grant —
/// otherwise a no-op, so no Active friend can grief a grantee into losing a
/// file they did not share.
pub fn ingest_grant_revoke(
    state: &mut OwnerState,
    granter_owner: OwnerAddr,     // butler-verified frame.sender_owner
    cid: [u8; 32],
    now_ms: u64,
) -> bool {
    match state.received_file_grants.get(&cid) {
        Some(g) if g.granter_owner == granter_owner => {
            // authorized: GC the entry AND stamp the convergent tombstone
            dismiss_received_grant_inner(state, cid, now_ms)   // reuse ZEB-727 seam
        }
        _ => false,   // no matching active grant → drop (already-dismissed / not-ours / griefing)
    }
}
```

- **Authorization by granter-of-record.** `received_file_grants[cid].granter_owner`
  (`owner_state_types.rs:2573`, serde `"gr"`) is compared against the
  butler-verified sender. Because `received_file_grants` is Flow-A replicated,
  every device that holds the entry can verify — no separate granter registry.
- **Reuses the ZEB-727 seam.** On authorization it calls
  `dismiss_received_grant_inner(state, cid, now_ms)`, which removes the entry
  **and** stamps `dismissed_received_grants[cid] = max(existing, now_ms)` (LWW,
  monotonic). No new tombstone logic — trigger #2 is literally trigger #1's seam
  driven by a remote signal instead of a local IPC.
- **Clock domain.** `now_ms = now_epoch_ms()` (grantee receipt time), the **same
  clock** as `received_at`. Never the owner's `revoked_at` — that would be a
  cross-clock comparison clock skew could invert. Intra-clock LWW is the correct,
  convergent choice.
- **notify_dirty is load-bearing.** On `true`, `apply_grant_revoke` calls the
  `notify_owner_state_dirty` mark and emits `"shared-with-me-updated"` — same as
  `apply_grant_push`. `received_file_grants`/`dismissed_received_grants` have **no
  deposit-rung re-delivery backstop**, so persistence + Flow-A replication depend
  entirely on `notify_dirty` (ZEB-709).
- **Idempotent under re-delivery.** The DmInbox CRDT marks `ingested_by` per
  device, so a given revoke entry applies once per device; `max`-join makes even a
  re-apply monotonic.

## Convergence / ordering

- **Common case (revoke, no re-share):** grantee stamps `dismissed_at = now >
  received_at` → entry inactive on the projection and swept on merge; Flow-A +
  ZEB-727's merge converge the tombstone across the grantee's fleet. ✓
- **In-order re-share after revoke:** owner revokes (grantee time R1, stamp
  `dismissed_at = R1`), then re-shares; `grant_push` ingests at R2 > R1 with
  `received_at = R2 > R1` → active again (ZEB-727's re-share reactivation). ✓
- **Authorization convergence:** every fleet device with the (Flow-A replicated)
  entry can verify the granter, so whichever device ingests the deposit stamps a
  tombstone that then replicates; a device that already dismissed (entry gone)
  drops the revoke harmlessly (already hidden). ✓

## Scope

**In:**
- `grant_revoke: Option<Vec<u8>>` on `DepositPayload`, `ButlerDepositRequest`,
  `DmInboxEntry` (+ `default`/`skip_serializing_if`, absent-loads-empty).
- `GRANT_REVOKE_DEPOSIT_MARKER`, `MAX_DEPOSIT_GRANT_REVOKE_BYTES`;
  `encode_grant_revoke`/`decode_grant_revoke` (canonical CBOR of `[u8;32]`).
- Sender marker-select arm (`butler_deposit.rs`).
- Butler accept arm + stray-field guards on all arms (`iroh_butler_acceptor.rs`).
- Recipient sweep arm + sibling-guard extensions + `apply_grant_revoke`
  trait method & prod impl (`dm_inbox_ingest.rs`); `DmInboxDoc::grant_revoke_key`
  (`dm_inbox_crdt.rs`).
- `ingest_grant_revoke` pure seam (`file_sharing.rs`) reusing
  `dismiss_received_grant_inner`.
- Owner-side best-effort deposit in `revoke_read_impl` (`lib.rs`).

**Not in scope (unchanged across this lineage):**
- **Cryptographic access withdrawal.** An already-delivered DEK cannot be
  recalled without rotation + re-encrypt. This is **state hygiene, not access
  revocation** — it makes the grantee's "shared with me" entry disappear; it does
  not revoke the bytes (ZEB-725/727 caveat).
- **Out-of-order revoke/re-share.** A revoke deposit delivered *after* a later
  legitimate re-share (network reordering) re-suppresses the re-share (grantee
  re-dismiss recovers; owner re-share re-fires). Same class as ZEB-727's replay
  non-goal.
- **Revoke deposit retry.** Best-effort, no outbox rung (grant_read parity);
  trigger #1 manual dismiss is the backstop.
- **UI.** No frontend change — the grantee's "shared with me" list already
  re-renders on `"shared-with-me-updated"` (ZEB-723). Backend-only PR.

## Testing

Keychain-free, wall-clock-free where possible; mirror the `grant_push` wire +
ingest tests and the ZEB-727 tombstone tests.

1. **Wire round-trip** — a `DepositPayload` with `grant_revoke: Some(cbor(cid))`
   and all other sub-payloads `None` encodes/decodes byte-identically (canonical
   CBOR); a pre-ZEB-730 payload (field absent) decodes with `grant_revoke: None`.
2. **`encode/decode_grant_revoke`** round-trips a `[u8;32]`.
3. **Butler accept — happy path** — a Friend-admitted, pure-shape `grant_revoke`
   within cap is accepted and keyed by `grant_revoke_key`.
4. **Butler accept — rejects** — non-Friend admission → `NotAuthorizedForScope`;
   stray sibling field (`grant_push`/`storage_blob`/…) → `BadPayload`; over-cap →
   `BadPayload`. And every *other* arm rejects a stray `grant_revoke`.
5. **`ingest_grant_revoke` — authorized** — seed
   `received_file_grants[cid]{granter_owner=G}` → `ingest_grant_revoke(G, cid, now)`
   → entry gone, `dismissed_received_grants[cid] == now`, returns `true`.
6. **`ingest_grant_revoke` — unauthorized (griefing guard)** — granter-of-record
   is `G` but sender is `H != G` → no-op, entry intact, no tombstone, returns
   `false`. **The security regression guard.**
7. **`ingest_grant_revoke` — no entry** — CID absent (never-received /
   already-dismissed) → no-op, returns `false` (no tombstone minted from an
   unverifiable revoke).
8. **Re-share after revoke reactivates** — authorized revoke stamps
   `dismissed_at`; a later `ingest_grant_push` with `received_at > dismissed_at`
   makes the grant active again (projection + merge sweep) — reuses ZEB-727's
   reactivation guarantee end-to-end.
9. **`apply_grant_revoke` (prod-path)** — on authorized revoke, mutates
   owner-state, calls `notify_dirty`, emits `"shared-with-me-updated"` with the
   canonical (lowercase) cid; on unauthorized/no-entry, neither.

Local gates (CI-exact): `cargo fmt --all -- --check`; `cargo clippy --locked
--all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest
run --locked --workspace --all-targets --features test-fixtures` (iterative:
`-p harmony-app --lib`; CI's 3-shard sweep is the backstop).

## Non-goals recap (billions-scale rationale)

Same as ZEB-727: `received_file_grants` is a grow-only, fully-replicated map;
leaving revoked grants in it forever is an unbounded per-fleet liability. This
change lets an owner-revoke prune it convergently. It is hygiene + UX, not a
cryptographic control.
