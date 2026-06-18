# ZEB-369: Targeted invite-only invites — design spec

**Status:** approved direction (Jake, 2026-06-18) — "seal to ALL known devices".
**Ticket:** [ZEB-369](https://linear.app/zeblith/issue/ZEB-369) (follow-up to ZEB-367 untargeted invite-only).
**Branch:** `zeb-369-targeted-invite-only` off `main@1d6bd4de` (ZEB-401 merged → `MAX_ENROLLED_DEVICE_KEYS` cap present).

---

## Goal

Let an admin generate an **invite-only** community invite **targeted at a specific invitee** (`invitee_hint = Some(addr)`), where the epoch key is sealed to the invitee's enrolled device key(s) so only that invitee can redeem — and they can redeem on **any** of their bound devices. Today `generate_invite` *rejects* a targeted hint on invite-only communities (`invite_only_generation_guard`), pointing here.

This is orthogonal to cross-WAN *first contact* (the untargeted path, ZEB-367, ships an ephemeral decrypt key in the URL). "Targeted" requires the inviter to already know the invitee's device key(s) — i.e. the invitee is a visible `Joined` member of some community the inviter shares with them.

## Background — the device-key correction

harmony devices carry three ed25519 keys. The two that matter:

- **#2 enrolled device signing key** — what `mint_redemption` decrypts a sealed epoch key with, via `ed25519_priv_to_x25519(signing_key)`. Its public is `cert.device_pubkeys.classical.ed25519_verify`, and the birational X25519 to seal to is `ed25519_pub_to_x25519(that)`.
- **#3 Reticulum identity key** — stored in `OwnerDeviceCache`; sealing to it **never decrypts** (redemption uses #2). This is why ZEB-367's "resolve #2 X25519 from `OwnerDeviceCache`" was not implementable, and why this work was split out.

The invitee's #2 verify key is recorded in `MemberState.enrolled_device_keys: BTreeSet<[u8;32]>` (populated by `materialize` from each Join/DeviceAnnounce's `EnrollmentCert`). After ZEB-495, a member can have **N>1** enrolled device keys; after ZEB-401, that set is bounded by `MAX_ENROLLED_DEVICE_KEYS` (32).

**ZEB-372 is NOT a blocker** (verified in code, not just ticket status): the entire seal→redeem path derives X25519 from the ed25519 verify key via `ed25519_pub_to_x25519`; it never reads `PubKeyBundle.x25519_pub`.

## Design overview

Four components, all in `src-tauri/`:

1. **Resolver** — `OwnerAddr` → the invitee's enrolled device-#2 verify keys, by scanning the materialized membership of every community the inviter belongs to.
2. **Wire-format extension** — an additive, optional repeated field on `InviteEpochSnapshot` carrying one sealed envelope per device. Untargeted/open invites encode byte-identically (back-compat).
3. **`generate_invite` targeted branch** — resolve → seal-to-all → set `invite_token.invitee_hint`.
4. **`mint_redemption` try-all** — iterate the new field, try each envelope with the device key until one opens; fall back to the single `sealed_epoch_key` when the field is absent (untargeted/legacy/open).

### Multi-device decision (settled)

**Seal to ALL the invitee's known enrolled device keys** — one envelope per key — so the invitee can redeem on any bound device (matches ZEB-169 "all my devices are me"). Rejected alternatives: a device-hint (awkward, failure-prone) and newest-device-only (silent "wrong device" footgun). Payload grows one ~92-byte envelope per device, bounded by the union of `enrolled_device_keys` across shared communities (each capped at 32).

---

## Component 1 — Resolver

**New fn** in `lib.rs` (near `generate_invite_impl`), signature approximately:

```rust
/// ZEB-369: collect the invitee's enrolled device-#2 ed25519 verify keys by
/// scanning the materialized membership of every Community the inviter belongs
/// to. Union across communities (an invitee may have enrolled different devices
/// in different communities; seal to every one we can see). Returns the keys
/// sorted+deduped; empty when the invitee is not a visible Joined member anywhere.
fn resolve_invitee_device_keys(
    crdt_state: &OwnerStateCrdt,          // already locked in generate_invite_impl
    community_registry: &CommunityRegistry, // source of per-community events
    inviter_admin_addr: OwnerAddr,
    invitee_addr: OwnerAddr,
) -> BTreeSet<[u8; 32]>
```

Behavior:
- Enumerate `crdt_state.spaces` filtered to `SpaceKind::Community`.
- For each, materialize membership (`materialize(events, admin_addr)`); the admin_addr per community is that community's admin (already available from the space / registry — mirror how `generate_invite_impl` obtains it at `lib.rs:20259-20277`).
- Look up `materialized.members.get(&invitee_addr)`; if `status == MemberStatus::Joined`, union in its `enrolled_device_keys`.
- Return the union (`BTreeSet`, naturally deduped & bounded by distinct devices).

