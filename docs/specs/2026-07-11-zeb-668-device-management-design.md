# ZEB-668 — Device management: revoke, replace, last-seen, petnames, fleet-key epoch

Status: draft for Jake's review. The four scope forks were settled interactively
(2026-07-11): rotation = composed revoke + re-pair; revoke power = master + self;
`#3`-signature gap = documented with follow-ups; extras = KeyTree epoch bump AND
synced petnames both in scope.

**Two items pending Jake's explicit confirmation (flagged, not assumed):**

1. §2 trust-state replication as the foundation slice — required because the
   trust CRDT is not network-synced today (survey correction to the first
   design presentation).
2. §7 epoch-bump version handling — no version-gate; a loud release note and
   "all fleet devices must upgrade" instead (fleet is ours, RC stage).

## §0 Ground truth (as-implemented, main @ d7a97e18)

Key inventory per device: **#1 owner master** (32-byte seed → ed25519; only on
seed-holding devices; reconstructable anywhere from the recovery artifact),
**#2 enrolled device signing key** (per-device ed25519, master-signed into an
`EnrollmentCert`; `device_id` = identity_hash of its `PubKeyBundle`), **#3
node identity** (`harmony_identity::PrivateIdentity`, per-device, never crosses
the pairing wire). All secrets live in the single `SecretVault` keychain item
(service `harmony` / account `identity`; slots Iroh/Device/OwnerMasterSeed +
`fleet_keytree`), with HRMI encrypted-file fallback.

Load-bearing facts the design builds on:

* There are TWO structures named OwnerState. The **trust CRDT**
  (`harmony_owner::state::OwnerState`: enrollments / vouching / revocations /
  liveness → `owner_state.cbor`) is **not network-replicated** — written only
  at pairing/mint/restore and local liveness refresh. The zenoh-synced one is
  the **nav/spaces CRDT** (`owner_state_crdt::OwnerState` →
  `owner_state_crdt.cbor`, topic `harmony/owner/{addr_hex}/state-root-v1`,
  KeyTree-encrypted, generic `fleet_sync::FleetSyncEngine`).
* `RevocationCert` is fully implemented in harmony-owner (issuers SelfDevice |
  Master; Quorum returns `QuorumRevocationNotImplemented`; `RevocationSet` is
  remove-wins/monotonic; `active_devices` excludes revoked; `evaluate_trust` →
  `Refused(Revoked)`). The client has **zero** wiring to it.
* Enrollment in practice is **N=1 master-signed** (`pairing/cert.rs` transient
  master reconstruct); the crate's K=2 quorum + vouching machinery is unused,
  and peer verifiers actively reject quorum-issued certs
  (`iroh_friend_acceptor.rs:785`).
* Pairing mints certs with `expires_at: None` — **enrollment certs never
  expire** — and every peer verifier (friend/PEX/community) checks
  expiry + master signature on a **lone cert**; none consults revocation.
* Signing split: #2 signs community events, deposits, relay pulls, voice,
  friend handshake, profile card, reachability. #3 still signs DM packets
  (ZEB-580), vine records + follow lists, storage-buddy records, and the
  profile membership broadcast.
* `FleetKeyMaterial.epoch: u32` exists on the wire, but epoch 0 is hard-coded:
  the HKDF salt bakes `epoch-0` in, `to_fleet_material()` stamps 0
  unconditionally, `from_fleet_material` rejects anything else. Enrollment
  seals epoch-0 material to joiners; only seed-holders can derive.
* Per-device last-seen signal already ships: fleet-net `FleetNetRow.seen_at`
  (HLC, self-stamped ~7.5 min + boot + reconnect, 15-min staleness window,
  CRDT-synced per-row LWW) — unsurfaced. `peer_liveness.last_connected_ms`
  is reserved in code for last-seen telemetry (in-memory, transport-keyed;
  joinable via `FleetNetRow.iroh_endpoint_id`).
* Device rename today is localStorage-only, self-only; the backend has no
  petname concept (sibling rows render "Device <hex[..8]>").

Honesty rule (ZEB-610 §0) governs everything: render only what the backend
can prove; UI copy states exactly what an action does and does not do.

