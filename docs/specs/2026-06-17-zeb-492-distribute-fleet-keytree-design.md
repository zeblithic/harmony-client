# ZEB-492 — Distribute the fleet KeyTree to enrolled (cert-only) devices at pairing

**Date:** 2026-06-17
**Ticket:** [ZEB-492](https://linear.app/zeblith/issue/ZEB-492) (child of ZEB-416; split from ZEB-491 "Gap 2")
**Status:** Design approved 2026-06-17

## Goal

Let a cert-only enrolled device participate in fleet sync and act as a butler by giving it the owner's **fleet `KeyTree`** (the 5 derived AEAD/HMAC keys) — sealed to the joiner during SAS pairing — **without** giving it the master seed. The master seed stays cold: enrolled devices still cannot perform identity recovery or sign new enrollment certs.

## Background / root cause

The entire owner-engine construction block in `start_node` (`src-tauri/src/lib.rs:3730–7607`) is gated on `if let Some(seed) = loaded.master_seed.as_ref()` and opens by deriving `kt = KeyTree::derive(seed)` (`lib.rs:3732`). All fleet engines take `kt: Arc<KeyTree>` as their only seed-derived input.

A paired device is **cert-only** by design (ZEB-197 / ZEB-446): `install_joiner_state` persists with `master_seed = None`. With no seed it can never derive `kt`, so it constructs **none** of the fleet engines and cannot act as a butler (`get_butler_held` 500s "dm-inbox not running"). Because only the original minting device holds the seed, every *secondary* device is cert-only — so cross-device fleet sync (the core ZEB-416 capability) currently works on no device except the first.

### Verified facts (forensic, 2026-06-17)

These were confirmed by direct code reading and gate the whole design:

1. **`loaded.master_seed` is used in exactly one place in all of `lib.rs`: the gate at `lib.rs:3730`.** The bound `seed` is consumed only at `lib.rs:3732` (`KeyTree::derive`). Every other "seed" token in the engine block is either a comment or `ed25519_seed` (`lib.rs:3850`) = the *device* signing key material (`ed25519_private_bytes[32..64]`), which cert-only devices already hold via `device_signing_key`.
2. **The `if let Some(seed)` gate exists solely to obtain the KeyTree.** Nothing inside the block needs the raw seed for anything else.
3. **Seed-only powers live on separate paths, not in this block:** enrollment-cert signing (`pairing/cert.rs::sign_enrollment_for_joiner`, called from `owner_commands.rs:738` and `pairing/state_machine.rs:747`), identity recovery (`recovery_cli.rs`). Each independently requires the seed, so a cert-only device cannot reach them regardless of this refactor.
4. **The `MintSyncEngine` takes `Arc::clone(&kt)`** (`lib.rs:4021`), not the seed. "Mint" here is a fleet dataset engine, not master-key currency signing.
5. **`KeyTree` is a pure deterministic HKDF-SHA256 expansion of the 32-byte master seed** (`owner_state_crypto.rs:130`) into 5 private `Zeroizing<[u8;32]>` sub-keys: `entry_aead`, `root_aead`, `lookup`, `nonce`, `friend_aead`. There is no intermediate "root secret" between seed and KeyTree, and no smaller distributable root exists short of the seed itself.
6. **`friend_aead` is used at runtime** to seal/unseal friend-graph rendezvous secrets (`encrypt_friend_secret`/`decrypt_friend_secret` in `iroh_friend_acceptor.rs`, `pkarr_friend_publisher.rs`, several `lib.rs` sites).

## Approach

Distribute the **serialized 5-key KeyTree material**, sealed in the existing pairing channel, persisted in a new vault slot, epoch-tagged.

### Alternatives rejected

- **Distribute the master seed.** Violates cold-master / no-recovery / no-enrollment-minting. Non-starter.
- **Add an HKDF layer (`seed → fleet_root → KeyTree`) to distribute a smaller "root".** Breaks compatibility with all existing encrypted fleet data (current KeyTree is `HKDF(seed)` directly), forces re-encryption, and buys nothing — the distributed blob is the same size either way.

Because the joiner has no seed, it must receive the KeyTree material directly (no re-derivation possible). To read the *same* existing encrypted datasets it must end up with the *identical* 5 keys.

## Components

### 1. `FleetKeyMaterial` (new — `owner_state_crypto.rs`)

The single auditable serialization surface for KeyTree key material.

```rust
/// Serializable export of a KeyTree's raw key material, for sealed
/// distribution to cert-only enrolled devices (ZEB-492). Carries an
/// explicit `epoch` so a future KeyTree rotation is non-breaking
/// (rotation/re-encryption itself is out of scope — see ZEB-492 §Scope).
///
/// SECURITY: this is the ONLY place KeyTree key bytes leave the type.
/// No `Debug`. All fields `Zeroizing`. Only ever moved through the
/// SAS-sealed pairing channel and the encrypted vault slot.
#[derive(Serialize, Deserialize)]
pub struct FleetKeyMaterial {
    pub epoch: u32,
    entry_aead: Zeroizing<[u8; 32]>,
    root_aead: Zeroizing<[u8; 32]>,
    lookup: Zeroizing<[u8; 32]>,
    nonce: Zeroizing<[u8; 32]>,
    friend_aead: Zeroizing<[u8; 32]>,
}
```

- `KeyTree::to_fleet_material(&self, epoch: u32) -> FleetKeyMaterial` — export; only the seed-holder (inviter) calls it.
- `KeyTree::from_fleet_material(m: &FleetKeyMaterial) -> KeyTree` — reconstruct a KeyTree from distributed material.
- No `Debug` impl on `FleetKeyMaterial`. Fields stay private except `epoch`; construction/extraction only via the two methods above.
- `epoch` is `0` today (matches `HKDF_SALT = b"harmony-owner-state-v1-epoch-0"`).

**Round-trip invariant:** `from_fleet_material(to_fleet_material(kt, e))` must produce a KeyTree that encrypts/decrypts byte-identically to `kt` (the keys are copied, not re-derived).

### 2. Boot gate refactor (`lib.rs:3730`)

Obtain the KeyTree from either source, then run the **identical** engine block:

```rust
let kt: Option<Arc<KeyTree>> = if let Some(seed) = loaded.master_seed.as_ref() {
    // Minting device: seed is authoritative. Any stored fleet material is ignored.
    Some(Arc::new(KeyTree::derive(seed).map_err(|e| format!("KeyTree::derive: {e}"))?))
} else if let Some(material) = loaded.fleet_keytree.as_ref() {
    // Cert-only enrolled device given a fleet KeyTree at pairing.
    Some(Arc::new(KeyTree::from_fleet_material(material)))
} else {
    // Cert-only device that never received a KeyTree (e.g. paired before this
    // shipped, or delivery failed). Graceful fallback = today's behavior: no
    // fleet engines. Surfaced via the existing transport/observability path.
    None
};

if let Some(kt) = kt {
    // ... existing engine block (lib.rs:3730-7607), unchanged ...
}
```

The block body is unchanged; only how `kt` is obtained changes. The `seed` binding is no longer in scope inside the block (it was only used to derive `kt`), so the block compiles against `kt` exactly as before.

### 3. Pairing payload (`pairing/state_machine.rs`, `pairing/persist.rs`)

- Extend `JoinerEnrollResult` with `fleet_keytree: Option<FleetKeyMaterial>`.
- The inviter holds the seed at cert-signing time (`master_seed` is in scope at `state_machine.rs:747`). Immediately after signing, it derives `KeyTree::derive(master_seed)`, calls `to_fleet_material(0)`, and sets the field on the `JoinerEnrollResult` it assembles for the joiner.
- The payload rides the **existing** transport: `JoinerEnrollResult` is sealed with XChaCha20-Poly1305 under the SAS-derived session key (`pairing/session.rs::encrypt`, key from `pairing/sas.rs`). This is the same channel that already carries the joiner's device signing key, so adding the KeyTree material introduces **no new transport trust assumption**.
- The joiner's `on_encrypted_payload` already deserializes `JoinerEnrollResult` and calls `install_joiner_state`; the new field flows through unchanged at the wire layer.

### 4. Persistence (`identity.rs`, `owner_state.rs`, `pairing/persist.rs`)

- Add `VaultSlot::FleetKeytree` (`identity.rs:1279`).
- `install_joiner_state_inner` (`persist.rs:38`) persists `result.fleet_keytree` (when `Some`) via the same `load_secret`/save path used for the device key and seed (keychain or per-profile encrypted-file vault). The serialized `FleetKeyMaterial` (CBOR) is the secret payload.
- `load_owner_state` (`owner_state.rs:386`) reloads it into a new `LoadedOwnerState.fleet_keytree: Option<FleetKeyMaterial>` field (load is best-effort: absent slot → `None`, not an error).
- The inviter (seed-holder) does **not** persist a separate fleet KeyTree — it re-derives from the seed on every boot. Only cert-only joiners persist material.

## Data flow

**Enrollment (inviter → joiner), one-directional:**

1. Inviter (always a seed-holder — cert signing requires the seed) signs the EnrollmentCert at `state_machine.rs:747`.
2. Inviter derives `KeyTree::derive(seed)`, exports `to_fleet_material(0)`, sets `JoinerEnrollResult.fleet_keytree`.
3. Inviter seals the `JoinerEnrollResult` under the SAS session key and sends it.
4. Joiner decrypts, deserializes, and `install_joiner_state` persists owner state + device key (`master_seed = None`, unchanged) **and** the fleet KeyTree material into `VaultSlot::FleetKeytree`.

**Boot (cert-only device):**

1. `load_owner_state` loads `master_seed = None` and `fleet_keytree = Some(material)`.
2. The gate takes the `from_fleet_material` branch, builds `kt`, and constructs all fleet engines exactly as a seed-holder would.

Cert-only devices can never be inviters (no seed → `sign_enrollment_for_joiner` is unreachable), so the KeyTree only ever flows from a seed-holder. No further propagation path exists.

## Security analysis

- An enrolled device with the KeyTree can read/write **all** fleet datasets and seal/unseal friend-graph rendezvous secrets. **Intended:** it is the owner's own device, a full peer (ZEB-173 / ZEB-197 model). `friend_aead` is included for exactly this reason — a butler that cannot do friend handshakes is half-crippled.
- The enrolled device **cannot**:
  - derive the master seed (HKDF-SHA256 is one-way; only the 5 outputs are shared),
  - sign new enrollment certs (`sign_enrollment_for_joiner` needs the seed → K=2 enrollment quorum preserved),
  - perform identity recovery (needs the seed).
- **Transport:** the material never appears in cleartext on the wire — it is inside the SAS-session-sealed `JoinerEnrollResult`, the same envelope already trusted with the device signing key. At rest it lives in the encrypted vault slot.
- **Memory hygiene:** `FleetKeyMaterial` fields are `Zeroizing`; no `Debug`; the type is moved, not cloned, through pairing and persistence.
- **De-enrollment threat model (documented, deferred):** a de-enrolled device retains fleet read access until a future KeyTree rotation, because the keys it holds remain valid. This ticket does **not** implement rotation. The persisted `epoch` tag makes a future rotation non-breaking. Tracked as future work; `HKDF_SALT` already reserves the `epoch-N` field for it.

## Testing

- **Unit (`owner_state_crypto.rs`):**
  - `KeyTree → FleetKeyMaterial → KeyTree` round-trips to byte-identical `encrypt_entry`/`decrypt_entry`/`space_lookup_key` output (and friend-secret seal/unseal).
  - `from_fleet_material` on material exported from a *different* seed produces a KeyTree that fails to decrypt the first's entries (sanity: material actually carries the keys).
  - CBOR (de)serialize round-trip of `FleetKeyMaterial` preserves all 5 keys + epoch.
- **Integration (pairing):** a paired joiner persists the material via `install_joiner_state`, reloads it via `load_owner_state`, builds a KeyTree, and decrypts an entry the inviter wrote under the same KeyTree.
- **e2e (`s7_butler_deposit_recover`):** flips from characterize-at-boundary-0b to a full **HELD → RECV → CLEARED** assert — the cert-only joiner B2 now constructs its dm-inbox/fleet engines and can hold deposits headlessly. Cross-WAN Scenario D3 (needs AVALON) remains the authoritative end-to-end proof.
- **Gates:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --features test-fixtures`; plus the e2e build + `s7` run (not in CI).

## Scope

**In scope:** `FleetKeyMaterial` type + KeyTree export/import; boot-gate refactor; `JoinerEnrollResult` extension + inviter populate + joiner persist; `VaultSlot::FleetKeytree`; `LoadedOwnerState.fleet_keytree`; epoch tag; tests above.

**Out of scope (deferred):** KeyTree rotation / re-encryption of fleet datasets; any de-enrollment / device-removal trigger; rotating the `epoch` beyond `0`. The two ZEB-491 items (Gap-1 intermittent persist, `fleet_net_enrolled` boot-snapshot staleness) already shipped in ZEB-491 and are not revisited here.

## Affected files

- `src-tauri/src/owner_state_crypto.rs` — `FleetKeyMaterial`, `KeyTree::to_fleet_material` / `from_fleet_material`, unit tests.
- `src-tauri/src/lib.rs` — boot-gate refactor at ~3730.
- `src-tauri/src/owner_state.rs` — `LoadedOwnerState.fleet_keytree`, load in `load_owner_state`.
- `src-tauri/src/identity.rs` — `VaultSlot::FleetKeytree`.
- `src-tauri/src/pairing/state_machine.rs` — `JoinerEnrollResult.fleet_keytree`, inviter populate.
- `src-tauri/src/pairing/persist.rs` — `install_joiner_state` persists material; integration test.
- `e2e-harness/` — `s7_butler_deposit_recover` upgraded from characterize to assert.