Notes:
- The target community (the one the invite is *for*) is included in the scan but the invitee is typically not yet a member there — that's expected; resolution comes from *other* shared communities.
- This only sees devices that actually joined a shared community. Devices the inviter has never seen cannot be sealed to (documented limitation, not a bug).

## Component 2 — Wire-format extension

Add to `InviteEpochSnapshot` (`community_invite.rs:33`), keeping the 2-char canonical-CBOR key convention:

```rust
/// ZEB-369: targeted invite-only invites seal the epoch key to EACH of the
/// invitee's enrolled device-#2 X25519 keys — one 92-byte envelope per device —
/// so the invitee can redeem on any bound device. Empty for open + untargeted
/// invites (those carry the single `sealed_epoch_key`); skip_serializing_if keeps
/// their encoded wire byte-identical. When non-empty, redemption tries each
/// envelope with `ed25519_priv_to_x25519(device_sk)` until one opens, and
/// `sealed_epoch_key` is left empty.
#[serde(
    rename = "se",
    default,
    skip_serializing_if = "Vec::is_empty",
    serialize_with = "crate::owner_state_types::serialize_vec_of_vec_as_bstr",
    deserialize_with = "crate::owner_state_types::deserialize_vec_of_vec_from_bstr"
)]
pub sealed_epoch_keys: Vec<Vec<u8>>,
```

- **Key**: `"se"` (2 chars; distinct from `ep`/`sk`/`ss`). Confirm at impl time no test asserts a uniform key-length set beyond 2 chars.
- **New serde helper pair** `serialize_vec_of_vec_as_bstr` / `deserialize_vec_of_vec_from_bstr` in `owner_state_types.rs`, mirroring the existing single-`Vec<u8>` `serialize_vec_as_bstr`/`deserialize_vec_from_bstr` — encodes a CBOR **array of byte-strings** (compact; no per-envelope int-array bloat).
- `sealed_epoch_key` (`sk`) keeps its current attributes unchanged → untargeted/open wire unchanged.

## Component 3 — `generate_invite` targeted branch

In `invite_only_generation_guard` (`lib.rs:20572`): **remove** the `invitee_hint.is_some()` rejection; **keep** the `self_owner == admin` (admin-only) check.

In `generate_invite_impl`'s `is_invite_only` block (`lib.rs:20296-20372`), branch on `invitee_hint`:

- **`Some(hint_hex)`** (targeted):
  1. Decode `hint_hex` → `OwnerAddr` (mirror the `community_id` decode at `lib.rs:20191`).
  2. `let device_keys = resolve_invitee_device_keys(...)`.
  3. If `device_keys.is_empty()` → return the shipped error text:
     `"can't target {addr}: their devices aren't known yet — use an untargeted link"` (addr hex-encoded).
  4. Seal the epoch key to each: for `ed in device_keys { let x = ed25519_pub_to_x25519(&ed)?; seal_to_owner(&x, mk.as_bytes())? }` → `Vec<Vec<u8>>` (mirrors `build_sealed_epoch_recipients` at `lib.rs:28475`).
  5. Mint the token **with** `invitee_hint = Some(invitee_addr)` (`mint_invite_token(self_owner, Some(invitee_addr), minted_at, expiry, &community_signing_key)`).
  6. Produce the snapshot with `sealed_epoch_key = vec![]`, `sealed_epoch_keys = envelopes`, `untargeted_decrypt_key = None`.
- **`None`** (untargeted): unchanged from ZEB-367 (`seal_epoch_key(.., Untargeted)`; `sealed_epoch_keys = vec![]`).

The shared tail (`CommunityInvitePayload` build at `lib.rs:20469`, `encode_invite_url`, pkarr case-A registration) is unchanged; targeted sets `untargeted_decrypt_key: None`.

## Component 4 — `mint_redemption` try-all

In `mint_redemption`'s invite-only branch (`lib.rs:22112-22143`):

```rust
let x25519_priv = match payload.untargeted_decrypt_key {
    Some(eph) => Zeroizing::new(eph),
    None => ed25519_priv_to_x25519(signing_key),
};

// ZEB-369: a targeted invite carries one envelope per invitee device in
// `sealed_epoch_keys`. Try each with our device key until one opens. An
// untargeted/legacy invite leaves it empty → fall back to the single blob.
let candidates: Vec<&[u8]> = if !payload.epoch_snapshot.sealed_epoch_keys.is_empty() {
    payload.epoch_snapshot.sealed_epoch_keys.iter().map(|v| v.as_slice()).collect()
} else {
    vec![payload.epoch_snapshot.sealed_epoch_key.as_slice()]
};
// length-guard each (SEALED_MIN) before open; succeed on first open.
let plaintext = candidates.iter()
    .filter(|c| c.len() >= SEALED_MIN)
    .find_map(|c| open_from_owner(&x25519_priv, c).ok())
    .ok_or_else(|| "invite-only epoch key decryption failed: no envelope opened with this device key".to_string())?;
```

