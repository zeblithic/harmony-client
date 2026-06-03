# Phase 4 — invite-only `generate_invite` (cross-WAN gate)

- **Status:** design approved 2026-06-02
- **Spec A of the cross-WAN community arc** (Spec B = ZEB-321 Phase 2 Zenoh-over-iroh ingestion, separate doc)
- **Blocks / fixes:** [ZEB-366](https://linear.app/zeblith/issue/ZEB-366) (cross-WAN iroh join has no working generate→redeem loop), [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) (Sub-D end-to-end validation)
- **Builds on:** ZEB-262 (Phase 4 invite-only design — Reticulum-framed), ZEB-323 (case-A pkarr publisher), ZEB-325 (iroh redeem handshake), ZEB-339 (enrolled device-#2 signing)

## Problem

`generate_invite` (`src-tauri/src/lib.rs:14812`) supports **open communities only**; invite-only generation hard-errors:

```rust
// lib.rs:14873-14878
if is_invite_only {
    return Err("Phase 3 supports OPEN communities only; invite-only generate_invite ships in Phase 4");
}
```

But the iroh cross-WAN redeem (`connectivity_redeem_invite_iroh_inner`, lib.rs ~31650) **only** accepts invite-only invites — it returns `inviter_unreachable` the instant `payload.invite_token` is `None` (lib.rs:31723) and derives its case-A pkarr lookup keys from `invite_token.sig`. So the two halves never overlap:

| | Generate | Redeem (iroh) |
|---|---|---|
| **Open** | ✅ Phase 3 | ❌ no token |
| **Invite-only** | ❌ not shipped (this spec) | ✅ case-A path exists |

Cross-WAN join is therefore structurally impossible today. This spec ships the missing half: invite-only invite **generation**, producing invites the iroh redeem can consume.

## Scope

In: invite-only `generate_invite` (mint the signed `InviteToken`, seal the epoch key, extract the admin bootstrap, populate the payload, publish the case-A pkarr record); the matching redeem-side decrypt branch for untargeted invites; unregister-on-consume of the case-A publication.

Out: ongoing cross-WAN CRDT sync after join (Spec B — Zenoh-over-iroh ingestion); DM-over-iroh migration (separate track); offline-countersigner redemption (ZEB-254).

## Two invite models (both supported)

Open communities ship the 32-byte epoch key **in the clear** in the URL. Invite-only **seals** it (92-byte `seal_to_owner` envelope) so the link alone is insufficient. Two recipient models, with **different guarantees**:

- **Targeted** — seal to a *specific* invitee's enrolled **device-#2 X25519** key. The epoch key is genuinely confidential to that invitee (true invite-only). `invite_token.invitee_hint = Some(invitee_addr)`; the redeemer's `join_event.actor` must equal it. Reuses `seal_to_owner` verbatim — **no new cryptography**. Requires the invitee's devices to already be resolvable (a known peer).
- **Untargeted** — a single-use link for *anyone*. Generate a one-time **ephemeral X25519 keypair**, seal the epoch key to its public half, and carry the **private** half in the URL. The URL holder can decrypt the epoch key — so confidentiality is **no better than Open**; the value is **single-use + admin-countersigned membership** ("controlled open"). The link must be treated as a secret (sent privately, burned on use). `invite_token.invitee_hint = None`.

## Architecture — a new `invite_mint` module

`generate_invite` is already large; extract the mint primitives into a focused, unit-testable module (`src-tauri/src/invite_mint.rs`). Three units, each one job, each with a well-defined interface and no Tauri/AppState dependency (so they test without a node):

### Unit 1 — `mint_invite_token`
```rust
pub fn mint_invite_token(
    inviter: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    minted_at: Hlc,
    expires_at: Option<u64>,
    device2_signing_key: &ed25519_dalek::SigningKey,
) -> InviteToken
```
Builds the unsigned token, computes `canonical_invite_token_bytes(&token)` (the existing public sig-preimage helper, `community_invite.rs:1533`), signs with the **enrolled device-#2 key** per ZEB-339, and sets `token.sig`. This is the single biggest missing primitive — the verify counterpart (`verify_invite_token_sig_device_key`, `community_invite.rs:1617`) already exists; this is its mint mirror.

### Unit 2 — `seal_epoch_key`
```rust
pub enum SealRecipient { Targeted([u8; 32] /* invitee device-2 x25519 pub */), Untargeted }
pub struct SealedEpochKey { pub sealed: [u8; 92], pub untargeted_decrypt_key: Option<[u8; 32]> }

pub fn seal_epoch_key(epoch_key: &[u8; 32], recipient: SealRecipient) -> SealedEpochKey
```
- `Targeted(pub)` → `seal_to_owner(pub, epoch_key)`; `untargeted_decrypt_key = None`.
- `Untargeted` → generate ephemeral X25519 keypair, `seal_to_owner(ephemeral_pub, epoch_key)`, return `untargeted_decrypt_key = Some(ephemeral_priv)`.

### Unit 3 — `extract_admin_bootstrap`
```rust
pub fn extract_admin_bootstrap(
    materialized_log: &CommunityEventLog,   // or the engine's event view
    community_id: SpaceId,
    admin_addr: OwnerAddr,
) -> Result<SignedMembershipEvent, InviteMintError>
```
Pulls the admin's bootstrap-Join (the community's event 0, with its `EnrollmentCert` embedded) from the log. The redeemer pre-inserts this so its empty CRDT can verify the admin's publish-back. `verify_admin_bootstrap` (`community_invite.rs:1211`) already validates it; this extracts it at generation time.

## Wire-format change

Add one field to `CommunityInvitePayload` (`community_invite.rs:89`):
```rust
/// Untargeted invite-only only: the ephemeral X25519 private key the redeemer
/// uses to open `sealed_epoch_key`. Rides ONLY in the URL — never in the case-A
/// pkarr record (which publishes routing keyed by token.sig), and OUTSIDE the
/// token-sig preimage (canonical_invite_token_bytes), so it perturbs neither the
/// signature nor the case-A key derivation. `None` for targeted + open invites.
#[serde(rename = "ud", skip_serializing_if = "Option::is_none")]
pub untargeted_decrypt_key: Option<[u8; 32]>,
```
`encode_invite_url` / `decode_invite_url` gain a guard: `untargeted_decrypt_key` is permitted only when `is_invite_only && invite_token.invitee_hint.is_none()`; rejected on open payloads and on targeted invite-only (mirrors the existing `OpenCommunityHasBootstrap` style of guard).

## The `generate_invite` invite-only branch (replaces the stub)

```
1. Power check: caller power ≥ POWER_THRESHOLDS.invite (existing).
2. recipient =
     targeted(invitee_addr)  → resolve invitee device-#2 X25519 from OwnerDeviceCache
                               (err if unresolvable; see Errors)
     untargeted              → SealRecipient::Untargeted
3. sealed = seal_epoch_key(epoch_key, recipient)
4. token  = mint_invite_token(self_owner, invitee_hint, hlc_now, expires_at?, device2_key)
5. bootstrap = extract_admin_bootstrap(log, community_id, admin_addr)
6. payload = CommunityInvitePayload {
     is_invite_only: true,
     invite_token: Some(token),
     admin_bootstrap: Some(bootstrap),
     admin_identity_pub: Some(self reticulum identity pub from dm_outbox.private_identity),
     inviter_enrollment: Some(inviter_enrollment_cert),   // already snapshotted at lib.rs:14841, unused in Phase 3
     epoch_snapshot.sealed_epoch_key: sealed.sealed,       // 92 bytes
     untargeted_decrypt_key: sealed.untargeted_decrypt_key,
     ..common(state_snapshot, community_name, etc.)
   }
7. url = encode_invite_url(&payload)            // all invite-only guards already exist
8. pkarr_invite_publisher.register_invite(&payload)   // case-A publish fires automatically (token is Some)
9. return url
```

Whether the inviter must BE the admin (only the admin holds the epoch key + can produce `admin_bootstrap`) vs. a non-admin member inviting (would need the admin's bootstrap from the synced log + their own epoch-key copy): **v1 restricts invite-only generate to the admin** (simplest correct cut; non-admin invite is a follow-up). The power check already gates this; the spec makes it explicit.

## Redeem-side touch (small, same spec)

`mint_redemption` (lib.rs:16409) decrypts `sealed_epoch_key` with the invitee's enrolled device-#2 X25519. Add one branch:
```rust
let x25519_priv = match payload.untargeted_decrypt_key {
    Some(ephemeral_priv) => ephemeral_priv,                 // untargeted: key rides in the URL
    None => ed25519_priv_to_x25519(&signing_key),           // targeted: invitee's device-2 key
};
open_from_owner(&x25519_priv, &sealed_epoch_key)
```
Everything else (token verify, admin-bootstrap insert, iroh countersign) is already built.

## Unregister-on-consume (in this spec)

The case-A pkarr publication must stop once the invite is consumed (single-use) or expired, so stale routing isn't served and the DHT slot is freed (`TODO ZEB-323 §5` at lib.rs:15041). `PkarrInvitePublisher::unregister_invite(&sig)` already exists; wire it at the **countersign-acceptance** points, where the inviter learns a specific token was redeemed:

- **iroh path:** `IrohInviteHandshakeAcceptor` (`iroh_invite_acceptor.rs`) — after countersigning the inbound `CommunityInviteSigned`, call `unregister_invite(token.sig)`.
- **Reticulum path:** `community_invite::handle_unicast` — same, after countersigning.
- **Expiry:** on register, the publisher already key-rotates per epoch; add a lazy guard so a token past `expires_at` is unregistered on next touch (or a low-frequency sweep). Targeted single-redemption and untargeted single-use both unregister on first successful countersign.

(Wiring the unregister call requires threading the `PkarrInvitePublisher` handle into the acceptor paths — a NodeState handle both already partially carry.)

## Error handling

- Targeted invitee unresolvable → `"can't target <addr>: their devices aren't known yet — use an untargeted link"`.
- Caller power < `POWER_THRESHOLDS.invite` → existing insufficient-power error.
- Non-admin caller (v1) → `"only the admin can generate invite-only invites (v1)"`.
- `expires_at`: default **7 days** when the caller passes `None` (Phase 3 ignored it; Phase 4 honors it — it's bound into the token sig, so the redeemer enforces expiry).
- `encode_invite_url` already rejects malformed invite-only payloads (missing token / bootstrap / inviter_enrollment, wrong sealed-key length); the new `untargeted_decrypt_key` guard joins them.

## Testing (TDD)

- **`mint_invite_token`:** mint → `verify_invite_token_sig_device_key` passes; tamper a field → verify fails.
- **`seal_epoch_key`:** targeted round-trip (`seal_to_owner` → `open_from_owner` with the invitee key recovers the epoch key); untargeted round-trip (recovers via the returned ephemeral private); untargeted returns `Some(key)`, targeted returns `None`.
- **`extract_admin_bootstrap`:** returns the admin's Join-0 with cert; `verify_admin_bootstrap` accepts it.
- **`generate_invite` invite-only:** targeted sets `invitee_hint` + leaves `untargeted_decrypt_key` None; untargeted sets `untargeted_decrypt_key` + `invitee_hint` None; both pass `encode_invite_url`; `register_invite` fires (case-A) iff `invite_token` is `Some`; open still leaves token None + no publish.
- **Wire guard:** `untargeted_decrypt_key` on an open or targeted payload → `encode/decode` rejects.
- **Full redeem round-trip (both kinds)** through `connectivity_redeem_invite_iroh_inner` — extends the existing iroh integration tests, which today can't exercise the happy path for lack of a generated invite-only invite. This closes the end-to-end gap and is the spec's acceptance test.
- **Unregister-on-consume:** after a countersigned redemption, `unregister_invite(sig)` was called; a second redeem of the same token finds no case-A record.

## Relationship to Spec B

Spec A makes a cross-WAN **join** succeed: the joiner gets the embedded community snapshot, the iroh handshake delivers the countersign, both sides are consistent *at join time*. **Ongoing** cross-WAN messages still won't flow until Spec B (Zenoh-over-iroh ingestion) lands — inbound iroh-zenoh links are currently discarded (lib.rs:2544-2552). The two ship in order: A (join) → B (chat).