## §1 Scope (locked) and non-goals

In scope: device revoke (master + self issuers), trust-state replication,
community retire-announce, per-device last-seen, synced device petnames,
KeyTree epoch bump on revoke, "Replace this device" composed flow.

Non-goals (v1): owner-master rotation (future harmony-kel); quorum
enrollment/revocation (follow-up ticket); auto-wipe of a revoked device's
local data (irreversible destruction stays a human action); migrating the #3
signing surfaces (follow-ups; DMs already ZEB-580); any change to the SAS
pairing ceremony.

## §2 S1 — Trust-state replication (foundation)

The next `FleetSyncEngine` dataset (the eighth engine instance overall —
same donor pattern `fleet_net` used) —
carrying the trust CRDT between the owner's devices:

* Transport via the established dataset pattern (plan-time amendment —
  seven `FleetSyncEngine` datasets already ship through
  `spawn_dataset_sync_zenoh_adapter`): dataset `owner-trust-v1` on
  `harmony/owner/{addr_hex}/ds/owner-trust-v1`, lookup tag
  `b"owner-trust-v1"`; KeyTree-encrypted like every sibling dataset;
  debounce `DEFAULT_DEBOUNCE_MS` (250 ms).
* Merge function delegates to the crate's own idempotent mutators
  (`add_enrollment`/`add_vouching`/`add_liveness`/`add_revocation`), folding
  the remote doc's records into the local one. Revocations are remove-wins,
  so merge order cannot resurrect a device. Records that fail crate
  validation are dropped with a warn log (never trust-degrading).
* Persistence through the existing `owner_state.cbor` save path
  (`save_owner_state_cbor_only` — disk-only, no keychain; keychain writes
  elsewhere stay behind the `*_inner`
  seams per ZEB-428).
* Publish triggers: any local trust mutation (revoke, liveness refresh,
  enrollment completion) → `notify_dirty`.
* **Revoked-self halt hook** (no data is wiped — §1 non-goal): the merge
  callback checks `is_revoked(self)`;
  if newly true → emit `device-revoked-self`, stop fleet publishes
  (owner-state, fleet-net, trust) and butler participation, and the UI shows
  a terminal "This device was removed from your account" state. Local user
  data is NOT wiped (§1 non-goal); copy says so.
* Side effect worth naming: liveness certs now propagate between devices —
  this delivers most of ZEB-410's intent (its ticket gets a note, not a
  close, since its 30d-cadence tightening remains open).
* Frontend: new `owner-devices-updated` Tauri event on any trust merge that
  changes the device set, so the panel stops being poll-only.

## §3 S2 — Revoke IPC + DevicesPanel UI

New IPC `revoke_device { deviceVkHex, reason }` → `()`:

* `reason` ∈ `"decommissioned" | "lost" | "compromised"` → maps to
  `RevocationReason` (Other unused in UI).
* Issuer selection: if `deviceVkHex` is the local device → 
  `RevocationCert::sign_self` (any device may remove itself). Otherwise the
  command requires the master seed in the vault (seed-holder), transiently
  reconstructs the master exactly as `pairing/cert.rs` does (sign, drop,
  zeroize) → `RevocationCert::sign_master`. On a cert-only device targeting
  a sibling → stable error prefix `notMaster:`.
* Guards: refuse revoking the last active device (`lastDevice:` prefix).
  Revoking the seed-holder itself is allowed but the UI warns (below).
* After `add_revocation`: persist, `notify_dirty` (S1 engine), emit
  `owner-devices-updated`, and hand the cert to S4's retire-announce queue.
* Self-revoke ordering is load-bearing: sign → add → persist → **publish and
  flush the trust doc** → only then enter the removed terminal state and stop
  engines (otherwise no sibling ever learns). The initiating device enters
  the terminal state directly, without waiting for its own merge callback.

DevicesPanel:

* Sibling rows show **Remove…** only when the local device holds the master
  seed (honesty rule — the affordance renders only where the IPC can
  succeed; `canBackUp` already exposes seed-holding to the frontend). The
  self row shows **Remove this device** everywhere.
