# ZEB-668 S5 — KeyTree epoch bump on revoke: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A master-issued revoke rotates the fleet KeyTree to epoch N+1 so the revoked device — which retains sealed epoch-0 `FleetKeyMaterial` — can no longer decrypt fleet-net/owner-state/trust publishes; survivors receive the new material through a master-signed, epoch-0-keyed `fleet-keys-v1` carrier dataset, with a dual-epoch read window until every active device catches up or 7 days elapse.

**Architecture:** Per spec §6 as amended by §6.1 (ground truth invalidated the trust-doc-carrier and trust-liveness-window mechanisms). Four moving parts: (1) per-epoch HKDF salt in `KeyTree::derive_at_epoch` (epoch-0 output byte-identical, golden-pinned); (2) a runtime-swappable `FleetKeySet` accept-set threaded through the fleet engines (decrypt tries each installed epoch, publish uses newest); (3) a ninth `FleetSyncEngine` dataset `fleet-keys-v1` — permanently keyed by the epoch-0 KeyTree, carrying `FleetKeyEpochDoc { epoch, bump_wall_ms, sealed-per-device blobs, master_sig }`, monotonic + master-signature-verified on merge; (4) bump orchestration hooked into the master-revoke path plus a `bump_fleet_epoch` IPC and a `fleetEpochStale` panel signal.

**Tech Stack:** Rust (hkdf/sha2, ed25519-dalek, x25519 via `dm_signing` seal helpers, ciborium canonical CBOR, tokio), Svelte 5 + vitest, Tauri IPC + api/rpc.rs headless RPC.

## Global Constraints

