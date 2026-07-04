# ZEB-236: user-driven DM-invite accept/decline — design

Stop silently auto-accepting DM invites from non-friends. Stage them for a user
decision behind a non-blocking surface; keep active-friend invites
auto-accepting (the friendship approval was already the consent gate).

Decisions settled with Jake 2026-07-04: **tiered policy** (friends auto-accept,
non-friends prompt) and **non-blocking toast + pending list** (supersedes the
ticket's app-modal framing). Live motivation: during the ZEB-636 flag-day probe
the same day, a fleet node silently auto-accepted an inbound `DmInvite` —
the exact gap.

## Contract (DM transport spec §"DmInvite rejection / decline semantics (v1)" — unchanged)

- **Decline writes no persistent state anywhere** — no CRDT mutation, no local
  sidecar, no file. Indistinguishable from the device being offline.
- **No notification to the inviter.** Their outbox entries expire at 30 days
  like any unacked DM.
- **Repeat invites re-prompt.** No durable blocklist in v1 (separate ticket if
  wanted).
- Already-accepted same `dedupe_key` → normal CRDT merge (existing behavior,
  untouched).

## Backend

### Policy fork in `apply_invite` (`dm_outbox.rs` — auto-accept tail at ~2279)

The five existing gates (inviter-bind, inviter∈members, signing-device∈
sender_devices, self∈members, signature verify) run unchanged. After they pass:

- **Inviter is an ACTIVE friend** → existing accept tail runs exactly as today
  (Space apply + gated `OwnerDeviceCache` refresh). Behavior byte-identical.
- **Otherwise** → do NOT write Space or cache. Return a new outcome variant
  carrying the verified invite; the **caller** stages it and emits the event.

`apply_invite` stays pure over `OwnerState` (no store/sink plumbed through it):
it returns `ApplyInviteOutcome::{Accepted, Staged(StagedDmInvite)}` and the two
ingest callers (`dm_inbox_ingest.rs` tunnel arm; `community_relay_prod.rs`
direct-relay arm + `apply_deposited_invite` wrapper) perform
`pending.stage(...)` + `emit_ser(sink, "dm-invite-received", …)` on `Staged`.
The friend check reads the same source `list_friends` uses (active status in
the friend graph), evaluated under the `OwnerState` lock already held.

### `PendingDmInvites` store (new module, `friend_requests.rs` pattern verbatim)

- Process-local `Mutex<HashMap<SpaceId, StagedDmInvite>>`. **Deliberately
  ephemeral** — ZEB-483 co-deposits the (rebuilt) invite alongside every
  message CidNotify, so a restart-lost pending invite re-stages on the next
  inbound message. Ephemerality is also what keeps the decline contract pure.
- `StagedDmInvite` keeps everything accept needs later: the verified
  `DmInviteSigned`, `received_at_ms`, and the ingest route's
  `refresh_owner_device_cache` flag (tunnel=true / deposit=false — accept must
  honor the same trust distinction the auto-accept path does today).
- API: `stage()` (idempotent by `space_id`; returns `bool` newly-staged —
  redundant ZEB-483 redeliveries of an already-pending invite must NOT re-emit
  the event), `list()`, `take(space_id)`.
- Parked on `NodeState` as `Option<Arc<PendingDmInvites>>` (the
  `PendingFriendRequests` slot pattern).
- Decline-then-change-mind falls out for free: decline removes the entry; the
  next co-deposited redelivery re-stages and re-prompts (spec's "repeat
  invites re-prompt", satisfied without any timer logic).

### Verbs (three-layer convention: `#[tauri::command]` wrapper → `_impl` seam → `rpc!`)

| Verb | Args | Behavior |
|---|---|---|
| `list_pending_dm_invites` | — | `Vec<PendingDmInviteDto>` |
| `accept_dm_invite` | `{spaceId}` | `take()` from store; run the accept tail (Space apply + cache refresh per the stored flag) under the `NodeState` lock; emit `dm-invite-list-changed` + `nav-updated` (the new Space must appear in nav) |
| `decline_dm_invite` | `{spaceId}` | `take()` and drop; emit `dm-invite-list-changed`. Nothing else — the contract. |

- `PendingDmInviteDto` (`camelCase`): `spaceId`, `inviterOwnerIdHex`, `kind`,
  `members[]`, `createdAtMs`, `receivedAtMs`. **Never** `content_key` or
  `inviter_identity_pub` (trust-secret material stays backend-side, mirroring
  `PendingFriendRequestDto`'s minimalism).
- Events: `dm-invite-received` (new staging only) + `dm-invite-list-changed`
  (any store mutation). Emitted via `NodeEventSink` → reaches the Tauri webview
  AND the headless `/events` WS from one call; both names added to the
  `rpc.rs` WS event allowlist. Verbs registered in `rpc.rs` (headless parity —
  fleet agents accept/decline programmatically) and `generate_handler!`
  (no capabilities edit needed — confirmed by the `accept_friend_request`
  precedent).
- Unknown `spaceId` → `Err("no pending DM invite for space")`.

### Tests (Rust)

1. **Decline writes no state** (the reinstated spec test): stage a non-friend
   invite, decline via `_impl`, assert canonical `OwnerState` bytes unchanged
   and store empty.
2. **Tier split**: active-friend invite → Space applied, nothing staged
   (existing tests keep passing untouched); non-friend invite → staged, no
   Space, no cache write.
3. **Accept parity (golden)**: accept-after-stage produces `OwnerState`
   byte-identical to the pre-change auto-accept for the same invite, for BOTH
   `refresh_owner_device_cache` variants.
4. **Redelivery idempotence**: second `stage()` of the same pending `space_id`
   returns not-new (callers emit nothing); decline-then-redeliver re-stages.
5. `_inner` projector unit test for the DTO (no secret material serialized).

### Fleet / e2e impact: none

Every existing automated DM flow (GCE T2, e2e harness, fleet probes) redeems a
friend token BEFORE DMing — the inviter is an active friend on the recipient
by then, so the auto-accept tier preserves those flows unchanged. Verified
live: today's probe showed `status: "active"` on the recipient post-redeem.

## Frontend

### `dm-invite-service.ts` (mirror of `friend-service.ts`)

`connectAdapter` listens `dm-invite-received` + `dm-invite-list-changed`,
fans out via listener `Set`s; `listPending()`, `accept(spaceId)`,
`decline(spaceId)` wrap the verbs with the standard
`e instanceof Error ? e.message : String(e)` normalization. DTO hand-mirrored.

### Surfaces (Jake's pick)

1. **Toast** on `dm-invite-received`: `IncomingCallToast`-style corner card —
   "DM invite from `{inviter short-hex}` ({kind})" with **Accept** /
   **Decline** / **Later** (Later just dismisses the toast; the invite stays
   pending). Mounted at the App-level toast area.
2. **Pending list**: a "DM invites" section in `FriendsPanel` directly below
   the friend-requests inbox (same social-inbox surface, same row pattern:
   accept/decline buttons, in-flight guard, event-driven refresh). Non-friends
   have no nickname — display short owner hex.
3. Accept → the DM appears via the existing `nav-updated` flow; both surfaces
   refresh via `dm-invite-list-changed`.

### Tests (vitest)

Service event fan-out with `createMockAdapter`; toast component accept/decline
callbacks; FriendsPanel section render + action wiring (extend
`FriendsPanel.test.ts`'s mock-service pattern).

## Non-goals

Durable blocklist (spec v1 exclusion); auto-promoting a staged invite when the
inviter later becomes a friend (user still decides; accept works regardless of
friendship); badge counts; invite expiry timers (ephemeral store + redelivery
makes them unnecessary); group-DM member-level vetting (tier keys on the
inviter only — noted for a follow-up if group-DM abuse appears).
