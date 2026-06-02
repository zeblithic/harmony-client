# ZEB-336 — Profile / display-name model (owner-canonical name + owner-private device hints)

- **Issue:** [ZEB-336](https://linear.app/zeblith/issue/ZEB-336) (parent: ZEB-327 alpha umbrella)
- **Status:** design approved 2026-06-02
- **Surfaced during:** v0.1.0-alpha smoke test, 2026-05-28
- **Related:** ZEB-173 (owner→device binding, Done), ZEB-338 (WelcomeModal hard gate, Done), ZEB-334 (self-notes — surfaced the "Anonymous" gap), ZEB-361 (owner-device sync — shares plumbing)

## Problem

A user's display name has no deliberately-designed relationship to the owner-vs-device identity model, and the current implementation conflates two distinct concepts:

- The owner's profile card **is** already broadcast owner-canonically — keyed by `owner_id` (`harmony/discovery/profile/owner/{owner_id}/card`, `src-tauri/src/profile_card_broadcast.rs`) and resolved by peers per `ownerIdHex` (`src/lib/member-card-service.ts`).
- **But** the local profile (`src/lib/profile-service.ts`) stores `displayName` against a random per-device `address` (a "placeholder until real key management"), decoupled from the real owner identity (`selfOwnerId`, `App.svelte`).
- **And** `DevicesPanel.svelte` shows each device's name by **reusing `profile.displayName`** (the backend returns a placeholder "this device"). So today **"your name" and "this device's name" are the same field** — there is no separate device label.

Alpha smoke also showed users land as "Anonymous" / "You" with no obvious prompt to set a name (see ZEB-334 / ZEB-337).

## Decisions

1. **Display name is a property of the owner identity** (owner-canonical): one "Jake" rendered everywhere, regardless of which device authored a message. (Per the model question — chosen over per-device names and pure-owner-no-hint.)
2. **Pre-binding state is already resolved by ZEB-338:** the `WelcomeModal` hard gate mints an owner identity on first run before the app is usable, so there is no "unbound device with no owner" state. The display name is always owner-keyed.
3. **A device label is a distinct, owner-private concept** ("KRILE", "Koya"), separate from the owner display name.
4. **Device hints ("(on KRILE)") are owner-private by default.** Peers always see the plain owner-canonical name. Per-message device attribution exists on the wire (`ChannelMessageDto.at.deviceId`, the HLC device_id), but device labels are NOT broadcast — so a hint only ever renders in the owner's own contexts.
5. **Peer-visible device labels are a future, strictly opt-in capability** — never a default, and chosen per-person or per-community. Deferred (Phase 3).

## The model — three separated concepts

| Concept | What it is | Scope | Where it lives |
|---|---|---|---|
| **Owner display name** | The person ("Jake") | Broadcast to all peers (owner-canonical) | Owner profile card (already owner-keyed) |
| **Device label** | This machine ("KRILE") | Owner-private | Per-device; synced only across the owner's own devices (Phase 2) |
| **Device hint** | "(on KRILE)" annotation | Owner-private; rendered only in the owner's own contexts | Derived: map `HLC.device_id` → the owner's device-label table |

## Implementation phases

The conceptual model is fully specified above; the build is phased to respect dependencies (and to share plumbing with ZEB-361 / owner-state sync).

### Phase 1 (v1 — this ticket's core)

1. **Separate owner name from device label.** Stop using `profile.displayName` as the device name in `DevicesPanel`. Introduce a distinct per-device label field.
2. **Owner display name = the canonical name.** Ensure `ProfileEditor`'s display name is the owner's name, associated with the owner identity (not the random `profile.address`), and broadcast as the owner card (already the case on the wire — this is mostly a frontend keying/clarity cleanup + making the address seam explicit).
3. **First-run name step.** Add a lightweight "What should we call you?" step to the first-run flow (after owner mint, in/after `WelcomeModal`) so testers set a real name instead of staying "Anonymous". Pre-fills nothing; skippable (defaults to "Anonymous", editable later in Settings).
4. **Device label field.** Per-device, owner-private, stored locally, editable in `DevicesPanel`. Default to the OS hostname via `@tauri-apps/plugin-os` `hostname()`, falling back to "This device" if unavailable. **Confirmed during planning:** the JS API exists (`@tauri-apps/plugin-os` is already a dep, used by `onboarding-env.ts`), but the granted capability is `os:default`, which *excludes* hostname — so this requires adding `os:allow-hostname` to `src-tauri/capabilities/default.json`. No Rust command needed.

The feed device-**hint** rendering ("(on <label>)") is **deferred to Phase 2** — see the planning note at the bottom. In short: the frontend `Message` type carries no device id, and the hint is suppressed for single-device users and can only resolve *this* device's label until the Phase 2 roster sync lands, so Phase 1 ships the label *separation* + *store* without the (today-invisible) feed annotation.

### Phase 2 (fast-follow — owner-private cross-device hints + hint rendering)

Two coupled pieces, deferred together because neither is useful without the other:

1. **Device-label roster sync.** Sync the owner's **device-label roster** across the owner's own devices (so KRILE can render "(on Koya)" for a message you authored on Koya). This rides the existing **owner-state sync** substrate (`src-tauri/src/owner_state_sync.rs` — already syncs nav tree + DM metadata + read markers between an owner's devices) — **not** a public broadcast. Shares plumbing with ZEB-361 (notes sync): both are "small per-owner datasets synced across my devices."
2. **Feed device-hint rendering** (moved here from Phase 1). Render "(on <label>)" in owner-private surfaces by mapping a message's `HLC.device_id` → the owner's device-label roster. Requires plumbing a device id onto the frontend `Message` type (from `ChannelMessageDto.at.deviceId` / the DM equivalent) and confirming `HLC.device_id` aligns with the device-identity id namespace. Suppressed for single-device owners.

### Phase 3 (future — opt-in peer-visible device labels)

Allow a user to *optionally* reveal device labels to specific people or communities (e.g. "Jake (on KRILE)" visible to a trusted community). **Strictly opt-in, never default**; per-person or per-community granularity. Requires a new privacy-gated broadcast surface. Out of scope for now; recorded so the model is complete.

## Surfaces (where names + hints render)

- **Message author labels** (`ChannelMessageFeed.svelte`, `TextFeed.svelte`): owner display name (peers). For *your own* messages, optionally append the owner-private device hint.
- **Member lists** (`MemberRow.svelte`), **DM list**, **profile popover**: owner display name only.
- **Notifications** (`notification-service.ts`): owner display name; for notifications about *your own* devices, the device hint adds clarity.
- **DevicesPanel** (`DevicesPanel.svelte`): shows each of your devices by its **device label** (not the owner name) — the primary place device labels are set/seen; marks "this device".

## Migration

- The existing `localStorage['harmony-profile'].displayName` is treated as the **owner display name** (unchanged for users who set one).
- Introduce a separate device-label store; `DevicesPanel` reads the device label from it instead of overlaying `profile.displayName`. Default the local device's label to the OS hostname on first read.
- Make the `profile.address` vs `owner_id` seam explicit: the profile is the *owner's* profile (keyed to the owner identity); the random `address` is retired from any identity-bearing role (kept only as a legacy local id if still referenced, with a follow-up to remove).

## Testing

- **Owner name**: set in `ProfileEditor` → reflected in feed/member surfaces; first-run name step persists; defaults to "Anonymous" when skipped.
- **Device label**: separate from owner name; defaults to hostname; editable in `DevicesPanel`; renaming one device does not change the owner name (regression guard against the current conflation).
- **Device hint (Phase 1)**: single-device user sees NO redundant hint; this device's own activity can render its label; peers' messages never carry a hint.
- **Phase 2**: a label set on device A appears in device B's hint rendering after owner-state sync; converges; no leak to peers.

## Out of scope

- Peer-visible device labels by default (Phase 3 is opt-in only).
- Renaming the underlying device identity keys (labels are a presentation layer over the existing `device_ed25519` / HLC `device_id`).
- Rich profile fields beyond what `ProfileEditor` already supports (avatar, status, About page — shipped in ZEB-341/345).

## Implementation notes (2026-06-02, planning)

Captured while turning this spec into an implementation plan, after reading the affected code:

- **Feed device-hint rendering deferred Phase 1 → Phase 2** (confirmed with the user). Phase 1 ships the label *separation* + *store* + first-run name; the "(on KRILE)" feed annotation rides Phase 2. Rationale: the frontend `Message` type (`src/lib/types.ts`) has no device id, so the hint needs new plumbing from `ChannelMessageDto.at.deviceId`; and per the model it's suppressed for single-device owners and can only resolve *this* device's label until the Phase 2 roster sync — so in today's alpha it renders nothing (or only an asymmetric this-device-only hint). Building it now is dead/partial plumbing; it belongs with the roster sync that makes it work.
- **Device label default needs a capability grant.** `@tauri-apps/plugin-os` is already a dependency and exposes `hostname()`, but `src-tauri/capabilities/default.json` grants `os:default`, which explicitly *excludes* hostname ("All information except the host name"). Phase 1 adds `os:allow-hostname`. No Rust command required.
- **First-run name step = a separate, dismissible post-gate `NamePromptModal`**, NOT a new stage inside `WelcomeModal`. The `WelcomeModal` is a hard gate with seed-redaction + focus-trap invariants and tests asserting `mint → backup` directly; the name step is *skippable* (so it isn't gate-like) and isolating it keeps the audited gate and its tests untouched. App shows it after `onMinted` when the profile name is still "Anonymous"; Save reuses `handleProfileSave` (persist + re-seed card + publish), Skip dismisses.
- **The conflation bug is real and pre-documented.** `DevicesPanel.svelte`'s `applyLocalProfileOverlay` overlays one `profile.displayName` onto *both* the owner header and the this-device row, and `saveRename` writes a device rename back into `profile.displayName` — i.e. "rename this device" currently renames the *owner*. The prior author left a migration note (`DevicesPanel.svelte:22-53, 92-111`) describing exactly this split. Two existing tests encode the coupling and flip as the TDD red signal: `'saving the rename calls profile-service.saveProfile'` and `'overlays profile.displayName onto the isThisDevice row after refresh'`.
