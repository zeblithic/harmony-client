# ZEB-341: Resolvable per-identity profile cards (display name + status) by `owner_id`

**Status:** Design approved 2026-05-30. Implementation pending.
**Linear:** [ZEB-341](https://linear.app/zeblith/issue/ZEB-341) (parent ZEB-218 Sub-D; related ZEB-281, ZEB-339, ZEB-217).
**Branch:** `zeb-341-profile-cards` (off `origin/main` `6aebf2f`, which includes the ZEB-339 merge).

## 1. Problem

In a community, the MEMBERS list and message authors render every member as a raw `owner_id` hex prefix (e.g. `685e4ba7`) — including the viewer's own row. The `ProfileEditor` ("Your Profile") panel already has a working DISPLAY NAME + STATUS field, but the value neither shows for the self member nor resolves for other members, and members are not clickable to view a profile.

**Root cause — an identity-space mismatch.** The existing profile system (`ProfileEditor`, `publish_profile`, `ProfileMembershipBroadcast`, `ProfilePopover`) is keyed to the **Reticulum identity address** (`address_hash` = `SHA256(X25519‖Ed25519)[:16]`, the `097364…` shown as "Address" in the profile panel). Community members are keyed by the harmony-owner **`owner_id`** (master hash = `SHA256(canonical_cbor{ed25519_verify, ml_dsa_verify})[:16]`, the `685e4ba7…` in the member list). These are two different 16-byte identities for the same person — the exact split ZEB-339 resolved for membership events.

Concretely today:
- `member_info_for()` (`src-tauri/src/lib.rs:11407`) hardcodes `display_name: None`.
- `MemberRow.svelte:86` → `member.displayName ?? member.address.slice(0, 8)`.
- `ChannelMessageFeed.svelte:350` → `msg.author.slice(0, 8)`.
- There is **no `owner_id → profile` resolution path**. `ProfileMembershipBroadcast` (`profile_broadcast.rs`) verifies to a Reticulum `address_hash` (`verify_broadcast`, line 158), not an `owner_id`.

## 2. Goal & non-goals

**Goal:** a member's **display name and status text** resolve from their `owner_id` and render in the members list, message authors, and a click-to-view profile popover — for the self member (immediately, offline) and for other members (cross-peer, verified). Editing already exists in `ProfileEditor`; this work makes the value *propagate and resolve*.

**Design decisions (brainstormed + approved 2026-05-30):**
- **Global per-identity** profile (one name/status per `owner_id`, the same everywhere), **not** per-community names in the membership CRDT.
- **Name + status only** this cut. No avatar/CAS content (see §9 for the deliberately-reserved extension path).
- **Cryptographically bound to `owner_id`** via the ZEB-339 `EnrollmentCert` model — a peer must not be able to publish a card under another owner's `owner_id`.

**Non-goals (this cut):** avatars / custom images (the generated identicon stays); per-community display-name override; disk-persisted card cache (in-memory only; cold start re-resolves); bridging `ProfilePopover`'s existing Reticulum-keyed "shared communities" section by `owner_id`; the separate "● refused" device-trust badge (ZEB-342).

## 3. Architecture overview

A new **`owner_id`-keyed, `EnrollmentCert`-verified profile-card broadcast**, mirroring the proven `ProfileMembershipBroadcast` machinery (`profile_broadcast.rs` + the `subscribe_peer_profile`/`get_cached_peer_profile` subscriber-pool + cache trio at `lib.rs:18541+`), but keyed by `owner_id` and verified through the cert model instead of a Reticulum `address_hash`.

```text
Publisher (you)                         Subscriber (another member)
─────────────                           ───────────────────────────
ProfileEditor save ──┐
startup / periodic ──┤
                     ▼
            ProfileCardBroadcast
        { owner_id, display_name,
          status_text, enrollment,
          shared_at, signature(dev#2) }
                     │  publish on
                     ▼  harmony/discovery/profile/owner/{owner_id_hex}/card
                  [Zenoh] ───────────────────▶ subscriber pool (per visible member owner_id)
                                                       │ verify via cert model (§5)
                                                       ▼
                                                ProfileCardCache  (owner_id → {name, status, hlc})
                                                       │ IPC get_cached_member_card
                                                       ▼
                                           member-card-service (reactive owner_id → {name,status})
                                              ├─ seeds SELF from local profile (offline)
                                              └─ overlays MemberRow / ChannelMessageFeed / ProfilePopover
```

## 4. Wire type — `ProfileCardBroadcast`

New module `src-tauri/src/profile_card_broadcast.rs` (sibling to `profile_broadcast.rs`; kept separate so the two broadcast families have one responsibility each).

```rust
pub const PROFILE_CARD_TOPIC_PREFIX: &str = "harmony/discovery/profile/owner/";
// full topic: {PREFIX}{owner_id_hex}/card   (owner_id_hex = lowercase 32-char hex of [u8;16])

pub const MAX_DISPLAY_NAME_BYTES: usize = 64;
pub const MAX_STATUS_TEXT_BYTES: usize = 128;
pub const MAX_CARD_WIRE_BYTES: usize = 4_096; // sanity bound before CBOR decode

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCardBroadcast {
    // owner_id / signature use the existing byte-string serde helpers
    // (serialize_bytes_as_bstr / deserialize_bytes_from_bstr) — the same
    // ones ProfileMembershipBroadcast uses for its 64-byte fields.
    #[serde(rename = "oi", serialize_with = "...as_bstr", deserialize_with = "...from_bstr")]
    pub owner_id: [u8; 16],
    #[serde(rename = "dn")] pub display_name: String,   // ≤ MAX_DISPLAY_NAME_BYTES
    #[serde(rename = "st")] pub status_text: String,    // ≤ MAX_STATUS_TEXT_BYTES
    #[serde(rename = "en")] pub enrollment: EnrollmentCert,
    #[serde(rename = "sa")] pub shared_at: Hlc,
    #[serde(rename = "sg", serialize_with = "...as_bstr", deserialize_with = "...from_bstr")]
    pub signature: [u8; 64],
}
```

- Canonical CBOR (the existing `canonical_cbor_encode` + `CanonicalPayload`/`CanonicalPayloadSealed` markers), 2-char serde field codes (`oi`, `dn`, `st`, `en`, `sa`, `sg`) consistent with the codebase convention. Byte arrays use the existing `crate::owner_state_types::serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` helpers (shown abbreviated above).
- `enrollment` reuses `harmony_owner::certs::EnrollmentCert` (the same type ZEB-339 put on `SignedMembershipEvent`).
- `shared_at` HLC gives newer-wins rotation, mirroring `ProfileMembershipBroadcast.shared_at`.
- **Forward-compat (see §9):** the struct is a CBOR map of named optional/required fields; adding future `Option<[u8;32]>` CAS fields is an additive, non-breaking change. No fixed positional layout.

## 5. Verification — cert model (mirrors ZEB-339)

`verify_card(card) -> Result<[u8;16] /* owner_id */, CardVerifyError>`:

1. **Bounds:** `display_name.len() ≤ MAX_DISPLAY_NAME_BYTES`, `status_text.len() ≤ MAX_STATUS_TEXT_BYTES`; reject otherwise.
2. **Cert validity:** `card.enrollment.verify()` (checks `hash(master_pubkey) == owner_id` + master signature) AND **Master-issuer-only** gate (reject `EnrollmentIssuer::Quorum`; the community/discovery path cannot fully verify Quorum sigs — identical to ZEB-339 spec §10 / `enrolled_key_from_cert`).
3. **Owner binding:** `card.enrollment.owner_id == card.owner_id`.
4. **Signer key:** device ed25519 = `card.enrollment.device_pubkeys.classical.ed25519_verify`.
5. **Signature:** `ed25519 verify_strict` of the canonical CBOR (with `signature` zeroed) under that device key.
6. **Attribution (subscriber-side):** the caller (subscriber pool `process_sample`) checks the returned `owner_id` equals the topic's `owner_id` — a card delivered on `…/{X}/card` must carry `owner_id == X`.

This reuses ZEB-339's cert-check logic. Implementation note: factor the shared cert→device-key check into a small helper (e.g. `harmony_owner` cert verification wrapper) called by both `community_membership::enrolled_key_from_cert` and `verify_card`, OR replicate the 4 checks if extraction is awkward — a light cleanup, not a refactor of the membership path.

`CardVerifyError` variants: `DisplayNameTooLong`, `StatusTextTooLong`, `EnrollmentCertInvalid`, `EnrollmentOwnerMismatch`, `SignatureInvalid`, `Encode(..)` (mirroring `BroadcastVerifyError` + `community_membership::VerifyError`).

## 6. Publish path

- **Sign** with **device key #2** (`community_signing_key`) and attach the owner's **`EnrollmentCert`** — both already loaded into the runtime by the ZEB-339 startup wiring (`DmOutbox.community_signing_key` + `enrollment_cert`; `lib.rs` start_node). No new key plumbing.
- **Triggers:** (a) on `ProfileEditor` save (the existing `publish_profile` IPC path), (b) once at startup after owner load, (c) periodic refresh — mirroring `ProfileBroadcastPublisher`'s debounce (`PUBLISHER_DEBOUNCE`, 2s) + refresh (`PUBLISHER_REFRESH_INTERVAL`, 600s) state machine.
- **HLC monotonic** via the existing HLC source so a newer card supersedes an older one at every subscriber.
- **Bounds enforced at publish** (reject over-length name/status before signing) so a card that would fail `verify_card` never leaves the node.
- The existing `publish_profile` Reticulum-topic publish is **retained** (DM/nav back-compat); we **add** the `owner_id`-card publish reading the same local profile fields (`display_name`, `status_text`).

## 7. Resolution & cache

**Backend** (mirror `subscribe_peer_profile`/`unsubscribe_peer_profile`/`get_cached_peer_profile` + the event-loop `ProfileBroadcastRequest` subscriber pool):
- `ProfileCardCache`: `owner_id → DiscoveredCard { display_name, status_text, shared_at }`, newer-HLC-wins, populated only by **verified** cards (`verify_card` runs in `process_sample` before insert; drop + warn on failure).
- IPCs: `subscribe_member_card(owner_id_hex) -> subscription_id`, `get_cached_member_card(subscription_id) -> Option<DiscoveredCardInfo>`, `unsubscribe_member_card(subscription_id)`. Subscriptions are owner-loaded-gated (`OWNER_NOT_LOADED_MSG`) like the existing trio.
- A dedicated `ProfileCardRequest` channel + subscriber pool task (or a generalized reuse of the existing pool keyed by topic) in `event_loop.rs`.

**Frontend** — new `member-card-service.ts` (sibling to `profile-broadcast-service.ts`):
- When the members panel / channel view renders, **eagerly** `subscribe_member_card` for each *visible* member's `owner_id`; expose a reactive `Map<ownerIdHex, { displayName, statusText }>`.
- **Seed `self` synchronously** from the local profile (`profile-service`) — no network — so the viewer's own row + messages render immediately.
- Poll/receive cached cards (same cadence pattern the existing profile-broadcast-service uses) and update the map.
- **Unsubscribe on unmount / view change** to bound the active subscription count.

## 8. Render touchpoints (frontend overlay)

Resolution is a **frontend overlay** (same shape as `DevicesPanel` overlaying the local device name), keeping the backend `MemberInfoDto` identity-only:
- `MemberRow.svelte:86` — populate `member.displayName` from the card map (falls back to `owner_id.slice(0,8)` while unresolved).
- `ChannelMessageFeed.svelte:350` — resolve `msg.author` (owner_id) through the same map.
- **Clickable:** the member-row name and the message-author become buttons opening `ProfilePopover` in a new **`owner_id`-card variant** showing display name, status, copyable `owner_id`, and the member's role/power **in this community** (already available in `MemberInfoDto`). The popover's existing Reticulum-keyed "shared communities" section is **omitted** in this variant (different identity key — deferred).
- `member_info_for()` (`lib.rs:11407`) stays `display_name: None` — the frontend overlay owns name resolution for one consistent path (self via local-profile seed, others via the card map). The backend `MemberInfoDto` remains identity-only.

## 9. CAS extensibility (forward-looking — reserved, NOT implemented)

The signed card is the **carrier** for richer profile content via Harmony's content-addressed storage. Future additive, serde-optional fields on the **same** `ProfileCardBroadcast`:

| Field (future) | Type | Meaning |
|---|---|---|
| `avatar_cid` | `Option<[u8;32]>` (serde `av`) | Profile picture as a single CAS object; receiver fetches the blob by 256-bit CID, caches by `owner_id`. |
| `profile_page_root` | `Option<[u8;32]>` (serde `pp`) | Long-form "personal webpage" profile as the 256-bit **root** of a CAS bundle/collection. |

**Properties this design guarantees so the above is "the same shape of effort":**
- Canonical-CBOR named-field encoding → adding optional fields is **non-breaking** (serde-default absent; old cards decode, new cards verify unchanged since the signature covers whatever fields are present).
- The card already carries the verified `owner_id` binding → an avatar/page CID inherits the same authenticity (the publisher signed the CID into the card).
- Resolution machinery (subscribe-by-`owner_id` + cache) is reused; only a CAS **blob fetch by CID** is added on top.

**Why deferred & why it matters:** Harmony's CAS has **not** yet had a successful peer-to-peer test. Avatar support is the natural **forcing-function** to prove CAS p2p end-to-end; doing it before CAS-p2p is validated would couple this feature to an unproven subsystem. This cut reserves the field slots + documents the path; a follow-up ticket implements avatar-via-CAS once CAS-p2p is demonstrated.

## 10. Error handling

- **Publish:** bounds-check before sign; if device key/cert are somehow unavailable (should be impossible post-owner-load), skip the card publish and log — never panic, never block startup.
- **Verify:** every received card runs `verify_card`; failures are dropped with a `warn!` (never cached, never surfaced) — a hostile peer publishing junk on a topic cannot poison the cache or crash the subscriber.
- **Resolution UX:** unresolved members render the `owner_id` prefix (current behavior) — graceful, never blank. The popover shows "name unavailable" gracefully if a card never arrives.
- **Subscription teardown races** mirror the existing trio's rollback (`drop_subscription` on `request_tx` send failure).

## 11. Testing

- **Wire-format fixture** pinning `ProfileCardBroadcast` canonical bytes (new `tests/wire_format_profile_card_fixtures.rs`), incl. a certless/old-shape decode for forward-compat confidence.
- **Verify-path negatives:** invalid cert, non-Master issuer (Quorum), `cert.owner_id ≠ card.owner_id`, tampered signature, oversize `display_name`/`status_text`, topic-attribution mismatch.
- **Publish:** HLC-monotonicity / newer-wins; bounds rejection before sign; publishes on save + startup.
- **Cross-peer e2e** (analogous to ZEB-339's cross-owner test): owner A publishes a card; owner B subscribes by A's `owner_id`, verifies, and reads A's name/status from the cache.
- **Frontend (vitest):** `member-card-service` resolution + self-seed + reactive overlay; `MemberRow`/`ChannelMessageFeed` render the resolved name; clickable opens the popover; unsubscribe on unmount.
- All gates green: `cargo fmt`/`clippy`/`nextest`/large-tests/MSRV + `tsc`/`vitest`.

## 12. File map

**Backend (new):** `src-tauri/src/profile_card_broadcast.rs` (wire type, sign, `verify_card`, publisher state machine, cache); `tests/wire_format_profile_card_fixtures.rs`; cross-peer e2e test.
**Backend (modified):** `lib.rs` (3 new IPCs + `NodeState` card-cache/request-tx fields + register publisher at start_node + add owner_id-card publish to `publish_profile`); `event_loop.rs` (`ProfileCardRequest` + subscriber-pool task); possibly a shared cert-check helper touch in `community_membership.rs`.
**Frontend (new):** `src/lib/member-card-service.ts`; tests.
**Frontend (modified):** `MemberRow.svelte`, `ChannelMessageFeed.svelte`, `ProfilePopover.svelte` (owner_id-card variant), the members panel + channel view wiring, `App.svelte` (popover open wiring), `community-service.ts`/types as needed.

## 13. Sequencing note (for the plan)

The **self-first** slice (local-profile seed → the viewer's own row + messages show their name, zero network) is a small, standalone, immediately-visible early task. Order it first; the cross-peer broadcast (wire type → verify → publish → subscribe/cache → cross-peer overlay) builds out from there, with the clickable popover last.