- Branch `zeb-668-s5-keytree-epoch` off main `17cb7180`; ONE commit per task.
- Epoch-0 derived keys MUST remain byte-identical to today's (`HKDF_SALT = b"harmony-owner-state-v1-epoch-0"`); pinned by golden-vector test written BEFORE the salt refactor.
- Keychain-touching code only via `*_inner`/`_with_store` seams (ZEB-428); never `KeychainStore::new()` in test-reachable code.
- Never bump persisted-file versions for additive fields; vault slot gains multi-material encoding with legacy single-material decode fallback.
- Epoch-0 material is NEVER pruned (it keys the carrier forever); vault invariant = epoch-0 + current (+ previous during window).
- Carrier doc payload is master-signed; receivers accept only strictly-higher epochs with valid signature.
- Window close source = `FleetNetRow.seen_at.wall_ms` (NOT trust-doc liveness); `FLEET_EPOCH_WINDOW_MS = 7 * 24 * 60 * 60 * 1000`.
- Self-revoke does NOT bump; master-revoke always attempts the bump, and bump failure degrades to the staleness signal (never rolls back the revocation).
- Naming: qualify identifiers "fleet epoch"/"FleetKeyEpoch" (collision guard vs transport/tunnel/community epochs).
- Gates per task: `scripts/test-select --context task` (paste `round=…/bucket=…`), `cargo fmt --all -- --check`, clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`; vitest + tsc on the UI task; full `--workspace --all-targets --features test-fixtures` nextest sweep before PR open.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## Locked design decisions (ground-truth survey 2026-07-11)

1. **Carrier ≠ trust doc.** Trust doc wire type is crate-owned `harmony_owner::state::OwnerState` (exhaustively destructured in `merge_trust_remote_into_local`, owner_trust_sync.rs:70-76) — not client-extensible. Carrier is a new client-local dataset (D3 below). Spec §6.1 amendment 1.
2. **Carrier keyed by epoch-0 KeyTree forever.** Every enrolled device holds epoch-0 from pairing; resolves the bootstrap deadlock (blobs under N+1 encryption could never be read by devices needing them to reach N+1); chained bumps safe. Metadata leak to revoked devices accepted + honesty-ledger'd.
3. **Carrier authenticity = master signature + monotonic epoch.** Revoked devices retain epoch-0 and could forge carrier publishes; merge accepts remote only if `remote.epoch > local.epoch` AND master sig verifies. Doc embeds `master_vk`; binding check = `identity_hash(master_vk) == owner_id` if the crate exposes a vk→owner-id helper, else compare `master_vk` against the issuer key in this device's own crate-verified `EnrollmentCert` (resolve at T3 step 1 with one grep into `~/.cargo/git/checkouts/harmony-*/8b870ae/crates/harmony-owner/`).
4. **`KeyTree` gains `epoch: u32`** (plain field, zeroize-skip semantics); `to_fleet_material` stamps `self.epoch` (removes the ZEB-492 "epoch param footgun" by construction); `from_fleet_material` accepts any epoch.
5. **`FleetKeySet`** = `Arc<RwLock<Vec<Arc<KeyTree>>>>` sorted desc by epoch, non-empty invariant; `newest()` for publish, `accept_set()` for decrypt-try-each, `install()`, `retain_newest_only()`. The carrier engine gets a FIXED epoch-0 `Arc<KeyTree>`, never the swappable set.
6. **Vault slot format:** CBOR `Vec<FleetKeyMaterial>` (newest-first), legacy decode falls back to single `FleetKeyMaterial` → 1-vec. `LoadedOwnerState.fleet_keytree` retypes to `Option<Vec<FleetKeyMaterial>>`. Seed-holder load gate unchanged (never reads the slot; carrier replay catches a stale-boot seed-holder up).
7. **Survivor enumeration at bump = enrollments minus revoked** (resident trust doc) — NOT `active_devices()` (a temporarily-offline, non-revoked device must still get a blob or it is orphaned). Zero/invalid `x25519_pub` in a cert → recompute via `ed25519_pub_to_x25519`; if that also fails, ABORT the bump naming the device (honesty over partial bumps).
8. **Sealing:** `dm_signing::seal_to_owner_with_info(recipient_x25519, cbor(FleetKeyMaterial), FLEET_EPOCH_SEAL_INFO)`; open with `ed25519_priv_to_x25519(device signing key)`. Template: `invite_mint.rs:52-76 seal_epoch_key`.
9. **Window close check lives in the `routing_republish` closure** (lib.rs:8070-8168, ~7.5 min cadence): if data accept-set has 2 epochs and (every non-revoked enrolled device's fleet-net `seen_at.wall_ms > bump_wall_ms` OR `now >= bump_wall_ms + FLEET_EPOCH_WINDOW_MS`) → narrow to newest + rewrite vault to {epoch-0, current}.
10. **Staleness signal:** `OwnerStateView.fleetEpochStale` = any revocation cert newer than `carrier.bump_wall_ms` (pre-S5 revocations included — honest). Seed-holder (`canBackUp`) sees the banner + "Rotate fleet keys" button (single-click, busy/error states, no typed confirm — non-destructive and idempotent); other devices get a passive note.
11. **Pairing:** ENROLL payload gains additive `#[serde(default)] fleet_keytree_set_cbor_hex: Option<String>` = CBOR `Vec<FleetKeyMaterial>` [epoch-0, current-if->0]; legacy field keeps carrying epoch-0 alone. Joiner prefers the set. Old-build joiner into a bumped fleet falls out of fleet sync (documented, no version gate).
12. **Bump failure on the revoke path**: `tracing::warn!` + revocation stands + return Ok — the panel's staleness banner is the retry surface (identical UX to the self-revoke case by design).

---

### Task 1: Crypto core — per-epoch derivation, epoch-carrying KeyTree, any-epoch import

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs` (salt const :103-106, `KeyTree` struct :124-130, `derive` :134-164, `to_fleet_material` :175-184, `from_fleet_material` :197-208, tests mod)

**Interfaces:**
- Consumes: existing `HKDF_SALT`, `INFO_*` constants, `FleetKeyMaterial`.
- Produces: `KeyTree::derive_at_epoch(master_seed: &[u8; 32], epoch: u32) -> Result<Self, CryptoError>`; `KeyTree { pub(crate) epoch: u32, … }` (field readable in-crate, e.g. `kt.epoch`); `derive(seed)` ≡ `derive_at_epoch(seed, 0)`; `to_fleet_material()` stamps `self.epoch`; `from_fleet_material` accepts any epoch and sets `epoch` from the material. `CryptoError::UnsupportedEpoch` variant retained (dead-code-allow or repurposed for future format bumps — keep, mark `#[allow(dead_code)]` only if clippy demands).

- [ ] **Step 1: Golden epoch-0 pin test — BEFORE any refactor.** In the tests mod:

