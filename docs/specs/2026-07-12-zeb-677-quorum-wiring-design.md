# ZEB-677: Quorum enrollment + revocation wiring (lost-master story) — Design

**Ticket:** ZEB-677 (ZEB-668 §9 follow-up 1). **Parent spec:** `docs/specs/2026-07-11-zeb-668-device-management-design.md`.
**Approved by Jake 2026-07-12** (design Q&A: full crypto cutoff sliced in; pre-armed co-sign window for enrollment).

## §0 Ground truth (surveyed 2026-07-12, main `83f5839c`)

The K=2 quorum machinery is **fully implemented and unit-tested in the external `harmony-owner` crate** (git dep, `src-tauri/Cargo.toml:109`, rev `8b870ae0`) and **completely unwired in the client**:

- `EnrollmentIssuer::{Master, Quorum}` (`certs/enrollment.rs:25-38`). Quorum = `{ signers: Vec<[u8;16]>, signatures: Vec<Vec<u8>> }`, signed payload tagged `issuer_kind: 0|1`. `EnrollmentCert::verify()` does **structural checks only** for Quorum (size ≥ 2, parity, distinctness) — full signature verification is delegated to `OwnerState::add_enrollment` (`state.rs:211-282`), which walks back to the signers' own enrollments. That delegation is the root cause of peer rejection: a peer does not hold the presenting owner's `OwnerState`.
- `enroll_via_quorum` (`lifecycle/enroll_quorum.rs:13`) — zero client callers. Emits `auto_vouch_certs` in the **new-device-vouches-siblings** direction; a quorum-enrolled device is `Provisional` until a sibling ratifies (crate test `enroll_quorum.rs:222-253`). `quorum_signing_payload()` exists (`state.rs:421`).
- `RevocationIssuer::Quorum` exists **structurally but inert**: no constructor mints it; `RevocationCert::verify()` returns `QuorumRevocationNotImplemented` (`certs/revocation.rs:169`); `OwnerState::add_revocation` rejects it (`state.rs:389-391`).
- "90-day" = the **active-signer window** (`trust.rs:4` `DEFAULT_ACTIVE_WINDOW_SECS`), not archival. Trust threshold `N_VOUCH_THRESHOLD_V1 = 1` (`trust.rs:6`).
- **8 client verifier seams** all hard-assert `issuer == Master`: shared helper A `iroh_friend_acceptor::verify_enrolled_device` (`:770`, reject `:785`; consumers: friend handshake both directions, PEX via `referral_catalog.rs:257/:310`), shared helper B `community_membership::enrolled_key_from_cert` (`:1412`, reject `:1425`; consumers: membership materialization `:3033`, invites `community_invite.rs:1530/:1845`, open-join `open_join_admit.rs:195`), plus **6 inline duplicates**: `iroh_butler_acceptor.rs:538`, `iroh_community_relay_acceptor.rs:243` and `:430`, `profile_card_broadcast.rs:166`, `community_invite.rs:1999`, and the retire-cert-pair verifier `community_membership.rs:1460-1499` (rejects Quorum enrollment `:1472` AND Quorum revocation `:1492`).
- 3 inline tests pin the rejection (`iroh_friend_acceptor.rs:2201`, `community_membership.rs:12283`, `profile_card_broadcast.rs:1138`). The retire-path Quorum arms have **no dedicated test** — silent-behavior gap to close.
- Master capability = `LoadedOwnerState.master_seed.is_some()` (`owner_state.rs:411`), surfaced to the frontend only as `OwnerStateView.can_back_up` (`owner_commands.rs:401`). No `selfIsMaster` field exists.
- Ceremony seams: pairing is Zenoh-over-LAN SAS (`pairing/`), single signing decision point `state_machine.rs:747-775` (`master_seed.expect(...)` → `sign_enrollment_for_joiner`); revocation issuer branch is the pure planner `plan_revocation` (`owner_commands.rs:112-180`, `notMaster:` error at `:156-160`); fleet-sync substrate `FleetSyncEngine` (`fleet_sync.rs`), datasets registered `event_loop.rs:1884-1962`, donor pattern `owner_trust_sync.rs` (owner-trust-v1). S5 epoch bump `plan_fleet_epoch_bump` (`owner_commands.rs:191-261`) master-signs the fleet-keys-v1 doc.
- UI: `DevicesPanel.svelte` has the banner idiom (`epoch-banner` `:752`), seed-gated affordances with honesty copy (`canBackUp` gating `:816-841`, `:940-945`), typed-confirm `RemoveDeviceDialog.svelte`; ENROLL payload already ships the full owner state (`owner_state_cbor_hex`), so a quorum cert's signer certs are derivable joiner-side without ENROLL wire changes.

