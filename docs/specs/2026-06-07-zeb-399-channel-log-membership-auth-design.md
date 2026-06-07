# ZEB-399: channel-log message author auth via materialized enrolled device keys

**Status:** approved (Jake, 2026-06-07) — "unify on membership"
**Ticket:** [ZEB-399](https://linear.app/zeblith/issue/ZEB-399) (Urgent, parent ZEB-217, blocks ZEB-366 + ZEB-330)
**Lineage:** completes [ZEB-339](https://linear.app/zeblith/issue/ZEB-339) (which moved *membership* verify off the owner-identity resolver) for the channel-log data plane. Sibling of ZEB-340.

## Problem

After a community invite redeem (no prior DM relationship), each node drops the
other's channel messages with `UnknownAuthor(OwnerAddr(..))`, proven symmetric on
both Koya and Ildwyn logs (community `4d3ca331`, channel `c4c840f2`). Membership
root sync already works both ways post-ZEB-398 (`outcome=Mutated`); the *only*
remaining failure is channel-log author resolution.

Root cause: `verify_channel_event` resolves the author via
`OwnerDeviceCacheResolver`, which reads the **DM-layer** `owner_device_cache`.
That cache is populated only by owner-state sync / DM handshake — never on a
community-invite first contact — so `resolve()→None` → `UnknownAuthor`, before
the signature is even checked.

Two identity/auth regimes coexist in one community:

| Path | Signs with | Verifies against |
|---|---|---|
| Membership + root publishes (post-ZEB-339) | enrolled device key #2 | materialized `member.enrolled_device_keys` (self-bootstraps from the inbound Join's EnrollmentCert) |
| Channel-log (and voting) | device #1 / owner identity key (`signing_key_arc`) | owner's 64-byte identity resolved from the DM `owner_device_cache` |

Only the first self-bootstraps on join. Two facts force the fix shape:
1. Membership stores only the **32-byte ed25519** device key, not the 64-byte
   `X25519‖Ed25519` composite — so the inbound peer's full identity isn't
   available to repopulate the DM cache.
2. `PubKeyBundle.classical.x25519_pub` is a **zeroed stub** in production
   (ZEB-372) — the X25519 half is fake everywhere.

The channel `author` is the **owner address** but the signing key is a **device
key**; a resolver returning a 64-byte composite bound by `address_hash == author`
structurally cannot validate a device-key signature. The only sound model is
membership's: "is this device key enrolled under this owner?"

Why it slipped: channel-log tests use a `FixedIdentityResolver`/`SharedResolver`
pre-populated with the right identities, masking the production gap. Public
avatars (PublicDurable, no author gate) masked it through ZEB-343.

## Fix

Unify channel-log author auth onto the community membership trust root.

1. **Verify side** — rework `verify_channel_event` to verify the post signature
   against the author's materialized `enrolled_device_keys` **at the event's
   HLC** (mirroring `verify_publisher_sig`), removing the `ChannelIdentityResolver`
   dependency. `CommunityStateAtHlc::snapshot_at` already materializes membership
   at `at`; extend `CommunityStateSnapshot` with the author's enrolled keys
   sourced from that same single materialization (preserves the torn-read
   guarantee). The channel-config + power gate is unchanged.
   - Chain order: misroute → replay (cheap, unchanged) → snapshot → membership
     gate (channel exists/not-deleted, author Joined, power ≥ write_power) →
     signature verify against enrolled keys → record.
   - Errors: author not Joined → `NotAuthorized`; member with empty enrolled set
     → `UnknownAuthor` (anomaly diagnostic); no key verifies → `BadSignature`.

2. **Sign side** — the channel-log registry signs posts with the **enrolled
   device key #2** (`community_signing_key_arc`), matching what membership trusts.
   Production currently wires `signing_key_arc` (device #1) at `lib.rs:3199`.

3. **Scope** — channel-log only. Voting (`VotingIdentityResolver` on the same
   `OwnerDeviceCacheResolver`) has the identical latent bug, unexercised on the
   first-contact path; left as a sibling to ZEB-340.

Wire format of `SignedChannelEvent::Post` is unchanged (64-byte ed25519 sig over
the same canonical CBOR) → no event fixture changes.

## Affected code

- `community_channel_log.rs` — `verify_channel_event` signature + body; add
  `author_enrolled_keys` to `CommunityStateSnapshot`; delete the
  `ChannelIdentityResolver` trait; migrate unit tests + `MockState`.
- `community_channel_log_engine.rs` — drop the `resolver` field/param across
  `ChannelLogEngineParams` / `ChannelLogEngine` / `DeferredSpawn` / `spawn`;
  drop the verify-call resolver arg; migrate `AlwaysJoinedState` + fixtures.
- `community_state_sync.rs` — `CommunityStateAtHlcAdapter::snapshot_at` surfaces
  enrolled keys; delete `ChannelIdentityResolverAdapter` + the engine
  `identity_resolver()` accessor (channel-log was its only consumer).
- `lib.rs` — registry `signing_key`: `signing_key_arc` → `community_signing_key_arc`;
  drop the spawn-time resolver wiring (`~17045-17050`).
- `tests/community_channel_messages_integration.rs` — migrate to membership model
  (delete `SharedResolver`, `BothJoinedState` returns enrolled keys). This is the
  cross-node regression anchor.

## Test plan (TDD)

1. **Bug repro (unit):** `verify_channel_event` accepts a post signed by an
   enrolled **device** key (≠ owner identity key) when the author is a Joined
   member whose snapshot carries that device key — fails under the old
   resolver model, passes after the fix.
2. **Negative (unit):** post signed by a non-enrolled key → `BadSignature`;
   non-member author → `NotAuthorized`/`UnknownAuthor`.
3. **Cross-node (integration):** the migrated two-engine test — A posts, B
   admits via materialized enrolled keys with **no resolver / no DM cache**;
   live + offline-backfill + replay-rejection all still pass.
4. **Live:** Koya↔Ildwyn re-test (diagnostics branch carries ZEB-398 + this) →
   both render each other's messages → closes ZEB-366 + ZEB-330 DoD#3.