* Confirmation = existing `TypeToConfirmDialog` (typed device name), reason
  picker (three reasons above), destructive styling. Copy states exactly
  what is severed (community posting, fleet sync, deposits/relay) and what
  is NOT (existing DMs/vines/storage records until the follow-up
  migrations land — §8). Removing the seed-holder adds: "This device holds
  your master key. Afterwards you will need your recovery phrase to manage
  devices."
* Revoked devices leave the active list (`active_devices` semantics) and
  appear in a collapsed **Removed devices** section (name, reason, date) —
  rendered from the local `RevocationSet`, which is real data.
* `DeviceView` additions: `revoked: bool` + `revokedAt`/`revokedReason` for
  the removed section (serde camelCase).

## §4 S3 — Community retire-announce

Communities verify #2 signatures against `enrolled_device_keys` learned via
the device-intro CRDT; without an active signal they would accept a revoked
device's community events indefinitely (certs never expire). Symmetric to
`CommunityDeviceIntroEntry`:

* New entry kind `CommunityDeviceRetireEntry { owner_id, device_id,
  revocation_cert, relayed_by, ttl }` in the device-intro CRDT doc
  (additive field, `#[serde(default)]` — no file-version bump, per the
  content-index lesson).
* Any surviving enrolled device relays it into each shared community
  (same relay/coverage/GC machinery as intro: grow-only `relayed_by`,
  30-day TTL).
* Receivers verify the `RevocationCert` (master-signed, or self-signed by
  the retired device key) against the member's known owner binding, then
  remove the key from `enrolled_device_keys`. Subsequent events signed by
  the retired key are rejected exactly as unknown-device events are today.

## §5 S4 — Per-device last-seen + synced petnames

Last-seen (read-only surfacing; no new heartbeat):

* Read seam: `get_owner_state` joins fleet-net rows by `device_vk_hex` →
  `DeviceView.lastSeenMs: number | null` (`seen_at.wall_ms`), plus
  `connectedNow: boolean` when the device's `iroh_endpoint_id` has a live
  `Connected` peer_liveness slot.
* UI: relative-time idiom already in the panel (`formatEnrolledAt` family);
  `null` → render nothing (device never fleet-synced — honest absence).
  Copy reflects cadence: "Last seen ~2h ago" tolerating the 7.5-min stamp.

Petnames (fleet-synced rename for every row):

* New LWW map on `FleetNetDoc`: `petnames: BTreeMap<deviceVkHex,
  { name, setAt: Hlc }>` (additive, `#[serde(default)]`). Deliberately NOT
  inside `FleetNetRow` — rows are self-stamped by their device; petnames
  are assigned by any device about any device. LWW by `setAt`.
* IPC `set_device_petname { deviceVkHex, petname }`; empty string clears.
* `DeviceView.petName: string | null`; label ladder in the panel becomes
  `petName ?? localStorage label (self only, migration seed) ?? "Device
  <hex[..8]>"`. First `set_device_petname` for the local device migrates the
  localStorage value; the localStorage path then becomes read-only fallback.

## §6 S5 — KeyTree epoch bump on revoke (hardest slice)

Purpose: a revoked device retains sealed epoch-0 `FleetKeyMaterial` and could
keep decrypting fleet-net/owner-state/trust publishes. On master-issued
revoke, the fleet moves to epoch N+1:

* Per-epoch HKDF salt derivation replaces the baked constant:
  `b"harmony-owner-state-v1-epoch-{N}"` (epoch 0 output must remain
  byte-identical to today's — pinned by test).
* The seed-holder derives epoch-N+1 material and seals one copy per
  surviving device to that device's x25519 (from its `EnrollmentCert`
  `PubKeyBundle`), publishing the sealed blobs through the trust doc (S1).
  The revoked device can see the blobs but cannot open any.
* Receivers install the new material into the vault `fleet_keytree` slot
  (through the `*_inner` seams), then publish on the new epoch.
* Transition: dual-epoch read window — engines accept the previous epoch on
  the subscribe path while publishing on the newest, until every active
  device's liveness postdates the bump or a 7-day window elapses, whichever
  first; then the old epoch is dropped from the accept set.
