# ZEB-213: Extend identity backup to include owner-state CRDT — design

**Date:** 2026-05-14
**Linear:** [ZEB-213](https://linear.app/zeblith/issue/ZEB-213)
**Related:** [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) (identity binding), [ZEB-176](https://linear.app/zeblith/issue/ZEB-176) (identity backup CLI), [ZEB-184](https://linear.app/zeblith/issue/ZEB-184) (identity backup GUI wizard), [ZEB-206](https://linear.app/zeblith/issue/ZEB-206) (owner-state CRDT / nav tree), [ZEB-211](https://linear.app/zeblith/issue/ZEB-211) (owner-state encryption)

## Goal

A user surviving total bound-device loss can restore **full Harmony state** — identity + nav tree + DM history metadata + read markers + per-DM-Space content keys — from a single offline artifact set. No social recovery, no third-party escrow, no peer dependency for nav-tree recovery.

Closes the gap in today's recovery story: ZEB-176/184 restore identity, but the restored device wakes to an empty nav tree because owner-state CRDT blocks live exclusively on the owner's bound devices — peers never replicate them (only the encrypted root-CID pointer is published, per ZEB-211). Total bound-device loss = total nav-tree loss unless the backup carries the state.

## Why this is harmony-client-only

The HRMR envelope from ZEB-176 lives upstream in the `harmony-owner` crate and is portable across all harmony clients (harmony-arch, harmony-os, harmony-glitch, harmony-stq8). Owner-state CRDT is a harmony-client concept — other clients don't have it. Extending HRMR would force ecosystem-wide format coordination for a harmony-client-private feature. A sidecar envelope (`HRSS`) defined in harmony-client keeps the boundary natural.

If other harmony clients later grow an owner-state CRDT, they can adopt HRSS via a coordinated PR — out of scope here.

## Architecture

Sidecar pair, both encrypted at rest with the same passphrase (`HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`, shared with HRMR).

```
recovery.bin          HRMR envelope    ~101 bytes
  └─ payload: 32-byte master seed                  (unchanged — ZEB-176)

recovery.bin.state    HRSS envelope    ~5–50 MB typical
  └─ payload: canonical-CBOR(OwnerStateSnapshot)   (NEW — ZEB-213)
       ├─ spaces[]      (folders, communities, channels, DMs)
       ├─ outbox[]      (sent-DM index — no message bodies)
       ├─ inbox[]       (received-DM index — no message bodies)
       ├─ markers{}     (per-space last_read_at)
       └─ dm_keys{}     (per-DM-Space content keys)
```

### What recovers from the artifact alone

- **Identity**: Ed25519 + ML-DSA + X25519 + ML-KEM (derived deterministically from the seed, per ZEB-176).
- **Nav tree**: folders, community IDs, channel IDs, DM Spaces with members, public-channel subscriptions.
- **DM history metadata**: "Alice sent me a message at time T with content CID C" — restored device knows the conversation happened, can scroll to it, and can decrypt the body when the CAS blob is fetchable.
- **Read markers**: per-space `last_read_at` HLCs, so unread counts are accurate post-restore.
- **DM content keys**: per-DM-Space symmetric keys for decrypting message bodies retrieved from CAS.
- **Custom names + notification prefs**: owner-local UX state.

### What recovers from peers post-restore (engine auto-fetches, no special restore-time wait)

- **Newer owner-state updates**: if any surviving bound device is still publishing to `harmony/owner/{addr}/state-root-v1`, Flow A's CRDT merge picks it up within seconds of engine boot. Snapshot HLC and live publish HLC determine per-field winners (last-writer-wins with tombstones winning).
- **Community membership state**: for each community in `spaces[]`, the engine subscribes to `harmony/community/{id}/membership` and re-fetches the signed-event log from any surviving member. Requires ≥1 other live member of the community (essentially guaranteed for non-trivial communities; tiny private community with total-loss-of-all-members is unrecoverable in v1 — documented).
- **DM message bodies in CAS**: best-effort fetch when scrolled into view. If no peer caches a particular `message_cid`, the row shows "Message unavailable" — content keys are restored, so as soon as a peer surfaces the body, decryption succeeds.

### Out of scope for v1

- **Per-community membership CRDT bundling.** Re-fetch from peers is the v1 strategy. Future-work for the tiny-private-community-total-loss edge case.
- **DM message body bundling.** Bodies fetch best-effort from CAS. Future-work if body-loss becomes a real complaint.
- **Auto-regeneration of sidecar on mutation.** User-triggered + staleness warning is the chosen cadence.
- **Forward secrecy under bound-device compromise.** Orthogonal — tracked in ZEB-211 future-work.
- **Cross-client HRSS adoption.** Other harmony clients (harmony-arch, harmony-os) get HRSS only when/if they grow an owner-state CRDT.

## Wire format

### HRSS envelope byte layout

Mirrors HRMR (same primitives, distinct magic for domain separation).

```
┌────────────────────────────────────────────────────────────────┐
│ HEADER (37 bytes, plaintext)                                   │
├────────────────────────────────────────────────────────────────┤
│  4   "HRSS"                magic                               │
│  1   0x01                  envelope version (HRSS-v1)          │
│ 16   <random>              Argon2id salt                       │
│ 12   <random>              XChaCha20-Poly1305 nonce            │
│  4   <u32 BE>              KDF params marker (m=64MiB,t=3,p=1) │
├────────────────────────────────────────────────────────────────┤
│ CIPHERTEXT (variable, AEAD-encrypted)                          │
├────────────────────────────────────────────────────────────────┤
│      canonical-CBOR(OwnerStateSnapshot)                        │
│      └─ Poly1305 tag appended by AEAD                          │
└────────────────────────────────────────────────────────────────┘

AAD passed to AEAD: b"harmony-owner-state-snapshot-v1"
```

KDF parameters (m=64 MiB, t=3, p=1) and AEAD primitive (XChaCha20-Poly1305) are identical to HRMR — implementation can reuse the existing envelope helpers in `harmony_owner::recovery` if the API exposes them with a configurable magic/AAD, otherwise a local fork in `state_snapshot.rs` is acceptable. The "configurable magic + AAD" decision is a planning-time call.

### `OwnerStateSnapshot` payload structure (canonical-CBOR per RFC 8949 §4.2)

```text
OwnerStateSnapshot = {
  "v":    uint,                                // = 1; bump if snapshot shape changes
  "addr": bstr(16),                            // owner address; binds snapshot to one identity
  "at":   HLC,                                 // export-time HLC; drives GUI staleness UI
  "tree": canonical-CBOR(OwnerState),          // exactly owner_state_persist::canonicalize() output
}

HLC = { "wall_ms": uint, "logical": uint, "device_id": tstr }
```

Canonical CBOR rules (RFC 8949 §4.2): bytewise-sorted map keys, shortest-form integers, definite-length collections, no tags, no floats.

### Bindings load-bearing

- **`addr`**: bound at export time, verified at restore time against the HRMR-derived identity. Prevents accidentally pairing one user's HRMR with another's HRSS sidecar (or a deliberately-mismatched malicious pair).
- **`at`**: drives the GUI staleness warning by comparing against the current owner-state's most-recent `updated_at` HLC.
- **`v`**: snapshot format version. Lets the inner shape evolve (e.g., add fields, change CBOR layout) without bumping the envelope magic.

### Backwards compatibility

- **Pre-ZEB-213 HRMR files**: restore exactly as before (no sidecar lookup, owner-state empty post-restore). No upgrade path needed.
- **Identity-only backup**: a user can deliberately strip or skip the `.state` sidecar to share identity-only with a trusted operator without leaking nav-tree data. Supported via `--no-state` (export) and `--ignore-state` (restore) flags.

## CLI surface (extends ZEB-176)

```text
harmony-app export recovery-file --out PATH [--comment STR] [--no-state]
                                            ─────────────────────────────
                                            NEW flag: skip sidecar emission
                                            (identity-only backup)

harmony-app restore recovery-file --in PATH [--force] [--ignore-state]
                                             ─────────────────────────
                                             NEW flag: ignore sidecar
                                             even if present at PATH.state

# Mnemonic flows: UNCHANGED. 24 BIP39 words = identity-only forever.
harmony-app export mnemonic
harmony-app restore mnemonic --mnemonic-file PATH [--force]
```

### Export semantics

| Condition | Behavior |
|---|---|
| `--no-state` passed | Emit `PATH` (HRMR) only. Identity-only. |
| Owner-state file exists at `~/.harmony/owner_state.cbor` | Emit `PATH` (HRMR) + `PATH.state` (HRSS). Both written atomically (tmp→rename). |
| Owner-state file does NOT exist (fresh install, no nav-tree activity yet) | Emit `PATH` only + stderr note: `no owner-state to bundle; emitted identity-only backup` |
| `PATH.state` already exists and `--force` NOT passed | Refuse: `Error: state sidecar already exists at <PATH.state>; pass --force to overwrite` |
| HRSS write fails after HRMR succeeded | Best-effort cleanup of HRMR; report both errors. Atomic-pair semantics (both succeed or neither persists). |

### Restore semantics

| Condition | Behavior |
|---|---|
| `PATH.state` exists, `--ignore-state` NOT passed | Read HRMR → restore seed. Read HRSS → AEAD-verify → CBOR-decode → verify `addr` matches restored identity → write `tree` to `~/.harmony/owner_state.cbor` atomically. Stderr: `restored identity-hash: <hex>\nowner-state snapshot: N spaces, exported <ago>` |
| `PATH.state` exists, `--ignore-state` passed | Restore identity only; warn `state sidecar found but ignored per flag` |
| `PATH.state` does NOT exist | Restore identity only; stderr: `no state sidecar found at <PATH.state>; nav tree will be empty post-restore (or sync from surviving peers)` |
| HRSS `addr` does not match HRMR-derived identity | **Hard fail.** `Error: state sidecar identity mismatch — HRSS addr <X> != restored identity <Y>` |
| HRSS decrypts but CBOR is malformed | **Hard fail.** Operator-actionable; should not silently degrade. |
| HRSS `v` field is unknown (future version) | **Hard fail.** `state snapshot format version N not supported; please update harmony-app` |
| HRSS wrong passphrase | AEAD tag rejected. Same error idiom as HRMR (no fingerprinting). |
| `~/.harmony/owner_state.cbor` already exists and `--force` NOT passed | Refuse with same idiom as HRMR refusal. `--force` overwrites atomically. |

### Error reporting

Pass-through `SnapshotError::Display` strings, same style as `RecoveryError` in ZEB-176. `WrongPassphraseOrCorrupt` is deliberately ambiguous (AEAD does not distinguish the two cases).

### Passphrase env-var resolution

The HRSS envelope uses the **same** passphrase as HRMR — resolved via `HARMONY_RECOVERY_PASSPHRASE` (literal) or `HARMONY_RECOVERY_PASSPHRASE_FILE` (path to passphrase file), in that order. Reuses the resolver added in ZEB-176 (`recovery_cli` module). One passphrase to remember; if either env var is missing, both export and restore fail with the same actionable error as today. No new env var introduced.

## GUI surface (extends ZEB-184 wizard)

### Export wizard

- Existing flow unchanged: passphrase entry → format choice (mnemonic / recovery-file) → emit.
- Adds **"Include nav tree + DM history"** checkbox to the recovery-file branch, **default ON**.
- Below checkbox: live size estimate ("Snapshot will be ~12 MB") computed from the on-disk owner-state file size.
- Footer of completion screen lists both artifacts: `Wrote recovery.bin (101 bytes) and recovery.bin.state (12.4 MB)`.

### Restore wizard

- Existing flow unchanged: file picker → passphrase → confirm-overwrite → restore.
- After file picker, if `<picked>.state` exists, surface "Found owner-state snapshot — restore both? [yes/no]" (yes = default).
- After restore, completion screen lists what was restored: `Identity restored. Nav tree restored: 47 spaces, last exported 3 days ago.`

### Staleness warning (new — surfaces in Settings → Backup section)

```
┌─────────────────────────────────────────────────────────────┐
│  ⚠ Your backup is 23 days old                                │
│                                                              │
│  You've made changes since your last backup. Communities     │
│  joined, DMs sent, and folder organization will be lost if   │
│  you can't access this device.                               │
│                                                              │
│  [ Export new backup ]   [ Dismiss for 7 days ]              │
└─────────────────────────────────────────────────────────────┘
```

**Trigger:** `current_owner_state.last_mutation_wall_ms > last_backup.at.wall_ms + 14*86_400_000` (14 days in ms).
- `current_owner_state.last_mutation_wall_ms` is derived at IPC-call time by scanning the live `OwnerState` and taking `max(entry.updated_at.wall_ms)` across `spaces[]`, `outbox[]`, `inbox[]`, and `markers{}`. O(N) per call where N = total entries; called only when the GUI checks staleness (typically once per app launch + on dismissal expiry). The implementation may opt to cache this as a single derived field on `OwnerState`, refreshed on every write — that's an optimization, not a correctness requirement.
- `last_backup.at` is read from `~/.harmony/last_backup.json`.
- Comparison is on `wall_ms` only (not full HLC ordering) — staleness is a UX heuristic, not a CRDT correctness invariant; `wall_ms` is precisely the right granularity for "how long ago".

**State tracking:** `~/.harmony/last_backup.json` — small JSON file written on every successful `export recovery-file`. Schema: `{ "at": HLC, "include_state": bool, "out_path": string }`.
**Dismissibility:** "Dismiss for 7 days" stores a `dismissed_until` timestamp in `localStorage`. No nag spam — banner reappears after the dismiss window if still stale.
**Edge cases:**
- Fresh install (no `last_backup.json`): banner does not appear until the user has made at least one CRDT mutation.
- `--no-state` export: `include_state: false` in `last_backup.json` → banner still uses state-age threshold (identity-only backups don't reset state staleness).

## Restore flow (step-by-step)

### Happy path — total device loss, fresh machine

```text
1. User installs harmony-app on a fresh machine.
2. Runs:  harmony-app restore recovery-file --in /usb/recovery.bin
3. CLI reads HRMR → derives seed → writes to OS keychain (or identity.enc fallback)
4. CLI detects /usb/recovery.bin.state → reads HRSS
5. HRSS AEAD-verifies + AEAD-decrypts using HARMONY_RECOVERY_PASSPHRASE
6. CBOR-deserializes OwnerStateSnapshot → verifies addr matches restored identity
7. Writes tree blob to ~/.harmony/owner_state.cbor (atomic tmp→rename)
8. Stderr: "restored identity-hash: <hex>
            owner-state snapshot: 47 spaces, exported 3 days ago"
9. User launches harmony-app GUI.
10. Engine boots, loads owner_state.cbor → NavService sees 47 spaces.
11. Engine subscribes to harmony/owner/{addr}/state-root-v1 — if any
    surviving device of this owner is publishing, Flow A merges in
    newer state within seconds (CRDT semantics handle conflicts).
12. For each community in spaces[], engine subscribes to its membership
    topic and re-fetches the signed-event log from other members.
13. UI is fully populated. DM history visible (metadata + content keys);
    message bodies fetch best-effort from CAS as scrolled into view.
```

### Partial-loss path — one device dead, another alive

```text
1. Same restore flow as above on the fresh machine.
2. Surviving device of the same owner is online and has been publishing.
3. At engine boot, the state-root subscription receives a publish with
   HLC > snapshot.at. Flow A merges → restored device sees the newer state.
4. CRDT semantics handle conflict naturally: tombstones win,
   last-writer-wins per-field on HLC. Snapshot state from N days ago
   doesn't resurrect anything deleted in the meantime.
```

## Failure modes

| Failure | Behavior |
|---|---|
| HRSS file missing at `PATH.state` | Restore identity only. Engine boots with empty owner-state. Flow A may populate if surviving peers exist; otherwise user starts fresh nav-tree. |
| HRSS present but wrong passphrase | AEAD tag rejected. Same error idiom as HRMR: `wrong passphrase or corrupted recovery file`. Restore aborts before any disk write. |
| HRSS `addr` field doesn't match HRMR-derived identity | Hard fail. Prevents accidentally pairing HRMR + HRSS from different owners. |
| HRSS CBOR malformed after decrypt | Hard fail with actionable diagnostic. Indicates upstream corruption or wrong-version sidecar. |
| HRSS `v` field is unknown (future version) | Hard fail: `state snapshot format version N not supported; please update harmony-app` |
| HRSS snapshot HLC older than newly-published peer state | No special handling. Flow A's CRDT merge resolves naturally. |
| Owner-state file already exists at `~/.harmony/owner_state.cbor` (operator re-running restore) and `--force` NOT passed | Refuse with same idiom as HRMR refusal. `--force` overwrites. |
| Export attempted but no owner-state file on disk | Emit HRMR only + stderr note. Operator-actionable; non-fatal. |
| Export size > soft-warning threshold (100 MB) | Emit normally. Stderr: `warning: snapshot is N MB; consider exporting to local disk before USB stick`. No hard cap. |
| HRSS write fails mid-pair (HRMR already written) | Best-effort cleanup of HRMR. Atomic-pair semantics. |

## Locked-in YAGNI calls

- **No journal / no auto-snapshot in v1.** User-triggered + staleness warning is the cadence. Future-work if real users hit staleness pain.
- **No DM message body bundling in v1.** Bodies fetch best-effort from CAS post-restore.
- **No community CRDT bundling in v1.** Re-fetch from peers.
- **No selective export.** Backup is all-or-nothing for state. `--no-state` is a Boolean opt-out, not a selective filter.
- **HRSS is harmony-client-specific.** No cross-client adoption in v1.
- **No size cap, only soft warning.** Power users with 500MB of CRDT state know what they're doing.

## Components & code organization

### New Rust modules (`src-tauri/src/`)

- **`state_snapshot.rs`** (new): HRSS envelope encode/decode. `encode_snapshot(state: &OwnerState, identity: &OwnerAddr, passphrase: &[u8]) -> Result<Vec<u8>, SnapshotError>` and `decode_snapshot(bytes: &[u8], passphrase: &[u8]) -> Result<OwnerStateSnapshot, SnapshotError>`. `SnapshotError` enum mirroring `RecoveryError` style.
- **`backup_state.rs`** (new): `last_backup.json` read/write + staleness logic. `should_warn_about_stale_backup(now_hlc, last_backup_path) -> bool`.

### Modified Rust modules

- **`src-tauri/src/recovery_cli.rs`** (existing — ZEB-176): extend `export_recovery_file_cli` and `restore_recovery_file_cli` signatures to accept `include_state: bool` / `ignore_state: bool` flags. Add `export_state_sidecar` and `restore_state_sidecar` helper functions (delegate to `state_snapshot.rs`).
- **`src-tauri/src/main.rs`** (existing): add `--no-state` flag to the `Export::RecoveryFile` subcommand and `--ignore-state` flag to the `Restore::RecoveryFile` subcommand. Wire to `recovery_cli` helpers.
- **`src-tauri/src/lib.rs`**: register the two new modules; add Tauri IPCs `get_backup_staleness() -> { is_stale: bool, days_since: u32 }` and `mark_backup_dismissed_for_days(days: u32)` for the GUI staleness warning.
- **`src-tauri/src/identity.rs`** (existing — ZEB-176): no changes; the seed read/write API is reused.

### New frontend modules

- **`src/lib/components/BackupStalenessWarning.svelte`** (new): the banner. Subscribes to a derived store fed by `get_backup_staleness()` IPC. Renders only when `is_stale` AND not dismissed.
- **`src/lib/backup-service.ts`** (new): thin TS wrapper over the two new IPCs. Mirrors the service pattern in `community-service.ts`. Always extract errors via `e instanceof Error ? e.message : String(e)` per Tauri convention.

### Modified frontend components

- **`src/lib/components/IdentityPanel.svelte`** (existing — ZEB-184; the single state-machine UI for both backup and restore flows): add the "Include nav tree + DM history" checkbox to the file-export branch of the backup state machine (default ON); update completion screen to list both artifacts when sidecar emitted; add sidecar-detection step in the restore state machine ("Found owner-state snapshot — restore both?") between the `fileEntry` and `fileDecrypted` phases.
- **`src/App.svelte`** (existing): mount `BackupStalenessWarning.svelte` as a top-level banner (visible globally, not gated behind opening IdentityPanel — the goal is to nag users who *aren't* currently in the backup flow).

## Testing strategy

### Unit tests

`src-tauri/src/state_snapshot.rs`:

1. `hrss_envelope_round_trip` — encode + decode a small `OwnerStateSnapshot` with known seed/passphrase → assert byte-identical state after decode.
2. `hrss_addr_binding_rejects_cross_identity` — encode HRSS for owner A, attempt to decode under restored identity B → AEAD verify succeeds (passphrase is right) but `addr` check rejects.
3. `hrss_wrong_passphrase` — encode HRSS, decode with different passphrase → AEAD tag rejected.
4. `hrss_unknown_version_rejected` — synthesize an HRSS with `v: 99` → decode fails with version-unsupported error.
5. `hrss_canonical_cbor_stability` — encode the same `OwnerState` value twice (deterministic-nonce variant for the fixture), assert byte-identical HRSS output.

`src-tauri/src/backup_state.rs`:

6. `staleness_warning_triggers_after_14_days` — synthesize `last_backup.json` with `at` 15 days old → `should_warn_about_stale_backup` returns true; 13 days → false.
7. `staleness_warning_handles_missing_file` — no `last_backup.json` AND no CRDT mutations → false; mutations present → true.
8. `dismiss_window_suppresses_warning` — `last_backup.json` is 30 days old, dismiss timestamp set to 5 days from now → false; dismiss timestamp 1 day in past → true.

`src-tauri/src/recovery_cli.rs` (extended from ZEB-176 set):

9. `export_emits_pair_when_state_exists` — tempdir with seed + owner-state file → export → both files exist, both decode round-trip.
10. `export_emits_solo_when_no_state` — tempdir with seed but no owner-state file → HRMR only, stderr contains expected note.
11. `export_no_state_flag_skips_sidecar` — owner-state exists but `--no-state` passed → HRMR only.
12. `export_refuses_when_sidecar_exists_without_force` — `recovery.bin.state` already exists → errors with refusal.
13. `export_hrss_write_fails_cleans_up_hrmr` — simulate HRSS write failure → HRMR is cleaned up, both errors reported.
14. `restore_emits_pair_round_trip` — full pipeline: generate seed + state → export pair → wipe both → restore pair → identity hash matches AND `owner_state.cbor` byte-identical.
15. `restore_ignores_missing_sidecar` — only HRMR present → restore succeeds with empty state, stderr warning shown.
16. `restore_ignore_state_flag_skips_sidecar` — both files present but `--ignore-state` passed → state file untouched, `owner_state.cbor` empty.
17. `restore_force_overwrites_existing_state_file` — `owner_state.cbor` already exists → without `--force` refuses, with `--force` overwrites atomically.
18. `restore_addr_mismatch_hard_fails` — pair HRMR for owner A with HRSS for owner B → restore aborts before writing anything.

### Integration tests (`src-tauri/tests/identity_state_recovery_integration.rs`)

1. `mnemonic_round_trip_still_works_unchanged` — regression: ZEB-176 mnemonic flow byte-identical with ZEB-213 in place.
2. `recovery_file_round_trip_with_state` — full export+restore pair preserves identity_hash + `owner_state.cbor` byte-equality.
3. `legacy_hrmr_only_restores_with_empty_state` — synthesize a pre-ZEB-213 HRMR file → restore succeeds with empty owner-state (backwards compat).
4. `cross_machine_state_restore` — emit HRMR + HRSS on one tempdir-rooted "machine", restore on a fresh tempdir → engine boots and owner-state matches.
5. `partial_loss_merges_with_peer` — two paired engines using the existing `PairedEngines::bootstrap` fixture pattern (per ZEB-285 integration test scaffolding); one snapshots + restores fresh, the other publishes newer HLC → restored device merges via Flow A within bounded latency. If the existing fixture cannot reach the state-root publish path within Phase 1 scope (verified during planning), the test is split out as a Phase 2 follow-up ticket — but Phase 1 must attempt the integration. The other four integration tests are non-negotiable.

### Wire-format fixture pinning

`src-tauri/tests/wire_format_zeb213_fixtures.rs`:

- One byte-pinning fixture for HRSS envelope with a deterministic small `OwnerStateSnapshot` (using `test-fixtures` feature for determinism).
- One byte-pinning fixture for the `OwnerStateSnapshot` canonical-CBOR payload (independent of the AEAD layer).

### Frontend tests

`src/lib/components/__tests__/BackupStalenessWarning.test.ts`:

- Renders when staleness > 14 days.
- Hides when staleness ≤ 14 days.
- "Dismiss for 7 days" button updates `localStorage` marker and hides the banner.
- "Export new backup" CTA triggers the export wizard.

`src/lib/__tests__/backup-service.test.ts`:

- `get_backup_staleness` IPC error is normalized via `e instanceof Error ? e.message : String(e)`.
- `mark_backup_dismissed_for_days` IPC pass-through.

## Acceptance criteria

1. `harmony-app export recovery-file --out PATH` emits `PATH` (HRMR, unchanged) AND `PATH.state` (HRSS) when an owner-state file exists locally.
2. `harmony-app export recovery-file --out PATH --no-state` emits only `PATH` (HRMR).
3. `harmony-app restore recovery-file --in PATH` auto-detects `PATH.state` and restores both identity and owner-state.
4. `harmony-app restore recovery-file --in PATH --ignore-state` restores identity only even if sidecar exists.
5. Round-trip: export pair → wipe identity + owner-state → restore pair → derived identity_hash matches original AND `owner_state.cbor` byte-identical.
6. HRSS `addr` mismatch with restored identity hard-fails with actionable error.
7. HRSS unknown version (`v != 1`) hard-fails with actionable error.
8. HRSS wrong passphrase fails with the same idiom as HRMR (no fingerprinting).
9. Pre-ZEB-213 HRMR-only files restore successfully with empty owner-state (backwards compat).
10. GUI Export wizard offers "Include nav tree + DM history" toggle, default ON; completion screen lists both artifacts with sizes.
11. GUI Restore wizard offers "Found owner-state snapshot — restore both?" prompt when sidecar detected.
12. GUI Settings → Backup surfaces staleness warning when `current_owner_state.last_modified_hlc > last_exported_at + 14 days`; staleness warning is dismissible for 7 days.
13. CLI surface documented in `docs/headless-install.md` with worked examples (paired export, identity-only export, paired restore, identity-only restore).
14. All five CI gates pass: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
15. No upstream `harmony-owner` crate changes required (HRSS lives entirely in harmony-client).

## Migration considerations

- No data migration. Pre-ZEB-213 HRMR backups continue to work unchanged.
- Fresh installs work normally — HRSS is only emitted if an owner-state file exists.
- A user upgrading harmony-app past ZEB-213 does not need to re-export their existing HRMR; they can, however, run a fresh `export recovery-file` to get a paired backup.

## References

- [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) — owner→device identity binding (parent context)
- [ZEB-176](https://linear.app/zeblith/issue/ZEB-176) — identity backup/restore CLI (HRMR envelope, env vars, --force semantics — directly reused)
- [ZEB-184](https://linear.app/zeblith/issue/ZEB-184) — identity backup/restore GUI wizard (extended here)
- [ZEB-206](https://linear.app/zeblith/issue/ZEB-206) — owner-state CRDT (the thing being snapshotted; explicitly identifies ZEB-213 as followup #5)
- [ZEB-211](https://linear.app/zeblith/issue/ZEB-211) — owner-state encryption (HKDF tree from master seed unlocks snapshot post-restore — load-bearing for the design)
- ZEB-206 spec § "Failure modes" — `All bound devices corrupted → Hard recovery via ZEB-173 identity backup (extend backup to include owner-state CRDT root — followup ticket)` — this spec implements that followup.