```rust
/// ZEB-668 S5: epoch-0 derivation is pinned byte-for-byte. These vectors
/// were captured from the pre-S5 implementation (HKDF_SALT const) and MUST
/// NEVER be regenerated — a mismatch means every existing fleet's keys broke.
#[test]
fn epoch0_derivation_matches_golden_vectors() {
    let seed = [7u8; 32];
    let kt = KeyTree::derive(&seed).expect("derive");
    assert_eq!(hex::encode(&*kt.entry_aead), "<PIN>");
    assert_eq!(hex::encode(&*kt.root_aead), "<PIN>");
    assert_eq!(hex::encode(&*kt.lookup), "<PIN>");
    assert_eq!(hex::encode(&*kt.nonce), "<PIN>");
    assert_eq!(hex::encode(&*kt.friend_aead), "<PIN>");
}
```

Fill `<PIN>` by running once with placeholder values and copying the actual hex from the assertion failure output:
`cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(epoch0_derivation_matches_golden_vectors)'`

- [ ] **Step 2: Run to capture vectors, pin them, re-run to green.** Expected: PASS with real vectors against UNCHANGED derivation code. Commit checkpoint optional (folded into this task's single commit at step 8).

- [ ] **Step 3: Refactor the salt + add `derive_at_epoch` + epoch field (failing tests first for new behavior).** Add tests:

```rust
#[test]
fn epoch_salt_zero_matches_legacy_constant() {
    assert_eq!(epoch_salt(0).as_slice(), b"harmony-owner-state-v1-epoch-0");
}

#[test]
fn distinct_epochs_derive_pairwise_distinct_keys() {
    let seed = [7u8; 32];
    let e0 = KeyTree::derive_at_epoch(&seed, 0).expect("e0");
    let e1 = KeyTree::derive_at_epoch(&seed, 1).expect("e1");
    assert_eq!(e0.epoch, 0);
    assert_eq!(e1.epoch, 1);
    assert_ne!(&*e0.entry_aead, &*e1.entry_aead);
    assert_ne!(&*e0.root_aead, &*e1.root_aead);
    assert_ne!(&*e0.lookup, &*e1.lookup);
    assert_ne!(&*e0.nonce, &*e1.nonce);
    assert_ne!(&*e0.friend_aead, &*e1.friend_aead);
}

#[test]
fn fleet_material_any_epoch_round_trips() {
    let seed = [9u8; 32];
    let kt = KeyTree::derive_at_epoch(&seed, 3).expect("derive");
    let m = kt.to_fleet_material();
    assert_eq!(m.epoch, 3);
    let back = KeyTree::from_fleet_material(&m).expect("epoch 3 must import post-S5");
    assert_eq!(back.epoch, 3);
    assert_eq!(&*back.entry_aead, &*kt.entry_aead);
}
```

Delete `fleet_material_unsupported_epoch_rejected` (:1143) — its pinned behavior is the thing S5 removes; `fleet_material_any_epoch_round_trips` replaces it. Run: expected FAIL (`derive_at_epoch`/`epoch_salt` undefined).

- [ ] **Step 4: Implement.**

```rust
/// Per-epoch HKDF salt (spec §6). Epoch 0 is byte-identical to the legacy
/// `harmony-owner-state-v1-epoch-0` constant — pinned by
/// `epoch_salt_zero_matches_legacy_constant` and the golden-vector test.
fn epoch_salt(epoch: u32) -> Vec<u8> {
    format!("harmony-owner-state-v1-epoch-{epoch}").into_bytes()
}
```

`KeyTree` gains `pub(crate) epoch: u32` (plain copy field — key bytes stay `Zeroizing`). `derive` body moves to `derive_at_epoch(master_seed, epoch)` using `Hkdf::<Sha256>::new(Some(&epoch_salt(epoch)), master_seed)` and stamping `epoch`; `derive(seed)` delegates to `derive_at_epoch(seed, 0)`. `to_fleet_material` stamps `epoch: self.epoch` (update its ZEB-492 doc comment: the footgun is gone because the epoch comes from the tree, not a caller). `from_fleet_material` drops the `!= 0` rejection, sets `epoch: m.epoch`, and updates its doc comment. Update the `HKDF_SALT` doc comment (:103-105) — constant replaced by `epoch_salt`; delete the const or keep it referenced only by the equality test.

- [ ] **Step 5: Run the module's tests.** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_state_crypto)'` — all green, INCLUDING the untouched golden-vector test (proves epoch-0 stability across the refactor).

- [ ] **Step 6: Fix ripples.** `cargo check --locked --all-targets --features test-fixtures` — struct-literal construction sites of `KeyTree` (from_fleet_material was one; grep `KeyTree {` for others) need `epoch`.

- [ ] **Step 7: Task gate.** `scripts/test-select --context task` (paste round/bucket line) + `cargo fmt --all -- --check` + clippy `--all-targets`.

- [ ] **Step 8: Commit** `ZEB-668 S5 T1: per-epoch KeyTree derivation, epoch-0 golden-pinned, any-epoch import`.

---

### Task 2: FleetKeySet runtime accept-set + multi-epoch vault persistence

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs` (add `FleetKeySet`)
- Modify: `src-tauri/src/fleet_sync.rs` (`FleetSyncConfig.kt` :120, `Ctx.kt` :366, decrypt :720-794, encrypt :575/:618)
- Modify: `src-tauri/src/owner_state_sync.rs` (same pattern — locate with `grep -n 'decrypt_root_publish\|encrypt_root_publish\|space_lookup_key\|kt' src/owner_state_sync.rs`)
- Modify: `src-tauri/src/owner_state.rs` (`LoadedOwnerState.fleet_keytree` :406, save/load/clear `fleet_keytree` :1049-1195, load gate :482-494)
- Modify: `src-tauri/src/identity.rs` (vault seams :2041-2133 — only if the stored-value doc comments name the single-material format; the seams are bytes-opaque)
- Modify: `src-tauri/src/fleet_net_persist.rs` / callers as ripples demand
- Modify: `src-tauri/src/lib.rs` (boot gate :4340-4368; engine config sites incl. trust block :5425)
- Modify: `src-tauri/src/pairing/persist.rs` (:83 joiner install → wrap single material in a 1-vec; full set handling lands in T3)

**Interfaces:**
- Consumes: `KeyTree.epoch` (T1).
- Produces:

```rust
#[derive(Clone)]
pub struct FleetKeySet { inner: Arc<std::sync::RwLock<Vec<Arc<KeyTree>>>> }
impl FleetKeySet {
    /// Non-empty by construction.
    pub fn new(first: Arc<KeyTree>) -> Self;
    pub fn newest(&self) -> Arc<KeyTree>;                 // highest epoch — publish key
    pub fn accept_set(&self) -> Vec<Arc<KeyTree>>;        // desc by epoch — decrypt candidates
    pub fn install(&self, kt: Arc<KeyTree>);              // idempotent by epoch; keeps sort
    pub fn retain_newest_only(&self);                     // window close
    pub fn epochs(&self) -> Vec<u32>;
}
```

  plus `owner_state::save_fleet_keytree(keychain, dir, materials: &[FleetKeyMaterial])` / `load_fleet_keytree(…) -> …Option<Vec<FleetKeyMaterial>>` (legacy single-material decode → 1-vec) and `LoadedOwnerState.fleet_keytree: Option<Vec<FleetKeyMaterial>>`.

- [ ] **Step 1: Failing unit tests for `FleetKeySet`** (owner_state_crypto tests mod): newest-picks-highest-epoch; install-idempotent-by-epoch (re-install same epoch does not grow); accept_set-desc-order; retain_newest_only-keeps-one; epochs-lists-desc.
- [ ] **Step 2: Implement `FleetKeySet`; green.**
- [ ] **Step 3: Failing tests for vault multi-material encode/decode** (owner_state.rs tests): round-trip `Vec<FleetKeyMaterial>` of 2; legacy bytes (CBOR of a single material) decode to a 1-vec — construct legacy bytes inline with `ciborium::into_writer(&single, …)`.
- [ ] **Step 4: Implement vault format.** Write path always encodes `Vec`; read path tries `Vec<FleetKeyMaterial>` first, falls back to single. Retype `LoadedOwnerState.fleet_keytree`; load gate (:482-494) semantics unchanged (cert-only loads, seed-holder skips).
- [ ] **Step 5: Thread `FleetKeySet` through the engines.** `FleetSyncConfig { kt: Arc<KeyTree> }` → `keys: FleetKeySet`; `Ctx` likewise. In `handle_incoming_publish`: try `decrypt_root_publish(&kt, &wire)` for each `keys.accept_set()` entry; the FIRST success binds `kt` for the remainder of the function (`space_lookup_key` + `decrypt_entry` at :780-781 use the same epoch's tree — a publish is entirely single-epoch). All decrypt candidates failing → existing Dropped path (log once, not per-candidate). Publish sites use `keys.newest()`. Apply the identical pattern to `owner_state_sync.rs`. Boot (lib.rs:4340): seed path `FleetKeySet::new(Arc::new(KeyTree::derive(seed)?))`; cert path: build from ALL decodable vault materials (`install` each; first = any — sort handles order); the three data engines share ONE `FleetKeySet` clone.
- [ ] **Step 6: Engine round-trip test.** Extend the existing fleet_sync tests (grep `mod tests` in fleet_sync.rs for the donor round-trip test): a publish encrypted under epoch-1 keys decrypts when the receiver's set holds {0,1}, and is Dropped when it holds only {0}.
- [ ] **Step 7: Task gate** (test-select task + fmt + clippy `--all-targets`).
- [ ] **Step 8: Commit** `ZEB-668 S5 T2: FleetKeySet accept-set through fleet engines + multi-epoch vault slot`.

---

### Task 3: fleet-keys-v1 carrier — doc, signing, merge, engine, install path, pairing set

**Files:**
- Create: `src-tauri/src/fleet_key_epoch.rs`
- Modify: `src-tauri/src/lib.rs` (module decl; ninth engine block cloned from the owner-trust block :5422-5463; on-applied install task)
- Modify: `src-tauri/src/pairing/types.rs` (ENROLL payload additive field), `src-tauri/src/pairing/state_machine.rs` (:794-822 inviter side), `src-tauri/src/pairing/persist.rs` (joiner prefers set)

**Interfaces:**
- Consumes: `FleetKeySet` (T2), `derive_at_epoch` (T1), `dm_signing::{seal_to_owner_with_info, open_from_owner_with_info, ed25519_pub_to_x25519, ed25519_priv_to_x25519}`.
- Produces:

```rust
pub const FLEET_KEYS_DATASET: &str = "fleet-keys-v1";
pub const FLEET_KEYS_LOOKUP_TAG: &[u8] = b"fleet-keys-v1";
pub const FLEET_EPOCH_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const FLEET_EPOCH_SEAL_INFO: &[u8] = b"harmony-fleet-epoch-key-seal-v1";
const FLEET_KEYS_SIG_DOMAIN: &[u8] = b"harmony-fleet-keys-v1-sig";

#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FleetKeyEpochDoc {
    #[serde(rename = "e", default)] pub epoch: u32,
    #[serde(rename = "b", default)] pub bump_wall_ms: u64,
    /// device_id_hex → sealed CBOR(FleetKeyMaterial), sealed to that device's x25519.
    #[serde(rename = "s", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sealed: std::collections::BTreeMap<String, Vec<u8>>,
    #[serde(rename = "k", default)] pub master_vk: [u8; 32],
    #[serde(rename = "g", default, skip_serializing_if = "Vec::is_empty")]
    pub master_sig: Vec<u8>,
}

impl FleetKeyEpochDoc {
    fn signing_bytes(&self) -> Vec<u8>;            // domain ‖ canonical CBOR(e, b, s, k)
    pub fn sign(&mut self, master: &ed25519_dalek::SigningKey);
    pub fn verify(&self, expected_master_vk: &[u8; 32]) -> bool;
}

/// Monotonic + authenticated merge: adopt remote iff epoch strictly higher
/// AND signature verifies against the trusted master vk. Returns changed.
pub fn merge_fleet_keys_remote(
    local: &mut FleetKeyEpochDoc,
    remote: FleetKeyEpochDoc,
    expected_master_vk: &[u8; 32],
) -> bool;
```

  `CanonicalPayload` impl mirrors `FleetNetDoc`'s (grep `impl CanonicalPayload for FleetNetDoc` in fleet_net.rs). Trusted-master-vk source resolved at step 1 (locked decision 3).

- [ ] **Step 1: Resolve the trusted-master-vk question** (one grep session into the harmony-owner checkout: `EnrollmentCert.issuer` type, any `identity_hash`/owner-id-from-vk helper, how `add_revocation` verifies master sigs). Record the resolution as a plan-amendment note in this file. Primary design: doc embeds `master_vk`; verifier checks binding against owner_id via crate helper if it exists, else against this device's own enrollment cert issuer key.
- [ ] **Step 2: Failing tests for doc + merge** (fleet_key_epoch.rs tests mod): sign/verify round-trip; tampered `sealed` fails verify; `merge_fleet_keys_remote` adopts strictly-higher signed remote (changed=true); rejects equal epoch, lower epoch, bad sig (changed=false, local untouched); wire-pin test `EXPECTED_FLEET_KEY_EPOCH_DOC_HEX` (canonical CBOR of a fixture doc — "NEVER regenerate" comment, FleetNetDoc-style); empty-doc default encodes without `s`/`g` keys (ciborium::Value assert).
- [ ] **Step 3: Implement doc/sign/verify/merge + CanonicalPayload; green.**
- [ ] **Step 4: Engine block in lib.rs.** Clone the owner-trust block (:5422-5463) as the ninth dataset: topic `…/ds/fleet-keys-v1`, replay file const alongside the trust one, resident `Arc<Mutex<FleetKeyEpochDoc>>` in NodeState, engine keyed by a FIXED epoch-0 tree: seed path `Arc::new(KeyTree::derive(seed)?)`, cert path the epoch-0 entry from the vault set (invariant: always present; if absent — legacy vault predating any bump — the current-epoch entry IS epoch-0). Merger closure = `merge_fleet_keys_remote` with the trusted vk captured. NOTE: the carrier engine's config takes a `FleetKeySet::new(fixed_epoch0)` if T2 changed the config type uniformly — that is fine (a 1-set never swapped).
- [ ] **Step 5: On-applied install task** (donor: `spawn_trust_applied_task`, owner_trust_sync.rs:343-374). On applied carrier doc with `doc.epoch > keys.newest().epoch`: load owner state (per-call, like `revoke_device_inner`); seed path → `derive_at_epoch(seed, doc.epoch)` → `keys.install`; cert path → find own `device_id_hex` blob → `open_from_owner_with_info(ed25519_priv_to_x25519(signing_key), blob, FLEET_EPOCH_SEAL_INFO)` → decode `FleetKeyMaterial` (assert `.epoch == doc.epoch`, warn+skip otherwise) → `from_fleet_material` → `keys.install` + vault rewrite {epoch-0, prev-current, new} via `save_fleet_keytree`; missing own blob → warn (revoked or enrolled-after-bump; pairing covers the latter). Emit `owner-devices-updated`. Unit-test the pure parts (blob open/install decision) with a fixture doc; the task wiring is covered by the T4 integration test.
- [ ] **Step 6: Pairing set handover.** types.rs: additive `#[serde(default)] fleet_keytree_set_cbor_hex: Option<String>` on `EncryptedPayload::Enroll`; state_machine.rs (:794-822): build `Vec<FleetKeyMaterial>` = [epoch-0 material, current-epoch material if resident carrier doc epoch > 0 (derive at that epoch — master seed is in scope)]; persist.rs: joiner decodes the set field when present (install all), else legacy single. Failing test first: pairing tests mod round-trips an Enroll payload with the set field and without it (legacy decode).
- [ ] **Step 7: Task gate + commit** `ZEB-668 S5 T3: fleet-keys-v1 carrier — master-signed monotonic epoch doc, install path, pairing set`.

---

### Task 4: Bump orchestration — IPC/RPC, revoke hook, window close

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (bump inner/impl near `revoke_device_inner` :466-607; hook on the master path after flush :580-591)
- Modify: `src-tauri/src/lib.rs` (`bump_fleet_epoch` tauri command + prod `generate_handler!`; window-close check in `routing_republish` closure :8070-8168)
- Modify: `src-tauri/src/api/rpc.rs` (rpc entry + curated name list, next to `revoke_device` :959-962)

**Interfaces:**
- Consumes: T1–T3 everything; `RecoveryArtifact::from_seed` master reconstruction (template owner_commands.rs:164-173); resident trust doc; resident carrier doc + engine handle; `FleetJoin`-style snapshot idioms.
- Produces:

```rust
/// Pure planning: derive epoch N+1, seal to every survivor, sign.
/// Errors: "notMaster:…" (no seed), "sealFailed:<device_id_hex>" (bad x25519,
/// after ed25519 recompute fallback), derivation/encoding failures.
pub(crate) fn plan_fleet_epoch_bump(
    trust: &harmony_owner::state::OwnerState,
    carrier: &FleetKeyEpochDoc,
    current_data_epoch: u32,
    master_seed: &[u8; 32],
    now_ms: u64,
) -> Result<(FleetKeyEpochDoc, KeyTree), String>;
// new_epoch = max(carrier.epoch, current_data_epoch) + 1
// survivors = trust.enrollments minus is_revoked(); seal cbor(to_fleet_material())
// per survivor; sign with the reconstructed master key.

pub(crate) async fn bump_fleet_epoch_impl(
    state: tauri::State<'_, NodeState>,
    sink: std::sync::Arc<dyn NodeEventSink>,
) -> Result<u32, String>;   // returns the new epoch
```

  plus `#[tauri::command] bump_fleet_epoch` and rpc `"bump_fleet_epoch"` (no args struct needed).

- [ ] **Step 1: Failing unit tests for `plan_fleet_epoch_bump`** (owner_commands tests, reuse `minted_loaded_state()` fixtures + a trust doc with 2 enrollments where 1 is revoked): bumps to max+1; sealed map covers exactly the non-revoked enrollment (revoked absent); each blob opens with that device's x25519 priv and decodes to material at the new epoch; doc verifies against the master vk; no-seed → "notMaster:"; zeroed-x25519 cert with recomputable ed25519 → sealed via fallback; unrecoverable key → Err naming the device.
- [ ] **Step 2: Implement `plan_fleet_epoch_bump`; green.**
- [ ] **Step 3: `bump_fleet_epoch_impl`** (template: `set_device_petname_impl`, lib.rs:55759-55848, minus routing republish): load owner state (seed required), snapshot resident trust doc + carrier + current data epoch (`keys.newest().epoch`), call planner, install new tree into `FleetKeySet`, adopt the new doc into the resident carrier under its lock, `notify_dirty` + `flush_now` (log-warn on flush error, same as petname), emit `owner-devices-updated`, return epoch. Register command in prod `generate_handler!` (next to `revoke_device`) + rpc.rs entry with sink. RPC-registration test mirroring `set_device_petname_rpc_is_registered_and_wired`.
- [ ] **Step 4: Revoke hook.** In `revoke_device_inner`, master path only (`Planned { is_self: false }` — the path that required `master_seed`), after the revocation flush (:589) and before the emit: run the bump inline (the seed and docs are already in scope — call the planner + adopt/flush, NOT the `_impl` which re-loads state). On Err: `tracing::warn!(error, "fleet epoch bump after master revoke failed — panel staleness banner is the retry surface")` and proceed Ok. Self-revoke path untouched. Failing test first: extend the existing revoke tests (grep `mod` tests around revoke in owner_commands.rs) — master revoke leaves resident carrier doc at epoch+1 with the revoked device absent from `sealed`; self-revoke leaves the carrier untouched.
- [ ] **Step 5: Window close in `routing_republish`.** After the existing self-row restamp: snapshot keyset epochs; if data accept-set holds >1 non-carrier epoch → read carrier `bump_wall_ms`, enumerate non-revoked enrollments' fleet-net `seen_at.wall_ms` (rows keyed by device_id_hex, S4 idiom); close when all postdate the bump OR `now_ms >= bump + FLEET_EPOCH_WINDOW_MS`; on close `keys.retain_newest_only()` + vault rewrite {epoch-0, current} (cert devices; seed-holder skips vault). Extract the decision as a pure function + unit tests: `fn fleet_epoch_window_should_close(bump_wall_ms, now_ms, survivor_seen_ms: &[Option<u64>]) -> bool` — all-postdate closes; one-stale holds; missing row holds; 7-day timeout closes regardless.
- [ ] **Step 6: Task gate + commit** `ZEB-668 S5 T4: bump_fleet_epoch IPC/RPC, master-revoke bump hook, 7-day dual-epoch window close`.

---

### Task 5: UI — fleetEpochStale signal + rotate button + panel copy

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (`FleetJoin` gains `carrier_epoch: u32`, `carrier_bump_wall_ms: u64`; `build_owner_state_view` computes staleness; `get_owner_state_inner` snapshots the carrier doc)
- Modify: `src-tauri/src/owner_state.rs` (`OwnerStateView` — locate the struct; add `#[serde(default)] fleet_epoch: u32`, `#[serde(default)] fleet_epoch_stale: bool`; extend the camelCase pin test: `fleetEpoch`, `fleetEpochStale`)
- Modify: `src/lib/owner-service.ts` (`OwnerStateView` += `fleetEpoch: number`, `fleetEpochStale: boolean`)
- Create: `src/lib/fleet-epoch-service.ts` + `src/lib/fleet-epoch-service.test.ts`
- Modify: `src/lib/components/DevicesPanel.svelte` + `src/lib/components/__tests__/DevicesPanel.test.ts`

**Interfaces:**
- Consumes: T4's `bump_fleet_epoch` IPC; S4 view-join idioms (`FleetJoin`, camelCase DTO keys — assertion keys are the serde camelCase names, e.g. `fleetEpochStale`, per the e2e-camelCase hard rule).
- Produces: `fleet-epoch-service.ts`:

```typescript
import { invoke } from '@tauri-apps/api/core';
/** ZEB-668 S5: rotate the fleet KeyTree to the next epoch (seed-holder only). */
export async function bumpFleetEpoch(): Promise<number> {
  return invoke<number>('bump_fleet_epoch');
}
```

- [ ] **Step 1: Rust staleness join, failing tests first** (owner_commands tests, S4 fixture style): revocation issued after last bump → `fleet_epoch_stale: true`; bump newer than every revocation → false; no revocations → false; pre-S5 fleet (carrier default epoch 0, bump 0) with any revocation → true. CHECK UNITS at implementation time: `RevocationCert.issued_at` is seconds (grep the cert struct) vs `bump_wall_ms` — convert explicitly (`issued_at * 1000`), and note the conversion in a comment.
- [ ] **Step 2: Implement join + DTO fields + camelCase pin extension; green.**
- [ ] **Step 3: Frontend service + test** (mirror `device-petname-service.test.ts`: stubs invoke, asserts command name and return).
- [ ] **Step 4: Panel, failing vitest first** (S4 `s4View()` fixture gains `fleetEpoch: 0 as number`, `fleetEpochStale: false as boolean`): stale + `canBackUp` → banner text ("Fleet keys predate the last device removal.") + "Rotate fleet keys" button; click → `bumpFleetEpoch` invoked + refresh; stale + !canBackUp → passive note, NO button; !stale → nothing; bump rejection → `role="alert"` error, busy guard (in-flight click ignored — saveRename idiom).
- [ ] **Step 5: Implement panel block** (banner above the device list; `.error` + `var(--warning)`-family tokens already in app.css — check the style-token allowlist before inventing tokens).
- [ ] **Step 6: UI gate:** `npx vitest run` + `npx tsc --noEmit` + `scripts/test-select --context task` (Rust half) + fmt + clippy.
- [ ] **Step 7: Commit** `ZEB-668 S5 T5: fleetEpochStale signal + seed-holder rotate-fleet-keys action`.

---

### Final gates (before PR)

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] Full sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (~22 min — normal, ZEB-676)
- [ ] `npx tsc --noEmit && npx vitest run`
- [ ] PR: title `ZEB-668 S5: fleet KeyTree epoch bump on revoke — per-epoch derivation, fleet-keys-v1 carrier, dual-epoch window`; body leads with the §6.1 amendments (spec deviation front and center for review); fire `@coderabbitai review` ONCE at open.

## Self-review notes (plan-time)

- Spec §6 bullets covered: per-epoch salt ✓(T1), seal-per-survivor via trust… **amended** to carrier ✓(T3/§6.1), vault install via seams ✓(T2/T3), dual-epoch window ✓(T2 accept-set, T4 close), any-epoch import ✓(T1), no version gate ✓(D11, release-note item for the PR body), self-revoke-no-bump + seed-holder copy ✓(T4/T5).
- Deliberate scope cuts: no re-seal path for a same-epoch blob repair (re-bump covers it); no carrier pruning of old sealed maps (doc replaced wholesale each bump); S6 will reuse the bump on replace.
- Known open item punted to T3 step 1: exact trusted-master-vk source (two concrete branches specified).
