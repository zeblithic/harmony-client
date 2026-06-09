# Multi-device fleet sync + always-on butler — top-level framing

**Status:** Framing approved 2026-06-09 (design gate). Sub-project specs to follow.
**Epic:** [ZEB-416](https://linear.app/zeblith/issue/ZEB-416) — Multi-device fleet sync + always-on butler (async store-and-forward)
**Sub-projects:** [ZEB-417](https://linear.app/zeblith/issue/ZEB-417) SP1 Fleet Sync · [ZEB-418](https://linear.app/zeblith/issue/ZEB-418) SP2 Butler (blocked by [ZEB-372](https://linear.app/zeblith/issue/ZEB-372))
**Prior art consulted:** Gemini Deep Research report on personal always-on relays + multi-writer fleet sync (Briar Mailbox, Matrix ReP2P/Dendrite, Delta Chat, Session/Oxen, SSB, Berty/Wesh, Veilid; Willow/iroh-docs, Automerge/Yjs, Earthstar).

This document frames the whole epic and fixes the boundary between the two sub-projects so each can be spec'd independently. It deliberately stops short of sub-project-level mechanism choices (reconciliation engine, envelope format, rendezvous rotation) — those belong in the SP1/SP2 specs, with leans recorded here.

---

## 1. Goal

Make an owner's fleet of devices behave as one. Two coupled capabilities:

- **Fleet Sync** — data written on any device follows the owner to all their devices and survives the loss of any single device.
- **The Butler** — at least one online device receives/holds deliveries on the owner's behalf, so two people who are never online at the same time can still exchange messages — with no depot, server, or third-party escrow.

The cryptographic foundation already ships: owner→device binding ([ZEB-173](https://linear.app/zeblith/issue/ZEB-173), `harmony-owner`), LAN pairing ([ZEB-197](https://linear.app/zeblith/issue/ZEB-197)), device admin UI ([ZEB-170](https://linear.app/zeblith/issue/ZEB-170)), keychain-at-rest ([ZEB-363](https://linear.app/zeblith/issue/ZEB-363)). This epic is the layer above that.

## 2. Terms

| Term | Meaning |
|---|---|
| **Owner** | the human; one canonical identity (`harmony-owner`) |
| **Device** | one bound instance, its own keypair + `EnrollmentCert` under the owner |
| **Fleet** | all of an owner's bound devices |
| **Butler** | a *role*, not a node: whichever of my devices is online right now, acting as the owner's reachable endpoint. Identical code on every device |
| **Butler-set** | the currently-reachable subset of my fleet, advertised so peers know where to deposit |
| **Relay** | an opt-in third party (a co-community always-on member) that holds *sealed* bytes it cannot read, only when first-party delivery fails |

## 3. Framing decisions (approved)

1. **Relay trust — first-party by default + opt-in sealed fallback.** Content only ever rests on your own devices or the recipient's. An opt-in, **community-scoped** third-party relay is the availability fallback when two fleets never overlap online; it holds ciphertext it cannot read and learns only minimal metadata.
2. **Butler payload scope — DMs + community posts** in v1 (1:1 and group DMs; offline community members' posts land and backfill). Mail and arbitrary CAS blobs are later.
3. **Always-on assumption — configurable.** Design for best-effort ("whichever of my devices is online is the butler") and degrade gracefully to today's sender-retry when the whole fleet is offline; allow power users to pin a genuinely always-on node.
4. **Relay scoping — per-community.** A community you are both in supplies the always-on volunteer relay; trust is community-scoped, consistent with Harmony's polycentric governance model (communities are the only first-class trust/moderation primitive).

## 4. Requirements & success criteria

**Functional**
- A note / DM / read-marker written on device A appears on devices B…N, offline-tolerant, deterministic merge. *(Fleet Sync)*
- Two people never online at the same time still exchange DMs + community posts, no depot. *(Butler)*
- Losing a device loses no data **that had reached ≥1 other device or the encrypted backup** ([ZEB-175](https://linear.app/zeblith/issue/ZEB-175)). Single-device owners still rely on backup.
- Inbound deposits accepted only from authorized peers (friend-edge / co-community membership). Retention bounded (reuse the 30-day DM expiry, [ZEB-227](https://linear.app/zeblith/issue/ZEB-227)).

**Non-functional**
- Self-sovereign: no server, no escrow. First-party content never rests on a third party; the opt-in relay only ever holds ciphertext + minimal metadata.
- Encrypted at rest (SecretVault/keychain) and end-to-end in transit across the owner's own fleet.
- Degrade gracefully: whole-fleet-offline falls back to today's sender-retry, never errors.

## 5. Threat model

| Adversary | First-party path | Opt-in relay path |
|---|---|---|
| Network observer | sees Iroh traffic, not content | sees a sealed blob in/out of the relay |
| The relay device | n/a (never touches it) | holds ciphertext it **cannot decrypt**; learns "owner X has mail for owner Y" — minimized via sealed-sender-style addressing |
| Seized/compromised *own* device | reveals that device's at-rest data (already true today; keychain-scoped) | same |
| Spammer flooding my inbox | admission control: deposits only from friend-graph / co-community peers | relay applies the same admission before accepting |

Residual exposure on the relay path: **timing correlation** (upload time vs. download time) survives sealed-sender. Mitigate with transport padding + randomized polling. This is the price of the opt-in fallback and is why first-party is the default.

The opt-in relay's entire security rests on **sealed delivery** — a blob encrypted to the recipient owner's key — which requires a working recipient X25519. That key is currently a zeroed stub ([ZEB-372](https://linear.app/zeblith/issue/ZEB-372)); it is the first prerequisite, not a footnote.

## 6. The seam — where Sync ends and Butler begins

Two layers, one narrow interface. The Butler is the network-facing accept/hold/deliver layer; Fleet Sync is the local replication layer.

```
            ┌────────────────────── BUTLER (network-facing delivery) ──────────────────────┐
  peer ───▶ │  accept sealed delivery (admission-gated)   │   hold outbound, retry to peer │
            │  advertise butler-set → pkarr/Mainline DHT   │   first-party ▸ then opt-in relay│
            └───────────────┬───────────────────────────────────────────┬──────────────────┘
                            │  write(dataset, op)        list_online_devices()
            ┌───────────────▼───────────────────────────────────────────▼──────────────────┐
            │  FLEET SYNC (local replication)                                                │
            │  per-owner encrypted datasets [dm-history | community-log | notes | read-state]│
            │  each a CRDT/op-log, replicated across MY devices over an owner-auth channel   │
            └───────────────────────────────────────────────────────────────────────────────┘
```

The contract between them is exactly two operations:

- `write(dataset, op)` — the Butler deposits an accepted delivery into a named fleet dataset. Replication fan-out is Fleet Sync's job; the Butler never manages it.
- `list_online_devices()` — Fleet Sync exposes the currently-reachable fleet subset, which is the source for the butler-set advertisement.

Because the seam is this narrow, **SP1's interface is fully pinned by what SP2 needs** — SP1 can be spec'd and built first. The Gemini report independently reproduces this exact decoupling (peer deposits → butler stores in local CAS + inbox log → primary reconnects → reconciles the inbox log from the butler), which is reassuring corroboration that the cut is in the right place.

## 7. Mapping onto the Harmony stack

| Layer | Role | Reuse |
|---|---|---|
| Transport & discovery | direct-dial P2P + NAT traversal; butler-set advertisement | Iroh (QUIC + relays); pkarr/Mainline DHT ([ZEB-322](https://linear.app/zeblith/issue/ZEB-322)/380/382) |
| Control / notify | "something changed" pub/sub (`/owner/fleet/sync`, `/owner/butler/inbox`) | Zenoh |
| Storage | encrypted content-addressed payloads | existing CAS |
| Reconciliation | multi-writer convergence across the fleet | generalize `owner_state_sync.rs` + Mint CAS sync (see §8) |

The clean split is: **Zenoh notifies, Iroh moves the bytes** — a change-notification on Zenoh wakes peers, which then reconcile over Iroh bi-streams. This matches how we already bridge zenoh-over-iroh.

## 8. Mechanism menu from the research

The deep-research report validated the four decisions and the seam. It is most useful as a menu for the sub-project specs. Three of its defaults are **consciously declined** for v1 because our butlers are *first-party* (owner-bound via `EnrollmentCert`), not semi-trusted strangers, so the Signal/Matrix-grade machinery it reaches for is over-built for us.

**Adopt (into SP2/SP1 specs):**
- **Multi-homed pkarr failover** — advertise primary + secondary butler in a priority list, ~10s fallback, to ride out the 60–90s DHT-propagation tail and avoid a single point of failure.
- **Sealed-sender nested envelope** — payload sealed to the recipient owner's X25519; outer layer carries only a destination token + admission proof. Confidentiality holds even if a relay is seized. (Gated on [ZEB-372](https://linear.app/zeblith/issue/ZEB-372).)
- **Zenoh control topics + Iroh bulk transport**; **per-contact quota + TTL** (reuse the 30-day expiry).

**Spec-time options (not v1 commitments):**
- **Rotating `R(t)` rendezvous** — `HMAC(seed, epoch)` ephemeral mailbox IDs (Berty/Wesh style) so DHT observers can't watch a stable identity key. Maps cleanly onto our existing per-friendship secret ([ZEB-371](https://linear.app/zeblith/issue/ZEB-371)) and a per-community seed. Treat as an SP2 metadata-hardening layer; default v1 to identity-keyed discovery and add rotation as a fast follow.

**Consciously declined for v1:**
- **Willow / iroh-docs RBSR for fleet sync — do not auto-adopt.** We already ship a working state-root sync (`owner_state_sync.rs`) + CAS-backed Mint sync. Our datasets are *small* (notes, DM history, read markers); RBSR's `O(δ log N)` edge only pays off at large N, the report's own Risk 2 flags RBSR as heavy on mobile, and iroh-docs has a maturity/maintenance dependency to verify. SP1 must document "generalize our state-root sync" vs "adopt Willow" as an explicit trade-off, leaning simpler unless dataset growth justifies the engine.
- **Double Ratchet + skipped-key cache — probably not v1.** Ratchets and store-and-forward fight (out-of-order arrival, skipped-key caches, ratchet state diverging *across the owner's own fleet*). Per-message sealed ECDH (ephemeral × recipient-static) is simpler, async-native, and fleet-friendly: any of the owner's devices decrypts with the owner key, no shared ratchet state to reconcile. Ratchet-grade forward secrecy is a later upgrade.
- **Per-sender UCAN write-tokens + dynamic PoW + 48h butler re-delegation — over-engineered for first-party butlers.** They are already owner-bound via `EnrollmentCert`; they need no per-session re-delegation, and revocation reuses the existing device-revocation path. Admission = an existing friend-edge / co-community-membership proof ([ZEB-370](https://linear.app/zeblith/issue/ZEB-370)/[ZEB-120](https://linear.app/zeblith/issue/ZEB-120)). Reserve short-lived UCAN + PoW strictly for the opt-in community-relay path, as hardening — not v1 core.

## 9. Dependencies / prerequisites

1. **[ZEB-372](https://linear.app/zeblith/issue/ZEB-372)** — owner/device X25519 is a zeroed stub. **Hard prerequisite** for sealed delivery + relay → blocks SP2.
2. Generalize `src-tauri/src/owner_state_sync.rs` (nav tree + DM metadata + read markers) into SP1.
3. Reuse/generalize `DmOutbox` ([ZEB-216](https://linear.app/zeblith/issue/ZEB-216)) for outbound hold; 30-day expiry ([ZEB-227](https://linear.app/zeblith/issue/ZEB-227)) for retention.
4. pkarr/Mainline DHT ([ZEB-322](https://linear.app/zeblith/issue/ZEB-322)/380/382) for butler-set advertisement.
5. Friend graph ([ZEB-370](https://linear.app/zeblith/issue/ZEB-370)/371) + RCPT admission ([ZEB-120](https://linear.app/zeblith/issue/ZEB-120)) for inbound authorization/anti-spam.
6. Relates: [ZEB-116](https://linear.app/zeblith/issue/ZEB-116) (Merkle multi-device), [ZEB-213](https://linear.app/zeblith/issue/ZEB-213) (backup includes CRDT root), [ZEB-340](https://linear.app/zeblith/issue/ZEB-340) (unify signing on device key #2).

## 10. Decomposition

- **SP1 — Fleet Sync substrate** ([ZEB-417](https://linear.app/zeblith/issue/ZEB-417)): one reusable per-owner replicated-dataset primitive; generalizes `owner_state_sync` + Mint sync; absorbs [ZEB-361](https://linear.app/zeblith/issue/ZEB-361) (Notes sync) as its first consumer. Its interface is pinned by §6, so it is spec'd first.
- **SP2 — Butler delivery** ([ZEB-418](https://linear.app/zeblith/issue/ZEB-418)): butler-set advertisement, inbound deposit, outbound hold, first-party path, opt-in community-scoped sealed relay, admission + retention. Blocked by [ZEB-372](https://linear.app/zeblith/issue/ZEB-372); depends on SP1.

## 11. Questions deferred to the sub-project specs

**SP1**
- Reconciliation engine: evolve state-root-compare vs adopt Willow/iroh-docs RBSR (lean: evolve).
- Dataset model: per-dataset CRDT shape (LWW-element-set vs grow-only log + tombstones) and how `dm-history` / `community-log` map onto datasets.
- Durability mechanics: how "reached ≥1 other device" is observed and surfaced to the user.

**SP2**
- Rendezvous: identity-keyed pkarr (v1) vs rotating `R(t)` (hardening).
- Envelope: confirm per-message sealed ECDH; exact AEAD + key-derivation construction (post-ZEB-372).
- Community relay: how a community advertises/selects its volunteer relay(s); opt-in UX; quota/PoW policy on that path only.