* `from_fleet_material` (import) accepts any epoch whose material this
  device has installed; a sealed blob for an unknown-higher epoch installs
  it. The engine's *decrypt accept set* is what narrows after the
  transition window ({N, N+1} → {N+1}).
* Version handling (pending confirmation, item 2 above): no version-gate.
  Older builds reject non-zero epochs and will fall out of fleet sync after
  a bump — release note makes this loud; fleet is ours and pre-RC.
* Self-revoke does NOT bump the epoch (no seed on cert-only devices); the
  panel notes stale fleet keys and offers "bump from this device" copy on
  the seed-holder. Master-revoke always bumps.

### §6.1 S5 ground-truth amendments (plan-time, 2026-07-11)

The pre-implementation seam survey invalidated two §6 mechanisms; the
amendments below supersede the corresponding bullets above.

1. **Sealed blobs do NOT travel through the trust doc.** The trust doc's
   wire type is the crate-owned `harmony_owner::state::OwnerState`
   (git-pinned rev, exhaustively destructured in
   `merge_trust_remote_into_local`) — not a client-local struct, so the
   additive `#[serde(default)]` trick (FleetNetDoc `pt`) cannot apply
   without a cross-repo crate bump. Worse, any carrier that itself rotates
   epochs deadlocks bootstrap: blobs published under epoch N+1 encryption
   can never be read by the survivors that need them to *reach* N+1.
   **Amendment:** a new client-local `FleetSyncEngine` dataset
   `fleet-keys-v1` (ninth engine instance, same donor pattern as
   `owner-trust-v1`) carries `FleetKeyEpochDoc { epoch, bump_wall_ms,
   sealed: BTreeMap<device_id_hex, sealed_blob>, master_sig }`. The
   carrier is **permanently encrypted under the epoch-0 KeyTree** (every
   enrolled device holds epoch-0 from pairing), which resolves bootstrap
   and makes chained bumps safe (a device offline across two bumps
   installs the current epoch directly). Because revoked devices retain
   epoch-0 and could forge carrier publishes, the doc payload is
   **master-signed** and receivers accept only strictly-higher epochs with
   a valid signature (rollback- and forgery-proof). Leak surface to a
   revoked device: epoch counter, bump time, device-id list, unopenable
   sealed blobs — recorded in the §8 honesty ledger.
2. **Transition-window liveness source is `FleetNetRow.seen_at`, not
   trust-doc liveness.** `LivenessCert` refreshes on a ~15-day cadence —
   unusable for a 7-day window. Fleet-net rows self-stamp every ~7.5 min.
3. **Epoch-0 material is never pruned.** It keys the carrier forever. The
   data engines' decrypt accept set still narrows {N, N+1} → {N+1} at
   window close; the vault invariant is "epoch-0 + current (+ previous
   during the window)".
4. **Pairing hands over a material set.** The ENROLL payload gains an
   additive optional `fleet_keytree_set_cbor_hex` (epoch-0 + current), so
   a device enrolled after a bump can read both the carrier and current
   traffic. The legacy single-material field keeps carrying epoch-0 for
   old builds; an old-build joiner into a bumped fleet falls out of fleet
   sync (same no-version-gate posture as the §6 decision, release-noted).
5. **Staleness signal covers all revocations, not just self-revokes.**
   `fleetEpochStale` = any revocation cert newer than the last bump
   (pre-S5 revocations included — those devices really can still decrypt,
   so showing stale is the honest state). Seed-holder panel offers the
   manual bump; other devices show a passive note.
6. **The friend-secret domain stays pinned to the epoch-0 tree**
   (implementation-time amendment). Friend secrets are sealed once and
   stored durably in the owner-state CRDT; rotating `friend_aead` would
   orphan every stored secret at window close unless the bump re-sealed
   them all. Rotation buys nothing there: a revoked device cannot RECEIVE
   new CRDT states post-rotation (the wire keys rotate), and it already
   knows the secrets it decrypted before revocation. Residual risk (a
   revoked device that somehow obtains post-revocation CRDT bytes out of
   band could open friend-secret blobs) is accepted and recorded in the
   §8 ledger; a re-seal migration is a §9 follow-up candidate. All ten
   fleet engines sharing the fleet tree rotate — ground truth found
   owner-state, fleet-net, trust, notes, DM-inbox, DM-outhold, relay-hold,
   relay-optin, community-device-intro, and mint, not the three §6 names.