Open-community path (raw 32-byte `sealed_epoch_key`) is unchanged (`sealed_epoch_keys` is always empty there).

## Component 5 — defense-in-depth invitee binding (secondary)

`invite_token.invitee_hint` is signed (tamper-proof) but the seal is the real access gate. As a clearer-error nicety: in `mint_redemption`, if `invite_token.invitee_hint == Some(h)` and `h != self_owner`, return `"this invite was issued for a different owner"` *before* attempting decrypt. Untargeted tokens (`invitee_hint = None`) skip this, so no behavior change for existing paths. Keep this small; the cryptographic gate is the seal.

---

## Data flow

```
generate_invite(community_id, invitee_hint=Some(bob), ...)            [admin = alice]
  └─ resolve_invitee_device_keys(alice's communities, bob) = {bob_d1, bob_d2}
  └─ for each: ed25519_pub_to_x25519 → seal_to_owner(epoch_key) → [env1, env2]
  └─ InviteEpochSnapshot { sealed_epoch_key: [], sealed_epoch_keys: [env1, env2], .. }
  └─ invite_token.invitee_hint = Some(bob);  untargeted_decrypt_key = None
  └─ encode_invite_url → harmony://invite/<b64url>   (+ pkarr case-A record)

redeem(url)                                                          [joiner = bob, device d2]
  └─ verify token sig; (opt) check invitee_hint == bob
  └─ x25519_priv = ed25519_priv_to_x25519(bob_d2_sk)
  └─ try open(env1) → fail; try open(env2) → OK → epoch_key
  └─ join proceeds (unchanged)
```

## Back-compat & wire fixtures

- Untargeted/open invites: `sealed_epoch_keys` empty → skipped → **byte-identical** to today. Existing `wire_format_*` invite fixtures unchanged.
- **New fixture**: pin the targeted `InviteEpochSnapshot` shape (`sealed_epoch_key` empty, `sealed_epoch_keys = [env_a, env_b]`) — deterministic envelopes via the `test-fixtures` deterministic-nonce seal helper so the CBOR is stable.
- **Back-compat decode test**: an old-format snapshot CBOR (only `ep`/`sk`/`ss`) decodes with `sealed_epoch_keys` defaulting to empty (proves `#[serde(default)]`).

## Error handling

- Unresolvable invitee → the shipped `"can't target … use an untargeted link"` Err (no silent downgrade to a weaker untargeted invite).
- Non-admin generating invite-only → existing `"only the admin can generate invite-only invites (v1)"`.
- Redeem with no opening envelope → clear "no envelope opened with this device key" Err.
- `ed25519_pub_to_x25519` rejects small-order points → propagate (a corrupt enrolled key fails generation loudly).

## Testing

- **Unit (resolver)**: invitee `Joined` in one community → returns its keys; invitee in two communities with different devices → union; invitee absent/`Left` → empty; multi-device member → all keys.
- **Unit (guard)**: targeted hint on invite-only no longer rejected; non-admin still rejected. Update the existing reject/allow test at `lib.rs:46464`.
- **Unit (redeem try-all)**: snapshot with 2 envelopes, device key matches the 2nd → opens; matches none → clear Err; untargeted fallback still opens the single blob.
- **Wire**: new targeted fixture + back-compat decode test (above).
- **E2E** (`tests/pkarr_iroh_redeem_full_integration.rs`): `targeted_invite_only_generate_then_redeem_roundtrip` mirroring `invite_only_untargeted_generate_then_redeem_roundtrip` (:1040) and `bob_joins_alice_via_iroh_handshake_option_a` (:718) — but seal to the key resolved from materialized membership (seed Bob as a Joined member of a shared community first), and redeem with Bob's real device key. Assert `outcome.status == "joined"`. Add a multi-device variant if `mint_second_device`/`make_device_announce` helpers make it cheap (seal to 2, redeem on the 2nd).

## Gates (CI parity)

```
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

## Out of scope (YAGNI)

- Re-keying `OwnerDeviceCache` onto device #2 (that's ZEB-340 §1).
- A dedicated "learned cert" store beyond scanning materialized membership.
- Targeting an invitee the inviter has never shared a community with (cryptographically impossible without their key; the untargeted link is the answer).
- Un-gating any other invite path; only the targeted invite-only branch changes.