## §1 Goals / non-goals

**Goals**

1. An owner whose master seed is unavailable (destroyed, wiped, cold-stored elsewhere) can, with **K=2 surviving active devices**, still: revoke a device, enroll a new device, and rotate fleet keys — the full lost-master story.
2. Peers accept quorum-issued certs everywhere master-issued certs are accepted today, via **depth-1 chain carriage** (§2).
3. All quorum actions are explicitly consented on the co-signing device; UI copy follows the honesty rule (§8).

**Non-goals**

- No change to the SAS pairing wire protocol phases (one new SM state, same transport).
- No quorum-signs-quorum chains: quorum **signers must hold Master-issued certs** (depth-1). A fleet that has quorum-enrolled devices but zero master-issued ones cannot mint further quorum certs — recorded in the honesty ledger (§8).
- No K configurability: K=2 fixed (crate constant honored).
- No social recovery / escrow (Harmony identity recovery stays A+C: offline artifact or fresh-identity-on-loss). Quorum is intra-fleet only.
- Vine/storage/DM signing migrations remain ZEB-678/679/580; revocation-aware friend/PEX proof consumption remains ZEB-680.

## §2 Verification architecture: depth-1 chain carriage

A quorum-issued cert is meaningless without its signers' certs, so it travels as a **bundle**:

```text
QuorumSignerCerts = Vec<EnrollmentCert>   // exactly the certs for issuer.signers, each Master-issued
```

**Crate additions (harmony-owner, S1):**

```rust
impl EnrollmentCert {
    /// Full verification of a Quorum-issued cert against the provided signer
    /// enrollment certs. Checks: structural quorum validity (existing verify());
    /// every signer id has a matching provided cert; each signer cert is
    /// Master-issued, same owner_id, and passes verify(now); each quorum
    /// signature verifies against its signer's enrolled ed25519 key over
    /// quorum_signing_payload(). Master-issued certs reject this call.
    pub fn verify_quorum_with_signers(&self, signers: &[EnrollmentCert], now_secs: u64)
        -> Result<(), CertError>;
}

impl RevocationCert {
    /// Canonical detached-signature payload for quorum revocation
    /// (version, owner_id, target, issued_at, reason, issuer_kind=2).
    pub fn quorum_signing_payload(owner_id: &[u8;16], target: &[u8;16],
        issued_at: u64, reason: &RevocationReason) -> Vec<u8>;
    /// Assemble from independently collected detached signatures.
    pub fn assemble_quorum(owner_id: [u8;16], target: [u8;16], issued_at: u64,
        reason: RevocationReason, parts: Vec<([u8;16], Vec<u8>)>) -> Result<Self, CertError>;
    pub fn verify_quorum_with_signers(&self, signers: &[EnrollmentCert], now_secs: u64)
        -> Result<(), CertError>;
}
```

`EnrollmentCert` gains the same detached-signature assembly path (payload fn exists at `state.rs:421`; expose it on the cert type + an `assemble_quorum` mirroring the revocation one) because the ceremony collects signatures across devices — `enroll_via_quorum`'s all-keys-local signature doesn't fit the distributed flow. `OwnerState::add_revocation` accepts Quorum with the same signer-policy checks as `add_enrollment`'s quorum path (≥2 distinct, enrolled, not revoked, active-window, not backdated).

