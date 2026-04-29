# Track B v2 — Pairing UI (LAN-symmetric, cert-only) — ZEB-197

**Status:** Design approved 2026-04-28. Linear: ZEB-197 (parent: ZEB-169 Track A umbrella).

**Predecessor:** ZEB-170 (Track B v1 — DevicesPanel + mint + backup). Builds on the persisted `OwnerState` infrastructure shipped there.

**Goal:** Make multi-device coexistence actually work in the UI. After this ships, the user can put two harmony-client instances on the same LAN and pair the second device to the first device's owner identity, ending up with both devices visible in each device's DevicesPanel.

## What changes from v1

- **DevicesPanel empty state** grows a second CTA: "Join existing identity →" alongside the existing "Bind this device to a new owner identity →".
- **DevicesPanel populated state** replaces the "Pairing UI is coming" placeholder footer with an active "Add another device →" button.
- Both new CTAs open dedicated wizard components.
- Two new Tauri commands (plus two helpers) wire the wizards to a backend pairing state machine that drives the handshake over Zenoh.

## Architectural decisions (recorded for the implementation plan)

- **Master seed never crosses the wire.** The Joiner installs only its own freshly-generated signing key, the EnrollmentCert signed by the Inviter, and a snapshot of `OwnerState` at pairing time. The Joiner's `canBackUp` stays `false`. The single-source-of-master-truth two-tier model from ZEB-170 is preserved.
- **Symmetric, both-sides-active discovery.** Both devices must explicitly enter pairing mode. No idle broadcasting. Avoids the privacy leak of "every fresh-install device announces itself on every LAN it joins."
- **LAN-only via Zenoh mDNS for v2.** Cross-internet pairing is deferred to v3 — adds threat-model complexity (codes can't be confirmed by the same human looking at both screens) without value for the primary use case ("I'm setting up my new laptop next to my old one").
- **SAS over ECDH for confirmation.** 6-digit Short Authentication String derived from an ephemeral X25519 key exchange. Both devices display the same digits; user confirms match on both sides. A passive eavesdropper can't derive the SAS; a MitM would need to fool both screens with matching digits, which is computationally infeasible at 6 digits.
- **No new code in the harmony repo.** The Inviter signs the EnrollmentCert by calling `harmony-owner::certs::enrollment::EnrollmentCert::sign_master` directly (which is `pub` and requires only the master signing key plus the Joiner's PUBLIC pubkey bundle). The full `enroll_via_master` wrapper is NOT used because it also signs auto-vouches on behalf of the new device — which would require the Joiner's PRIVATE key on the Inviter side, an unacceptable security regression. v2 skips auto-vouches entirely (deferred to v3, which adds gossip-on-mutate). X25519/HKDF primitives are already in `harmony-crypto`. All new code lives in `harmony-client`.
- **Stateless sessions.** Pairing sessions are entirely ephemeral — if either side cancels or crashes mid-flow, the session is gone and the user restarts. The only persistence is the post-Enroll write of `EnrollmentCert` + `OwnerState` on the Joiner side, which uses the existing atomic-write contract from ZEB-170.

## Out of scope (filed as v3 follow-ups)

- **Cross-internet pairing** (Zenoh routing beyond LAN; needs different SAS coordination).
- **Auto-vouch CRDT propagation.** `enroll_via_master` already returns `auto_vouch_certs` for siblings; v2 throws these away. v3 will ship gossip-on-mutate so subsequent enrollments and vouches propagate across paired devices.
- **K-quorum recovery UI** (the "I lost the master device, but I have N other paired devices" path). Different surface entirely.
- **Revocation UI.** Separate concern.
- **QR / camera-based pairing.** Different transport, not currently a fit for desktop-first harmony-client.

---

## Architecture

### Two roles in the pairing flow

- **Inviter** = OLD device, the one with `master_seed` on disk (`canBackUp == true`). Enters pairing mode from the populated DevicesPanel.
- **Joiner** = NEW fresh-install device with no owner identity bound. Enters pairing mode from the empty-state DevicesPanel.

Both are explicit, user-driven mode entries. Pairing mode is opt-in and ephemeral.

### Three layers per device (mirrors v1 pattern)

- **UI layer** (Svelte): `PairingInviter.svelte` and `PairingJoiner.svelte`. Multi-step wizards, reuse the existing dialog/wizard CSS pattern from IdentityPanel and DevicesPanel.
- **Service layer** (TypeScript): `pairing-service.ts` wraps the Tauri command surface, subscribes to backend state-transition events, exposes a typed observable to the wizard components.
- **Backend layer** (Rust): `pairing.rs` owns the state machine; `pairing_commands.rs` is the IPC surface. Cert signing on the Inviter side calls `harmony_owner::certs::enrollment::EnrollmentCert::sign_master` directly with `master_signing_key` (loaded transiently from `master_seed` via `RecoveryArtifact::master_signing_key()`) + the wire-received Joiner pubkey bundle. Cert installation on the Joiner side calls `state.add_enrollment(cert, now, active_window_secs)` after verifying the cert's signature against the embedded master pubkey.

### Wire protocol

All messages flow over a Zenoh key-prefix scoped to a per-pairing-attempt session ID:

```text
harmony/pairing/v2/lan/<session-id>/{discover,select,confirm,enroll,cancel}
```

The `<session-id>` is a fresh UUID generated at `start_inviter` / `start_joiner` time. The Zenoh subscription scope is restricted to LAN peers (mDNS-bootstrapped, no remote routing).

| Phase | Wire? | Direction | Payload |
|---|---|---|---|
| 1. **DISCOVER** | yes | both → pairing scope | `{ role: Inviter\|Joiner, ephemeral_x25519_pubkey, display_name, owner_id_if_inviter, session_id }` (plaintext — needed for discovery) |
| 2. **SELECT** | yes | each side → peer | `{ peer_session_id, my_session_id }` — sent when user clicks the peer's row in the discovered list. Plaintext. The "I want to pair with this specific peer" claim. |
| 3. **HANDSHAKE** | no (local) | n/a | Once a side has both SENT a SELECT and RECEIVED the matching SELECT from that peer, it computes locally: `shared = ECDH(eph_sk, peer_eph_pk)`, `session_key = HKDF(shared, "session-v2")`, `sas = format!("{:06}", HKDF(shared, "sas-v2") % 1_000_000)`. UI then renders the SAS. |
| 4. **CONFIRM** | yes | each side → peer | `{ user_confirmed: true }` — sent only after the local user clicks "Yes, codes match." Encrypted under `session_key`. |
| 5. **ENROLL** | yes | inviter → joiner | `{ enrollment_cert, owner_state_snapshot, joiner_advisory_display_name }` — encrypted under `session_key`. |

`CANCEL` is a separate cross-cutting wire message that may be sent at any phase by either side; recipient transitions to Idle.

**Why mutual SELECT before HANDSHAKE:** ensures both users explicitly opted into pairing with this specific peer before any SAS is computed. Prevents the "I clicked one row, they're now in my pairing flow without their consent" pattern.

### Discovery semantics

When a device enters pairing mode (Inviter or Joiner), it:
1. Generates an ephemeral X25519 keypair (held in RAM, dropped on session end).
2. Generates a fresh session ID.
3. Publishes a DISCOVER message on the pairing scope.
4. Subscribes to DISCOVER messages from the OTHER role.
5. On receiving a peer DISCOVER, adds the peer to the discovered list.

The user sees the discovered list in real-time and clicks the peer they want to pair with. The clicked side becomes the "initiating side" for the handshake (sends the first HANDSHAKE message). The other side's matching click is the "confirming side" — both sides must independently click to claim the pairing, which prevents accidental cross-pairing on a busy LAN.

Once both sides have selected each other, the SAS is computed and displayed. Both sides see the same digits because ECDH is symmetric.

### Trust boundary

- **Confidentiality:** the ENROLL payload is encrypted under the ECDH-derived `session_key`. A passive observer of the Zenoh traffic sees ciphertext.
- **Authentication of the peer:** the SAS confirmation is the user's eyes-on-both-screens check. The SAS is derived from the ECDH shared secret, which an attacker cannot compute without one of the ephemeral private keys. A MitM doing two separate ECDH exchanges (one with each side) would result in different SAS digits on each screen, which the user notices.
- **Integrity of the EnrollmentCert:** signed by the Inviter's master signing key (existing `harmony-owner::EnrollmentCert::sign_master` path). The Joiner verifies the signature on receipt before persisting.
- **Non-replay:** session ID is fresh per attempt; the ENROLL message is not stored or replayable across sessions.

---

## Components

### New files

| File | Responsibility |
|---|---|
| `src-tauri/src/pairing.rs` | State machine (`PairingState` enum), Zenoh session driver, ECDH/HKDF/SAS derivation, ENROLL payload assembly and verification. |
| `src-tauri/src/pairing_commands.rs` | Tauri IPC surface: `start_inviter`, `start_joiner`, `select_peer`, `confirm_sas`, `cancel_pairing`, `get_pairing_state` (the latter for UI polling, plus a `pairing-state-changed` Tauri event for push). |
| `src/lib/pairing-service.ts` | Wraps the IPC + event subscriptions; exposes a typed Svelte 5 store + imperative methods (`startInviter`, `startJoiner`, `selectPeer`, `confirmSas`, `cancel`). |
| `src/lib/components/PairingInviter.svelte` | Inviter wizard. States: announcing → discovered list → SAS confirm → enrolling → done. |
| `src/lib/components/PairingJoiner.svelte` | Joiner wizard. States: name-this-device → announcing → discovered list → SAS confirm → installing → done. |
| `src-tauri/tests/pairing_integration.rs` | End-to-end test: two `NodeRuntime`s on isolated Zenoh sessions in one process, run full handshake, assert post-conditions. |

### Modified files

| File | Change |
|---|---|
| `src/lib/components/DevicesPanel.svelte` | Empty state: add a second CTA "Join existing identity →" that opens `PairingJoiner`. Populated state: replace the "ADD ANOTHER DEVICE" placeholder text with an active "Add another device →" button that opens `PairingInviter`. |
| `src-tauri/src/lib.rs` | Add `pub mod pairing; pub mod pairing_commands;` and register the 6 new commands in `tauri::generate_handler!`. |

### Key types (Rust, mirrored 1:1 in TS via serde camelCase)

```rust
pub enum PairingRole { Inviter, Joiner }

pub enum PairingState {
    Idle,
    Discovering { role: PairingRole, ephemeral_pubkey_hex: String, session_id: Uuid },
    Discovered { peers: Vec<DiscoveredPeer> },
    Handshaking { peer_id: [u8; 16], sas_digits: String },  // "012845" not raw bytes
    WaitingPeerConfirm { peer_id: [u8; 16] },
    Enrolling,
    Complete { device_id: [u8; 16] },
    Failed { reason: String },
}

pub struct DiscoveredPeer {
    pub session_id: Uuid,
    pub role: PairingRole,
    pub display_name: String,
    pub owner_id_if_inviter: Option<[u8; 16]>,
    pub ephemeral_pubkey_hex: String,
    pub seen_at_unix: u64,
}
```

### Persistence

None during pairing — sessions are entirely RAM-resident. The Joiner's POST-Enroll persistence reuses the existing ZEB-170 contract:
- Joiner's signing key → keychain (with encrypted-file fallback per `HARMONY_PASSPHRASE`)
- Joiner's `EnrollmentCert` (now in `OwnerState.enrollments`) + full `OwnerState` snapshot → CBOR file via `save_owner_state_atomic`. The `.cbor` is written LAST per the atomicity contract.

---

## Data flow (happy path)

```text
Inviter (OLD)                            Joiner (NEW)
─────────────                            ────────────
DevicesPanel populated
  └─ click "Add another device →"
       └─ PairingInviter wizard
            └─ start_inviter()
                 └─ pairing.rs: gen X25519 ephemeral
                 └─ session_id = fresh UUID
                 └─ publish DISCOVER on pairing scope
                 └─ state = Discovering{Inviter}
                 └─ subscribe to peer DISCOVER

                                              DevicesPanel empty
                                                └─ click "Join existing identity →"
                                                     └─ PairingJoiner wizard
                                                          └─ user types device display name
                                                          └─ start_joiner({name})
                                                               └─ gen X25519
                                                               └─ session_id fresh UUID
                                                               └─ publish DISCOVER (Joiner role)
                                                               └─ state = Discovering{Joiner}
                                                               └─ subscribe

  ◄──── both sides see each other's DISCOVER ────►
  state = Discovered{[joiner_peer]}        state = Discovered{[inviter_peer]}

  UI shows discovered list                  UI shows discovered list
       └─ user clicks the joiner row             └─ user clicks the inviter row
            └─ select_peer(joiner_session)            └─ select_peer(inviter_session)
                 └─ publish SELECT                        └─ publish SELECT
                 └─ wait for peer's matching SELECT       └─ wait for peer's matching SELECT

  ◄──── both sides receive each other's SELECT ────►

  Each side, ON SEEING the matching peer SELECT, computes locally:
       └─ ECDH(eph_sk, peer_pk) → shared
       └─ session_key = HKDF(shared, "session-v2")
       └─ sas_digits = format!("{:06}", HKDF(shared, "sas-v2") % 1_000_000)
       └─ state = Handshaking{sas: "012845"}

  UI shows: "0 1 2  8 4 5"                  UI shows: "0 1 2  8 4 5"
  both show "Codes match?" buttons

       └─ user clicks "Yes, match"               └─ user clicks "Yes, match"
            └─ confirm_sas()                          └─ confirm_sas()
                 └─ publish CONFIRM (encrypted)            └─ publish CONFIRM (encrypted)
                 └─ wait for peer CONFIRM                  └─ wait for peer CONFIRM

       both received peer CONFIRM
       state = Enrolling                         state = Enrolling

       └─ load OwnerState + master_seed
       └─ artifact = RecoveryArtifact::from_seed(master_seed)
       └─ master_sk = artifact.master_signing_key()
       └─ master_pk = artifact.master_pubkey_bundle()
       └─ cert = EnrollmentCert::sign_master(
                   &master_sk, master_pk,
                   joiner_device_id, joiner_pubkey,
                   now, None)
       └─ drop(master_sk) — wipe from RAM
       └─ state.add_enrollment(cert.clone(), now, active_window)
       └─ persist updated OwnerState locally
       └─ encrypt ENROLL payload {cert, full_state_snapshot, name}
            under session_key
       └─ publish ENROLL

                                              ◄──── ENROLL received ───
                                                   └─ decrypt under session_key
                                                   └─ verify cert signature against
                                                       Inviter's master_pubkey from cert
                                                   └─ persist:
                                                       • device_signing_key → keychain
                                                       • OwnerState (with cert) → .cbor LAST
                                                   └─ state = Complete{device_id}
                                                   └─ wizard transitions to "done"
                                                   └─ DevicesPanel re-renders → populated
```

### Key invariants (load-bearing for security review)

- **Session ID is fresh per pairing attempt.** Prevents replay across sessions.
- **SAS is computed from ECDH, not from a pre-shared secret.** Eavesdropper sees ECDH pubkeys but cannot compute the SAS without an ephemeral private key.
- **MitM detection:** under MitM, each side's ECDH uses a different attacker key, so each side computes a different SAS. The user sees mismatched codes and clicks "No, don't match."
- **Master seed never appears in any wire payload.** Verified by integration test asserting captured Zenoh messages contain no bytes equal to the loaded master_seed.
- **Joiner's signing key is generated locally on the Joiner.** Never network-derived. Joiner's identity is rooted in a secret it owns.
- **EnrollmentCert is verified before persistence.** Joiner runs the existing `harmony-owner` cert verification path (master pubkey embedded in the `EnrollmentIssuer::Master` variant, so verifier is self-contained).
- **Display name is advisory.** It's UX metadata, not a security primitive. The cryptographic identity is the device pubkey hash.

---

## Error handling

### Pre-flight (before any network traffic)

| Condition | Behavior |
|---|---|
| Joiner-side: device already has owner identity | "Join existing" CTA disabled with title="This device is already bound to identity X" |
| Inviter-side: master_seed wiped (`canBackUp == false`) | "Add another device" CTA disabled with title="This device cannot enroll others — use a device that holds the master seed" |
| Either side: harmony node not running | Inline error "Start the node first" — same pattern as ZEB-170 mint. The pairing commands invoke `require_node_running` (the inverse of `require_node_stopped` from ZEB-170/ZEB-191). |
| Either side: Zenoh transport unavailable | Inline "Network unavailable — pairing requires LAN connectivity." |

### In-flight

| Phase | Failure | Recovery |
|---|---|---|
| Discovery | No peer found within 60s | UI: "No nearby devices found. Make sure the other device is also in pairing mode and on the same network." Retry button restarts. |
| Discovery | Multiple peers found, user picks wrong one | SAS will mismatch with the actually-paired peer; user clicks "No, don't match" → CANCEL → restart. |
| Handshake | ECDH/HKDF computation error (invalid pubkey) | `Failed{crypto error}`. Suggest restart. |
| SAS confirmation | User says "no match" on either side | CANCEL both sides; both return to Idle. Log the mismatch event as a potential MitM signal (telemetry only — don't block retry). |
| SAS confirmation | One side confirms, other doesn't (90s timeout) | Both go to Idle; the unconfirmed side shows "Pairing timed out — start over." |
| Enroll | ENROLL message lost or decrypt fails | 30s timeout → `Failed{network}`. User restarts. |
| Enroll | Joiner keychain write fails | Honor the keychain-error-preservation pattern from ZEB-170 round 4: surface the real keychain error, don't swallow into a generic message. Joiner stays in `Failed`. The Inviter's `OwnerState` mutation is idempotent (re-pairing same Joiner pubkey is a no-op via existing `add_enrollment` semantics), so retry from the user's restart is safe. |
| Enroll | Joiner partial persistence (signing key written, OwnerState write fails) | Roll back the keychain entry so Joiner doesn't end up half-enrolled. The `.cbor`-written-LAST contract from ZEB-170 is the basis: if `.cbor` write fails, the keychain entry is the orphan, so the rollback removes the keychain entry. |

### Concurrent-pairing edge cases

- **Two Joiners discover the same Inviter:** Inviter's discovered list shows both; user picks one; the other Joiner times out at 60s.
- **Two Inviters in pairing mode on same LAN:** Joiner sees both; user picks based on the display name shown for each.
- **Same-device replay (user accidentally clicks "Add another device" twice while a session is already in progress):** the second invocation rejects with "Pairing already in progress."

### Telemetry signals (no PII)

- Pairing started (role)
- Discovery duration to first peer
- SAS mismatch events (security signal)
- Enroll completion or failure cause

---

## Testing plan

### Backend unit tests (`pairing.rs`)

| Test | Asserts |
|---|---|
| `sas_derivation_deterministic` | Same ECDH inputs always produce the same 6-digit SAS |
| `sas_differs_under_mitm` | Different ephemeral keys (simulating intercepted handshake) produce different SAS on each side |
| `state_machine_happy_path` | Idle → Discovering → Discovered → Handshaking → WaitingPeerConfirm → Enrolling → Complete with mocked Zenoh transport |
| `state_machine_cancel_at_each_stage` | Cancel from any non-terminal state returns cleanly to Idle |
| `inviter_signs_and_installs_cert` | Inviter side calls `EnrollmentCert::sign_master` and `OwnerState::add_enrollment` correctly using the real `harmony-owner` library; resulting state contains the new enrollment |
| `joiner_verifies_cert_before_install` | Joiner rejects a tampered cert (modified bytes or wrong signature) before persisting; only valid master-signed certs reach disk |
| `joiner_persists_state_atomically` | After Complete, on-disk artifacts (keychain entry + .cbor) match in-memory expectations; `.cbor` is written LAST per the ZEB-170 contract |
| `replay_same_pubkey_is_idempotent` | Re-pairing same Joiner pubkey is a no-op on Inviter (relies on existing `add_enrollment` semantics) |

### Backend integration test (`tests/pairing_integration.rs`)

End-to-end: spawn two `NodeRuntime`s on isolated Zenoh sessions in the same process. Mitigate the ZEB-165 UDP-port collision by using distinct ports per runtime (or use Zenoh's in-process transport mode if available). Run the full 4-phase handshake through real `pairing.rs` modules with no UI.

Asserts:
- Both sides reach `Complete`
- Joiner's `OwnerState` after pairing contains EnrollmentCert keyed by Joiner's device_id
- Inviter's `OwnerState` mutated to include the new enrollment
- **Master seed never appears in any wire payload** (assert by snooping the Zenoh messages and verifying none contain bytes matching `master_seed`)

### Frontend tests (`PairingInviter.test.ts`, `PairingJoiner.test.ts`)

Mock `pairing-service.ts`. Per-component:
- Renders empty discovered-peers list initially
- Renders peer rows when service emits `Discovered` state
- Clicking a peer row triggers `service.selectPeer()`
- Renders SAS digits when state transitions to `Handshaking`
- "Yes, match" button disabled until SAS rendered
- Cancel from any state calls `service.cancel()` and dismisses
- `Failed` state renders the error string with a Retry button
- `Complete` state shows success message + closes wizard, triggering DevicesPanel refresh

### DevicesPanel changes (additions to existing `DevicesPanel.test.ts`)

- Empty state renders TWO CTAs (existing + new "Join existing identity →")
- Populated state's "ADD ANOTHER DEVICE" footer renders an active button (regression: not the placeholder text from v1)
- Clicking either CTA opens the corresponding wizard (verify by checking dialog appears)

### Manual acceptance test (run by user before PR merge)

1. On KRILE: launch harmony-client, mint owner identity → DevicesPanel shows KRILE
2. On AVALON (same LAN): launch harmony-client, click "Join existing identity →"
3. On KRILE: click "Add another device →"
4. Verify both screens show the other in their discovered list within ~10s
5. Pick the other on either side → both screens show identical 6-digit SAS code
6. Confirm match on both sides → AVALON's DevicesPanel transitions to populated, showing both KRILE and AVALON
7. Run `get_owner_state` on each side; verify both show 2 devices with same `owner_id` and `enrollments` keyed by the right `device_id`s

### Negative acceptance tests

- Cancel mid-discovery → both return to Idle
- "No, don't match" on SAS → both return to Idle
- Disconnect AVALON from network during Enroll → AVALON sees Failed; KRILE's state mutation is idempotent on retry

---

## References

- **Predecessor:** `docs/specs/2026-04-28-zeb-170-track-b-devices-panel-v1-design.md`
- **harmony-owner protocol:** `crates/harmony-owner/src/certs/enrollment.rs::EnrollmentCert::sign_master` (the actual cert-signing primitive used by Inviter), `crates/harmony-owner/src/state.rs::OwnerState::add_enrollment` (the cert-install primitive used by Joiner), `crates/harmony-owner/src/lifecycle/mint.rs::RecoveryArtifact::{from_seed, master_signing_key, master_pubkey_bundle}` (transient master reconstruction). The high-level `lifecycle::enroll_via_master` wrapper is intentionally NOT used — see "Architectural decisions" above for why.
- **harmony-crypto primitives:** ML-KEM / X25519 / HKDF already shipped
- **Existing transport:** harmony-client `ZenohService` (telemetry path, reusable for pairing pubsub scope)
- **Atomicity contract for persistence:** ZEB-170 round 3+ in `src-tauri/src/owner_state.rs::save_owner_state_atomic`
- **Open follow-ups that intersect:**
  - ZEB-165 — integration test UDP port collision (impacts the integration-test design; mitigation noted above)
  - ZEB-191 — `require_node_stopped` helper duplication (the inverse helper `require_node_running` will likely also benefit from extraction)
  - ZEB-194 — Tauri save-handle capability tokens (orthogonal; no impact on pairing)
  - ZEB-195 — Modal focus trap (the new wizards should adopt the reusable Modal once it lands; if it lands first, this spec gets a follow-up commit; if it lands after, that ticket sweeps DevicesPanel + IdentityPanel + PairingInviter + PairingJoiner together)
