# ZEB-363 — Consolidate setup secrets into a single keychain item

**Status:** Approved 2026-06-08
**Issue:** [ZEB-363](https://linear.app/zeblith/issue/ZEB-363) (Medium, harmony-client)
**Context:** macOS onboarding prompts for keychain access ~4× during setup, even
when choosing "Always Allow." First-impression UX is poor.

## Root cause

macOS keychain ACLs are **per-item** *and* **per-code-identity**. "Always Allow"
whitelists one caller for one specific item; N distinct items ⇒ N prompts. Setup
writes up to **4 distinct keychain items**, loaded sequentially at startup:

| Item (service / account) | Secret | Loader (`lib.rs`) |
|---|---|---|
| `harmony` / `identity` | identity seed (32 B) | `identity::load_or_generate` (`:2377`) |
| `harmony.owner` / `device_signing_key` | device #2 signing key (32 B) | `owner_state::load_owner_state` (`:2450`) |
| `harmony.owner` / `master_seed` | owner identity master seed (32 B) | `owner_state::load_owner_state` (`:2450`) |
| `harmony.client` / `iroh.secret_key` | iroh transport key (32 B) | `iroh_endpoint::load_or_create_secret_key` (`:2742`) |

(The owner `master_seed` item is absent in the cert-only joiner model, so a
joiner sees 3 of these; a full-owner install sees all 4. A further unsigned-dev-
build re-prompt can appear on top of these — see Scope.)

The seed already has a clean `SeedStore` abstraction (trait + keychain impl +
`HRMI` encrypted-file fallback). The iroh and device keys each open their own raw
`keyring::Entry`.

## Goal

Collapse the 4 items into **one keychain item** (one ACL ⇒ at most one prompt per
fresh setup), preserving the encrypted-file fallback and the seed-only recovery
model, with a safe migration from the existing multi-item layout.

## Scope

- **In:** one composite "secret vault" item; migration; versioned encrypted-file
  fallback that also holds the vault; verified-read-back-then-delete of old items.
- **Out:** *across-launch* re-prompting — that is the code-signing / designated-
  requirement-stability axis (**ZEB-328**); an unsigned dev build re-prompts on
  every access regardless of item count. This change reduces fresh-setup
  *distinct-item* prompts to 1 (from 3 for a joiner, 4 for a full-owner install);
  a signed alpha creating its own single item gets ~0. Also out: keychain access
  groups (need entitlement + signing), and deriving
  sub-keys from the seed (deliberately not chosen — keys stay independent).

## Architecture

A single versioned **`SecretVault`** is the unit of storage, owned by the existing
identity storage layer (`identity.rs`) and consumed by the iroh + owner
subsystems.

```rust
/// All process-local secrets, stored as ONE keychain item (and one encrypted
/// file in the headless fallback). Zeroized on drop.
struct SecretVault {
    version: u8,                              // VAULT_VERSION = 1
    seed: [u8; 32],                           // node identity master seed (recovery root)
    iroh_secret_key: Option<[u8; 32]>,        // independent-random transport key
    device_signing_key: Option<[u8; 32]>,     // device #2 signing key
    owner_master_seed: Option<[u8; 32]>,      // owner identity master seed (None for joiners)
}
```

- Serialized with **ciborium** (CBOR), zeroized buffers throughout.
- Stored in the **single existing `harmony`/`identity` keychain item**, replacing
  the raw 32-byte seed blob.
- `SeedStore` generalizes to a `VaultStore` (load/save a `SecretVault` rather than
  a `[u8; 32]`), keeping its keychain → encrypted-file fallback chain.
- `iroh_endpoint::load_or_create_secret_key` and `owner_state`'s device-key
  load/store stop opening their own `keyring::Entry`; they call vault accessors
  (`vault_iroh_key_or_create` / `vault_device_key_or_create`) that lazily generate
  + persist a missing key into the one item. Startup is sequential
  (seed → owner → iroh), so the vault is loaded once and held; no locking.

### Keychain item format + legacy detection

The legacy value is exactly **32 raw bytes** (the seed). The CBOR vault is always
longer and is valid CBOR. On load:

- `len == 32` → **legacy** raw seed → migrate (below).
- else → parse as CBOR `SecretVault`; on parse failure, hard error (corrupt item),
  never silently overwrite.

### Encrypted-file format (`HRMI`) — version bump to 0x02

Current `v0x01`: 13-byte header (`HRMI` + version + kdf params) + salt(16) +
nonce(24) + ciphertext(**seed, 32**) + tag(16) = 101 bytes fixed.

New `v0x02`: identical header/KDF/AEAD, but the protected plaintext is the
**CBOR `SecretVault`** (variable length). Ciphertext length is derived from file
length (`file − header − salt − nonce − tag`), so no explicit length field is
needed (XChaCha20-Poly1305 is a stream cipher). Decode dispatches on the header
version byte:

- `0x01` → decrypt → 32-byte seed → wrap into a `SecretVault { seed, None, None }`.
- `0x02` → decrypt → CBOR `SecretVault`.

Saves always write `0x02`. A `0x01` file is thus read transparently and upgraded
to `0x02` on the next save.

## Migration (failure-safe, idempotent)

Migration is **inline and per-key**, driven by the accessors rather than a single
startup pass. The seed item is read first (a legacy 32-byte seed decodes into a
seed-only vault; a CBOR vault is used as-is). Each app-local key — iroh
(`harmony.client`/`iroh.secret_key`), device signing
(`harmony.owner`/`device_signing_key`), and owner master seed
(`harmony.owner`/`master_seed`) — folds in on first access via its slot accessor:

1. Read the slot from the vault. Present → return it (steady state).
2. Absent → best-effort read the corresponding **legacy single-key item**.
3. If found, set the slot and **write** the vault to `harmony`/`identity`,
   **read it back**, and verify the folded key matches.
4. **Only on verified read-back**, delete that legacy item.
5. If the write or read-back fails → keep the legacy item, log a warning, and
   continue (no data loss; retried next access / next boot).

Fresh installs never migrate: the vault is created once with the seed, and the
three app-local keys are generated lazily into it on first use — all within the
one item.

## Recovery stays seed-only

The mnemonic and exported recovery files encode **only `vault.seed`** (the
app-local keys — iroh, device, owner-master — are regenerable). `read_seed_from_disk*`
returns `vault.seed`; `write_seed_to_disk*` writes the seed into the `seed` field
of a vault (RMW-preserving any existing app-local fields, or creating a seed-only
vault). On the keychain backend this preserves the app-local keys across a
restore; where it must fall back to the encrypted file, they regenerate on next
boot, consistent with the seed-only recovery model.

## Startup wiring

`lib.rs` loads the vault once (at/around `:2377`), then:
- owner-state load (`:2450`) takes the device key from the vault (generating +
  persisting if absent);
- iroh load (`:2742`) takes the iroh key from the vault (likewise).

The `load_or_create_secret_key` / `load_owner_state` signatures gain the vault
handle (or the resolved key); call sites updated accordingly.

## Testing

Mock-keychain (`keyring::mock`) + tempfile unit tests:

- **Fresh install:** no prior items → one vault item created; iroh/device lazily
  added; only the `harmony`/`identity` item is ever opened.
- **Migration (full):** legacy seed + old iroh + old device items → vault folds all
  three; old items deleted; seed/iroh/device round-trip unchanged.
- **Migration (partial):** legacy seed only (no iroh/device yet) → vault with seed;
  keys generated lazily.
- **Verified-read-back gate:** inject a store whose read-back fails → old items are
  **not** deleted; legacy path still works (no data loss).
- **Recovery seed-only:** `read_seed_from_disk` returns `vault.seed`; a recovery
  export contains only the seed; restore yields a seed-only vault.
- **Encrypted-file v1→v2:** a `0x01` file decodes to a seed-only vault; a save
  re-emits `0x02`; `0x02` round-trips the full vault.
- **Corrupt item:** non-32-byte, non-CBOR value → hard error, never overwritten.
- All existing `identity.rs` seed tests continue to pass (seed semantics preserved).

Gates (from `src-tauri/`, per repo policy — `--locked` and `--all-targets` are
load-bearing):

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `cargo check --locked --all-targets --features test-fixtures` (MSRV toolchain)

## Risks

- **Identity-bricking on a bad migration.** Mitigated by verified-read-back-before-
  delete and keep-old-on-failure (no destructive step until the new item is proven
  readable).
- **Format ambiguity.** Mitigated by the `len == 32` legacy check + explicit
  `HRMI` version byte + hard-error on un-parseable vault.
- **Zeroization regressions.** All seed/key buffers stay in `Zeroizing`; CBOR
  scratch buffers are zeroized after encrypt/decrypt.
</content>