**Client chokepoint (S2):** new module `src-tauri/src/enrollment_verify.rs`:

```rust
/// The ONE issuer-policy decision point for peer-presented enrollment certs.
/// Master → existing self-contained verify. Quorum → verify_quorum_with_signers
/// against the presented bundle. Returns the enrolled device pubkeys on success.
pub fn verify_enrollment_any_issuer(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],       // empty for Master-issued
    expected_owner: Option<&[u8;16]>,
    now_secs: u64,
) -> Result<PubKeyBundle, EnrollmentVerifyError>;

/// Same decision point for revocation certs (SelfDevice | Master | Quorum).
pub fn verify_revocation_any_issuer(
    cert: &RevocationCert,
    target_enrollment: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    now_secs: u64,
) -> Result<(), EnrollmentVerifyError>;
```

All 8 seams route through these (each maps `EnrollmentVerifyError` to its local error type), retiring the 6-way inline duplication. The retire-pair verifier's issued-at-time expiry semantics (`community_membership.rs:1475`) are preserved via the `now_secs` parameter.

**Bundle assembly (presentation side):** a device presenting its own quorum cert pulls its signer certs from the local trust doc by `issuer.signers` ids (the ENROLL payload's `owner_state_cbor_hex` already delivers them to a quorum-enrolled joiner). New helper `own_cert_bundle(&OwnerState, &EnrollmentCert) -> Vec<EnrollmentCert>`.

**Wire changes (all additive, `#[serde(default)]`, empty = Master-issued as today):** friend-handshake request/accept payloads, profile-card struct, butler deposit frame, relay deposit/pull frames, membership `DeviceAnnounce`/`Join` event cert carriage, retire-announce entry, invite structs (admin-bootstrap, join-event, inviter-enrollment). Old peers ignore the new field and keep rejecting quorum certs exactly as today — quorum certs interoperate only between updated builds (§8).

## §3 The quorum-request dataset: `owner-quorum-req-v1`

New `FleetSyncEngine` instance (donor `owner_trust_sync.rs`; registered alongside the other datasets in `event_loop.rs:1884-1962`), KeyTree-encrypted like owner-trust-v1.

```rust
pub struct QuorumReqDoc {
    /// Pending co-sign requests, keyed by request id (16-byte random, hex).
    pub requests: BTreeMap<String, QuorumRequest>,
    /// Pre-armed enrollment windows, keyed by armer device-id hex.
    pub enroll_arms: BTreeMap<String, EnrollArm>,
}

pub struct QuorumRequest {
    pub kind: QuorumRequestKind,           // Revocation { target, reason, epoch_doc } | Enrollment { new_device_pubkeys, .. }
    pub initiator: [u8; 16],
    pub issued_at: u64,                    // cert timestamp — part of the signed payload
    pub created_at: Hlc,                   // LWW for metadata merge
    pub expires_at_ms: u64,                // 24h for revocation, arm-window for enrollment
    /// device-id hex → one detached signature PER constituent payload of the
    /// request kind (Revocation: the RevocationCert payload + optionally the
    /// epoch-doc hash, §7; Enrollment: the enrollment payload). One approval
    /// action on the co-signer yields all of them. Grow-only union.
    pub signatures: BTreeMap<String, QuorumRequestSigs>,
    pub declined_by: BTreeSet<String>,     // grow-only; any decline tombstones the request
}

pub struct EnrollArm { pub armed_until_ms: u64, pub set_at: Hlc }

pub struct QuorumRequestSigs {
    pub primary_sig_hex: String,           // over the cert canonical payload (revocation or enrollment)
    pub epoch_doc_sig_hex: Option<String>, // over the bundled next-epoch doc hash (Revocation kind only, §7)
}
```

Merge: per-request LWW on metadata (`created_at`), **grow-only union** on `signatures` and `declined_by`; `enroll_arms` per-cell LWW (same shape as `FleetNetPetname`). Expired and declined requests are pruned locally by a sweep on apply. Completion is initiator-driven: when the initiator observes ≥ K=2 valid signatures (its own included), it assembles the cert and applies it through the existing authoritative path (`mutate_trust_state` → `add_revocation`/`add_enrollment`), which is idempotent — the request is then pruned. If the initiator dies mid-flow the request expires and the user retries; no orphaned half-certs (the trust doc is the only authority).

IPCs (each `#[tauri::command]` + `_inner` seam + RPC mirror; Rust params snake_case as below, JS callers pass camelCase — Tauri's IPC layer auto-converts at the boundary, per CLAUDE.md):

```text
request_quorum_revocation(device_vk_hex: String, reason: String) -> Result<String, String>  // returns request id
cosign_quorum_request(request_id: String) -> Result<(), String>
decline_quorum_request(request_id: String) -> Result<(), String>
arm_quorum_enrollment() -> Result<u64, String>     // arms 15-min window; returns armed_until_ms
disarm_quorum_enrollment() -> Result<(), String>
```

Pending requests + arm state are exposed on `OwnerStateView` (additive fields: `quorumRequests: QuorumRequestView[]`, `quorumArmedUntilMs`, plus `selfIsMaster` — see §5); the dataset's `on_applied` emits a new `owner-quorum-updated` Tauri event (idiom: `owner-devices-updated`).

## §4 Quorum revocation ceremony (async)

1. **Initiate (device A, `master_seed` absent):** DevicesPanel sibling-row "Remove…" currently hides behind `canBackUp`. It becomes visible when a quorum is possible — **A itself and at least one other active sibling (excluding the target) hold Master-issued certs** (depth-1: both signers must; the initiator is the first signer); the dialog keeps the typed-confirm tier and gains copy: "This device doesn't hold your master key. Your other devices will be asked to co-sign." Confirm → `request_quorum_revocation`: planner validates target (reuse `plan_revocation`'s guards: `lastDevice:`, `unknownDevice:`, `badDeviceVk:`), builds the canonical payload, signs with A's device key, writes the request (A's signature attached), `notify_dirty` + `flush_now`.
2. **Co-sign (sibling B):** `owner-quorum-updated` → DevicesPanel banner (idiom of `epoch-banner`): "Co-sign request from *A-petname*: remove *target-petname* (reason)". Approve = click-confirm (the destructive typed-confirm already happened on A) → `cosign_quorum_request` verifies the request payload locally, signs, unions its signature. Decline → tombstones the request everywhere.
3. **Assemble + apply (A):** on observing K=2 signatures, A assembles `RevocationCert` via `assemble_quorum`, applies through `revoke_device_inner`'s existing pipeline (trust mutate → flush → retire nudge → `owner-devices-updated`). The retire-announce deposit attaches the signer-cert bundle (§2 wire change) so communities can verify.
4. **Co-signers vouch nothing here**; revocation needs no trust ratification.
5. **Epoch bump:** bundled into the same request — see §7. No second co-sign round-trip.

Self-revoke and master-revoke paths are untouched.

## §5 Quorum enrollment ceremony (pre-armed window)

1. **Arm (sibling B):** DevicesPanel gains "Approve adding a device" (visible when `!selfIsMaster`, B holds a Master-issued cert, and ≥1 other active Master-certed sibling exists to act as inviter — the affordance is only for master-less fleets; master-holding fleets use normal pairing). Tapping arms a **15-minute, single-use** window (`arm_quorum_enrollment`), shown with a countdown + "Cancel"; it auto-disarms on first co-sign or expiry. New `OwnerStateView.selfIsMaster` (= `master_seed.is_some()`) drives this; `canBackUp` keeps its existing meaning.
2. **Pair (device A, inviter, no master):** normal SAS flow. At the signing decision point (`state_machine.rs:747`), `StartInviter` now carries `quorum_ctx: Option<QuorumEnrollCtx>` instead of requiring `master_seed`: if the seed is absent and an unexpired `EnrollArm` from an online sibling exists, the SM enters a new bounded state **`AwaitingQuorumCosign`** (timeout 120 s): A builds the enrollment quorum payload over the joiner's pubkeys, signs it, writes an `Enrollment` request; B — armed — **auto-co-signs on apply** (no second manual step; the arm was the consent) and also mints a `VouchingCert{stance: Vouch}` for the new device (this is the sibling ratification that lifts the joiner from `Provisional` to `Full` under `N_VOUCH_THRESHOLD_V1 = 1`).
3. **Complete (A):** on B's signature arriving, A assembles `EnrollmentIssuer::Quorum` cert, `add_enrollment` into the prospective state (the crate's quorum verification runs here), and the ENROLL payload proceeds unchanged — the quorum cert rides the existing `enrollment_cert_cbor_hex` field and the signer certs are already inside `owner_state_cbor_hex`. Fleet-keys handover (ZEB-492/S5 fields) is unchanged.
4. **Joiner:** after ENROLL receipt, mints its own auto-vouches for active siblings (the `enroll_via_quorum` direction, joiner-side since only it holds its signing key); they ride trust-sync.
5. **Failure:** timeout/no-arm/declined → SM error state with informative copy ("Your other device didn't co-sign in time — re-arm and retry"); SAS session torn down cleanly (existing cancel path).

## §6 Verifier rollout across the 8 seams

Every seam switches its inline `matches!(issuer, Master)` (or helper call) to `verify_enrollment_any_issuer` / `verify_revocation_any_issuer`, passing the bundle from the new wire field (empty slice when absent). The 3 rejection-pinning tests flip to acceptance tests for valid bundles and gain rejection cases for: single-signer bundles, duplicate signers, non-Master signer certs (depth violation), signature-mismatch, wrong-owner signer certs, and missing bundle. The retire-pair verifier gains the missing dedicated Quorum tests (both cert positions).

## §7 Quorum-signed fleet epoch bump (full crypto cutoff)

The fleet-keys-v1 carrier doc is master-signed today (`plan_fleet_epoch_bump` seals per-survivor and master-signs). Extension (client-side; carrier readers are own-fleet devices that hold the trust doc, so signer walk-back is natural — no chain carriage needed):

- Doc signature becomes an enum: `Master(sig)` (existing) | `Quorum { signers: Vec<[u8;16]>, signatures: Vec<Vec<u8>> }` over the same doc hash. Readers verify quorum sigs against enrolled device keys from the local trust doc with the same policy checks (≥2 distinct active non-revoked signers).
- **Bundled with revocation:** a quorum revocation request's `kind` carries the pre-built next-epoch doc (initiator generates the new KeyTree, seals to all survivors — excluding the target). B's single approval action produces **two detached signatures** — one over the RevocationCert canonical payload, one over the epoch-doc hash (`QuorumRequestSigs`, §3) — so each artifact verifies independently against its own payload. On assembly, A applies the revoke, installs the new KeyTree, and flushes the quorum-signed carrier doc — mirroring `revoke_device_inner`'s master-path bump (`owner_commands.rs:737-799`), including the no-rollback rule (bump failure leaves the revoke standing; `fleetEpochStale` banner is the retry surface, and the manual `bump_fleet_epoch` IPC gains the quorum path too).
- Until this slice lands, quorum revoke without a bump shows the existing `fleetEpochStale` banner with copy: "Fleet keys not yet rotated — a revoked device may still read fleet-synced data until rotation."

## §8 Honesty ledger (UI copy commitments)

| Claim the UI could imply | Actual truth | Handling |
| --- | --- | --- |
| Quorum works whenever the master is lost | Needs K=2 surviving **active** (90-day window) devices with Master-issued certs | Copy on the arm/remove surfaces: "Requires 2 of your other devices"; fewer → "This fleet can no longer manage devices without the recovery phrase" (fresh-identity floor) |
| Quorum-enrolled devices are full citizens everywhere | Only updated peers accept quorum certs; old builds reject | Release-notes + spec note; no in-app claim of universal acceptance |
| Quorum certs can chain | Depth-1 only: signers must be Master-issued | Arm affordance hidden when no 2 Master-certed active siblings exist |
| Co-sign proves the sibling saw the exact device | Enrollment pre-arm auto-signs whatever request arrives in-window | Arm copy: "For the next 15 minutes this device will approve one new device enrollment started from your other devices" (window auto-closes after one use) |
| Peers detect revoked signers | Peers verify signer certs as presented; post-issuance signer revocation isn't visible at first contact | Same staleness class as Master certs today; ZEB-680 (retire-proof consumption) is the mitigation track |
| Quorum revoke = instant crypto cutoff | True once §7 lands; before that, trust/membership cutoff only | `fleetEpochStale` banner copy (§7); slice ordering documented in ticket |

## §9 Slices → PRs

| Slice | Repo | Content | Gate |
| --- | --- | --- | --- |
| S1 | zeblithic/harmony | `verify_quorum_with_signers` (both cert types), `RevocationCert::sign/assemble/payload` quorum, `EnrollmentCert::assemble_quorum` + payload exposure, `OwnerState::add_revocation` quorum; unit tests incl. depth-1 rejection | crate tests green |
| S2 | harmony-client | rev bump; `enrollment_verify.rs` chokepoint; all 8 seams + wire fields; flip 3 pinned tests; retire-pair quorum coverage | full sweep + flipped tests |
| S3 | harmony-client | `owner-quorum-req-v1` dataset + engine; revocation IPCs; DevicesPanel banner + dialog copy; `selfIsMaster` view field; two-engine integration test (A requests → B co-signs → revoke lands fleet-wide) | sweep + vitest |
| S4 | harmony-client | arm IPCs + `AwaitingQuorumCosign` SM state + joiner-side vouches; pairing SM tests; UI arm surface | sweep + vitest |
| S5 | harmony-client | quorum-signed fleet-keys doc + bundled bump-in-revoke + manual IPC quorum path; banner copy | sweep |

S1 opens first (one PR per repo at a time; the client PRs proceed after the rev bump in S2). Each slice independently green; honest interim states per §8.

## §10 Testing

- **Crate (S1):** quorum sign/assemble/verify round-trips; rejections: <2 signers, dup signers, non-Master signer cert, wrong owner, bad sig, expired signer cert; revocation payload domain-separation from enrollment (`issuer_kind` tag distinct).
- **Seams (S2):** per-seam accept/reject matrix via the chokepoint; serde round-trip for every additive wire field (old-decoder tolerance: missing field ⇒ empty bundle).
- **Ceremony (S3/S4):** two-engine zenoh integration tests on the dataset (donor: trust-sync tests): request/co-sign/assemble happy path, decline tombstone, expiry sweep, initiator-crash retry idempotency; pairing SM unit tests for `AwaitingQuorumCosign` (co-sign arrives / timeout / disarm race).
- **Epoch (S5):** carrier round-trip with quorum signature; reader rejects quorum doc with revoked/inactive signer; bundled revoke+bump applies atomically-enough (revoke stands if bump fails).
- **UI:** vitest for banner render from `quorumRequests`, arm countdown, honesty copy gates (`selfIsMaster`/sibling-count conditions).

## §11 Follow-ups (not this ticket)

- ZEB-680 consumes retire-announce proofs to close the stale-signer window at first contact.
- Quorum-issued cert **renewal to Master issuance** once a master returns (cert "upgrade" so depth-1 capacity regenerates) — file when S4 lands.
- `Stance::Challenge` remains unminted (no UX story yet).