## §7 S6 — "Replace this device" (the rotation answer)

Pure composition, no new cert types: on the seed-holder, **Replace…** on a
sibling row = typed-confirm revoke (reason pre-set "decommissioned") +
immediately launch inviter pairing; on completion the old row's petname is
written for the successor via `set_device_petname`. Master rotation remains
out of scope and the spec says so in UI-adjacent docs.

## §8 Honesty ledger

| Claim the UI might imply | Reality in v1 | Handling |
|---|---|---|
| "Remove device" cuts it off everywhere | Cuts #2 surfaces (community posting via S3, fleet sync via S1+S6 epoch, deposits/relay/voice/friend handshake once state propagates) | Confirm-dialog copy enumerates |
| …including DMs/vines/storage records | PARTIAL — **vine feeds now block** a revoked device's post-revocation records once the feed has migrated to its #2 device key (ZEB-678: revocation-aware `FeedAuthorityRecord`, sticky/first-write-wins cache). DMs and storage records still sign with the device's own #3 node identity | Vines: **retired by ZEB-678** — `revoke_device` publishes a revoked authority (self- and master-revoke); confirm-dialog copy now states the feed cutoff. DMs: ZEB-580. Storage: separate §9 ticket |
| …and friends/PEX immediately | NO — lone-cert verifiers, certs never expire | Follow-up ticket (§9): revocation-aware friend verification (can reuse retire-announce mechanics) |
| Last seen = live presence | It is the last fleet heartbeat (~7.5 min cadence, HLC wall time) | Copy + tolerance; null renders nothing |
| Removed device's data is gone | Local data on that device persists until its owner wipes it | Terminal-state copy on the revoked device says so |
| Enrollment is quorum-protected | N=1 master-signed today | Recorded here for the quorum follow-up; no UI claim made |
| Epoch bump cuts a revoked device off completely | It still reads the epoch-0 `fleet-keys-v1` carrier: epoch counter, bump times, device-id list, sealed blobs it cannot open. No key material, trust state, or content | §6.1 amendment 1; accepted metadata leak, documented here |
| Epoch bump rotates every fleet key | The friend-secret domain (`friend_aead`) stays on the epoch-0 tree — stored blobs would otherwise be orphaned. A revoked device that obtains post-rotation CRDT bytes out of band could open friend-secret blobs (it cannot receive them via sync) | §6.1 amendment 6; re-seal migration is a follow-up candidate |

## §9 Follow-up tickets (file at implementation end, use assigned IDs)

1. Quorum revocation + quorum enrollment wiring (lost-master story; crate
   machinery exists, peer verifiers must stop rejecting quorum certs).
2. Vine-record + follow-list signing migration #3 → #2.
3. Storage-buddy record signing migration #3 → #2.
4. Revocation-aware friend/PEX verification (consume retire-announce or
   equivalent proof).
(DM signing migration is existing ZEB-580; ZEB-410 gets a note from §2.)

## §10 PR map and gates

Sequential, one open PR at a time, each with its own converge cycle:
S1 trust replication → S2 revoke IPC + UI → S3 retire-announce →
S4 last-seen + petnames → S5 epoch bump → S6 replace flow.
S4 may land before S3 if review timing favors it (no dependency).

Gates per PR: `scripts/test-select --context task|round` iteratively (paste
the `round=…/bucket=…` line), `cargo fmt --all -- --check`, clippy
`--all-targets`, vitest + tsc for UI slices; full
`--workspace --all-targets --features test-fixtures` nextest sweep before
each PR opens. Keychain-touching code only via `*_inner` seams (ZEB-428).
Never bump persisted-file versions for additive fields (`#[serde(default)]`).
