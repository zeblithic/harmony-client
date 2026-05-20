# Mint Phase 2 Sync — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sync the local Mint SQLite ledger across a single user's multiple devices via the existing Zenoh + ContentStore + AEAD infrastructure already deployed for owner-state sync.

**Architecture:** Dedicated `MintSyncEngine` mirroring `owner_state_sync.rs`'s shape — debounced publisher serializes a typed CBOR snapshot of the ledger, encrypts it with a mint-specific lookup key, `put`s to ContentStore, and publishes `{root_cid, hlc}` on Zenoh topic `harmony/owner/{addr_hex}/mint-root-v1`. Subscriber decrypts, fetches the blob, and merges per-row LWW on `updated_at` with soft-delete tombstone propagation for transactions and a per-device account deletion floor for hard-deleted accounts.

**Tech Stack:** Rust (rusqlite, serde, serde_cbor, blake3, chrono, tokio); Zenoh; Svelte 5; Tauri 2.

**Spec:** `docs/specs/2026-05-19-mint-sync-design.md`
**Branch:** `mint-sync` (off post-#145 main; spec at `8f2b428`)

---

## Working conventions

- All Cargo commands run from `src-tauri/`. All `npx` commands run from repo root.
- Tests use `cargo nextest run --locked --features test-fixtures` (per CLAUDE.md).
- Per CLAUDE.md: Tauri IPC params declared `snake_case` in Rust, called `camelCase` from JS.
- Per `feedback_koya_aarch64_builder.md`: aarch64 builds use Koya (M5 Mac), not QEMU. Phase 2 sync is x86_64-developable; aarch64 only matters for release.
- Every task ends with a commit. Do NOT amend; every fix is a new commit. Per CLAUDE.md "Never skip hooks (--no-verify)."
- Per Jake's standing directive: CI is paused; do not propose re-enabling.

---

## Task 1: Schema v2 migration (deleted_at + updated_at columns)

Add the schema v2 columns to all three mint tables and verify the migration is idempotent.

**Files:**
- Modify: `src-tauri/src/mint.rs` (the `apply_migrations` function)
- Test: `src-tauri/tests/mint_integration.rs` (new test)

- [ ] **Step 1: Write the failing integration test**

In `src-tauri/tests/mint_integration.rs`, append:

```rust
#[test]
fn migration_v2_adds_columns_and_backfills() {
    let db_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    // First open: schema v1 lands.
    {
        let mut conn = harmony_app::mint::open_database(&db_path).unwrap();
        harmony_app::mint::apply_migrations(&mut conn).unwrap();
        // Insert v1-shaped rows (without the new columns).
        conn.execute(
            "INSERT INTO accounts (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["acct-1", "Chase", "2026-05-01T00:00:00Z"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?, ?)",
            rusqlite::params!["default_currency", "USD"],
        )
        .unwrap();
    }
    // Second open: v2 migration runs.
    let mut conn = harmony_app::mint::open_database(&db_path).unwrap();
    harmony_app::mint::apply_migrations(&mut conn).unwrap();

    // accounts now has updated_at, backfilled from created_at.
    let updated_at: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            ["acct-1"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(updated_at, "2026-05-01T00:00:00Z");

    // settings now has updated_at, backfilled to non-empty.
    let setting_updated: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!setting_updated.is_empty());

    // transactions has the deleted_at column (NULL by default).
    conn.execute(
        "INSERT INTO transactions (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            "tx-1", "2026-05-01", "-12.34", "USD", "acct-1", "Coffee",
            "2026-05-01T00:00:00Z", "2026-05-01T00:00:00Z"
        ],
    )
    .unwrap();
    let deleted_at: Option<String> = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            ["tx-1"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(deleted_at, None);

    // Idempotency: a third migration is a no-op.
    harmony_app::mint::apply_migrations(&mut conn).unwrap();
}
```

- [ ] **Step 2: Run the test and verify it fails**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(migration_v2_adds_columns_and_backfills)'
```

Expected: FAIL (likely either compile error if `apply_migrations` isn't `pub(crate)` enough, or a SQL error about missing columns).

- [ ] **Step 3: Extend `apply_migrations` in `src-tauri/src/mint.rs`**

Find the existing `apply_migrations` function and add the v2 migration block AFTER the existing `CREATE TABLE IF NOT EXISTS` statements but BEFORE the function's `Ok(())`:

```rust
// --- Schema v2 (Phase 2 sync) ---

// transactions.deleted_at — tombstone column for soft-delete.
let _ = conn.execute(
    "ALTER TABLE transactions ADD COLUMN deleted_at TEXT NULL",
    [],
);
conn.execute(
    "CREATE INDEX IF NOT EXISTS idx_tx_deleted_at ON transactions(deleted_at)",
    [],
)?;

// accounts.updated_at — backfilled from created_at for legacy rows.
let _ = conn.execute(
    "ALTER TABLE accounts ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
    [],
);
conn.execute(
    "UPDATE accounts SET updated_at = created_at WHERE updated_at = ''",
    [],
)?;

// settings.updated_at — backfilled to migration time for legacy rows.
let _ = conn.execute(
    "ALTER TABLE settings ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
    [],
);
let now = chrono::Utc::now().to_rfc3339();
conn.execute(
    "UPDATE settings SET updated_at = ?1 WHERE updated_at = ''",
    rusqlite::params![now],
)?;
```

The `let _ = conn.execute("ALTER TABLE ...")` pattern tolerates "column already exists" errors on second-run idempotency (rusqlite returns an error, we ignore it, the column is already there).

- [ ] **Step 4: Verify the test passes**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(migration_v2_adds_columns_and_backfills)'
```

Expected: PASS.

- [ ] **Step 5: Verify no existing tests broke**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and test(mint)'
```

Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mint.rs src-tauri/tests/mint_integration.rs
git commit -m "feat(mint-sync): schema v2 — deleted_at + updated_at columns on accounts/settings

Adds the columns Phase 2 sync needs but doesn't yet wire them into
any reads/writes — that's Task 2. Migration is idempotent and backfills
updated_at from created_at for accounts and from now() for settings."
```

---

## Task 2: Soft-delete transactions + updated_at bumps on writes

Convert transactions hard-delete to soft-delete, filter reads, and bump updated_at on every account/setting write.

**Files:**
- Modify: `src-tauri/src/mint.rs` (delete_transaction, list_transactions, get_transaction, list_accounts JOIN, export_csv JOIN, create_account, rename_account, set_default_currency)
- Test: `src-tauri/tests/mint_integration.rs` (new tests)

- [ ] **Step 1: Write the failing tests**

In `src-tauri/tests/mint_integration.rs`, append:

```rust
#[test]
fn soft_delete_transaction_filters_from_reads() {
    let mut conn = fresh_in_memory_db();
    harmony_app::mint::apply_migrations(&mut conn).unwrap();
    // Seed an account + transaction.
    let acct = harmony_app::mint::create_account(&mut conn, "Chase").unwrap();
    let new_tx = harmony_app::mint::NewTransaction {
        transaction_date: "2026-05-01".into(),
        amount: "-12.34".into(),
        currency: "USD".into(),
        account_id: acct.id.clone(),
        description: "Coffee".into(),
        metadata: None,
    };
    let tx = harmony_app::mint::create_transaction(&mut conn, new_tx).unwrap();

    // Soft-delete it.
    harmony_app::mint::delete_transaction(&mut conn, &tx.id).unwrap();

    // Reads exclude the row.
    let listed = harmony_app::mint::list_transactions(&mut conn, None, None, None, None).unwrap();
    assert_eq!(listed.len(), 0);
    let fetched = harmony_app::mint::get_transaction(&mut conn, &tx.id).unwrap();
    assert!(fetched.is_none());

    // The row still exists in the table with deleted_at populated.
    let deleted_at: Option<String> = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            [&tx.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(deleted_at.is_some(), "deleted_at should be populated");
}

#[test]
fn soft_delete_is_idempotent() {
    let mut conn = fresh_in_memory_db();
    harmony_app::mint::apply_migrations(&mut conn).unwrap();
    let acct = harmony_app::mint::create_account(&mut conn, "Chase").unwrap();
    let tx = harmony_app::mint::create_transaction(
        &mut conn,
        harmony_app::mint::NewTransaction {
            transaction_date: "2026-05-01".into(),
            amount: "1".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "x".into(),
            metadata: None,
        },
    )
    .unwrap();

    harmony_app::mint::delete_transaction(&mut conn, &tx.id).unwrap();
    let first_deleted_at: String = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            [&tx.id],
            |r| r.get(0),
        )
        .unwrap();

    // Second delete is a no-op — does NOT overwrite the tombstone timestamp.
    std::thread::sleep(std::time::Duration::from_millis(5));
    harmony_app::mint::delete_transaction(&mut conn, &tx.id).unwrap();
    let second_deleted_at: String = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            [&tx.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first_deleted_at, second_deleted_at);
}

#[test]
fn account_rename_bumps_updated_at() {
    let mut conn = fresh_in_memory_db();
    harmony_app::mint::apply_migrations(&mut conn).unwrap();
    let acct = harmony_app::mint::create_account(&mut conn, "Chase").unwrap();
    let before: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&acct.id],
            |r| r.get(0),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    harmony_app::mint::rename_account(&mut conn, &acct.id, "Chase Checking").unwrap();
    let after: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&acct.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(after > before, "rename should bump updated_at");
}

#[test]
fn set_default_currency_bumps_updated_at() {
    let mut conn = fresh_in_memory_db();
    harmony_app::mint::apply_migrations(&mut conn).unwrap();
    harmony_app::mint::set_default_currency(&mut conn, "USD").unwrap();
    let before: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    harmony_app::mint::set_default_currency(&mut conn, "JPY").unwrap();
    let after: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(after > before, "set_default_currency should bump updated_at");
}
```

(`fresh_in_memory_db()` is the existing test helper that calls `harmony_app::mint::open_in_memory()`.)

- [ ] **Step 2: Verify the tests fail**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(soft_delete_transaction_filters_from_reads) or test(soft_delete_is_idempotent) or test(account_rename_bumps_updated_at) or test(set_default_currency_bumps_updated_at)'
```

Expected: FAIL (delete still hard-deletes; rename and set_default_currency don't touch updated_at; reads don't filter).

- [ ] **Step 3: Modify `delete_transaction` in `src-tauri/src/mint.rs`**

Find the existing `delete_transaction` function. Replace its `DELETE FROM transactions ...` with:

```rust
pub(crate) fn delete_transaction(conn: &mut Connection, id: &str) -> Result<(), MintError> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = conn.execute(
        "UPDATE transactions SET deleted_at = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
        rusqlite::params![now, now, id],
    )?;
    if rows == 0 {
        // Either the row doesn't exist or it's already tombstoned; both are OK.
        // Check existence to distinguish "already deleted" from "never existed".
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if exists == 0 {
            return Err(MintError::NotFound(format!("transaction {id}")));
        }
        // Already tombstoned: no-op success.
    }
    Ok(())
}
```

- [ ] **Step 4: Add `WHERE deleted_at IS NULL` to every transaction-read path**

In `src-tauri/src/mint.rs`, find every SQL query that touches the `transactions` table. The places that need a `deleted_at IS NULL` filter:

1. `list_transactions` — the main query's WHERE clause.
2. `get_transaction` — `SELECT ... FROM transactions WHERE id = ?` becomes `... AND deleted_at IS NULL`.
3. `list_accounts` — the JOIN that computes `transaction_count` joins on transactions; add `AND t.deleted_at IS NULL` to that JOIN.
4. `export_csv` — the JOIN that emits rows; add `AND t.deleted_at IS NULL` to the WHERE.
5. `update_transaction` — the row-lookup before update should also filter (so a tombstoned row can't be silently un-updated). Add `AND deleted_at IS NULL` to the lookup; if it returns zero rows, return `MintError::NotFound`.

For `list_transactions`, the existing query looks something like:

```rust
let mut sql = String::from(TRANSACTION_SELECT);
sql.push_str(" WHERE 1=1");
```

Change to:

```rust
let mut sql = String::from(TRANSACTION_SELECT);
sql.push_str(" WHERE t.deleted_at IS NULL");
```

(Or `WHERE transactions.deleted_at IS NULL` depending on the table alias the existing code uses — match the surrounding convention.)

For `list_accounts`, the existing JOIN looks like:

```sql
LEFT JOIN transactions t ON t.account_id = a.id
```

Change to:

```sql
LEFT JOIN transactions t ON t.account_id = a.id AND t.deleted_at IS NULL
```

For `update_transaction`, the existing row-lookup probably reads:

```rust
let existing = conn
    .query_row("SELECT ... FROM transactions WHERE id = ?", [id], map_transaction_row)
    .optional()?;
```

Change to:

```rust
let existing = conn
    .query_row("SELECT ... FROM transactions WHERE id = ? AND deleted_at IS NULL", [id], map_transaction_row)
    .optional()?;
```

- [ ] **Step 5: Modify `create_account` to set `updated_at = created_at`**

Find `create_account` and update the INSERT to include `updated_at`:

```rust
let now = chrono::Utc::now().to_rfc3339();
conn.execute(
    "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
    rusqlite::params![id, canonical_name, now],
)?;
```

(Using `?3` twice gives us the same timestamp for both columns.)

- [ ] **Step 6: Modify `rename_account` to bump `updated_at`**

```rust
let now = chrono::Utc::now().to_rfc3339();
conn.execute(
    "UPDATE accounts SET name = ?1, updated_at = ?2 WHERE id = ?3",
    rusqlite::params![canonical_name, now, id],
)?;
```

- [ ] **Step 7: Modify `set_default_currency` (and any other settings-write path) to bump `updated_at`**

```rust
pub(crate) fn set_default_currency(conn: &mut Connection, currency: &str) -> Result<(), MintError> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![DEFAULT_CURRENCY_KEY, currency, now],
    )?;
    Ok(())
}
```

- [ ] **Step 8: Modify the `update_transaction` body to bump updated_at**

If `update_transaction` doesn't already set `updated_at = chrono::Utc::now().to_rfc3339()` in its dynamic SQL, add it as an always-set column. The MVP design's D5 said updated_at is maintained on every mutation; verify this is true and add it if missing.

- [ ] **Step 9: Run the four new tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(soft_delete_transaction_filters_from_reads) or test(soft_delete_is_idempotent) or test(account_rename_bumps_updated_at) or test(set_default_currency_bumps_updated_at)'
```

Expected: all PASS.

- [ ] **Step 10: Run the full mint suite**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and test(mint)'
```

Expected: all PASS. If any existing test fails because it counted on hard-delete semantics (e.g., expects `COUNT(*)` of transactions to drop), update the test to expect the new soft-delete behavior — note the change in the test's comment.

- [ ] **Step 11: Commit**

```bash
git add src-tauri/src/mint.rs src-tauri/tests/mint_integration.rs
git commit -m "feat(mint-sync): soft-delete transactions + updated_at bumps on writes

delete_transaction now sets deleted_at + updated_at instead of DELETE FROM.
All read paths (list, get, account counts, CSV export, update lookup) filter
WHERE deleted_at IS NULL. Account create/rename and set_default_currency
now bump updated_at on every write — load-bearing for per-row LWW merge."
```

---

## Task 3: Snapshot types module

Create the type definitions in a new module. No business logic yet — just structs, enums, and round-trip tests.

**Files:**
- Create: `src-tauri/src/mint_sync_types.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod mint_sync_types;`)

- [ ] **Step 1: Write the failing test (inline in the new module)**

Create `src-tauri/src/mint_sync_types.rs`:

```rust
//! Type definitions for Mint Phase 2 sync.
//! No business logic — just data, errors, and (de)serialization.

use serde::{Deserialize, Serialize};

pub const MINT_SCHEMA_VERSION: u32 = 1;
pub const LOCAL_MAX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSnapshot {
    pub schema_version: u32,
    pub accounts: Vec<AccountRow>,
    pub transactions: Vec<TransactionRow>,
    pub settings: Vec<SettingRow>,
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRow {
    pub id: String,
    pub transaction_date: String,
    pub amount: String,
    pub currency: String,
    pub account_id: String,
    pub description: String,
    pub metadata: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingRow {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

/// Wire envelope published on the Zenoh topic. Two-char serde rename keys
/// satisfy the same-length-keys precondition that `canonical_cbor_encode`
/// established in owner-state Phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintRootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: crate::owner_state_types::ContentId,
    #[serde(rename = "at")]
    pub at: crate::owner_state_types::Hlc,
}

#[derive(thiserror::Error, Debug)]
pub enum MintSyncError {
    #[error("mint sync IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("mint sync cbor encode/decode: {0}")]
    Cbor(String),
    #[error("mint sync sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("mint sync crypto: {0}")]
    Crypto(String),
    #[error("mint sync content store: blob {0:?} missing")]
    MissingBlob(crate::owner_state_types::ContentId),
    #[error("mint sync schema version too new: remote={remote}, local_max={local_max}")]
    SchemaTooNew { remote: u32, local_max: u32 },
    #[error("mint sync other: {0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_snapshot_round_trips_through_cbor() {
        let snap = MintSnapshot {
            schema_version: MINT_SCHEMA_VERSION,
            accounts: vec![AccountRow {
                id: "acct-1".into(),
                name: "Chase".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![TransactionRow {
                id: "tx-1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "-12.34".into(),
                currency: "USD".into(),
                account_id: "acct-1".into(),
                description: "Coffee".into(),
                metadata: Some(r#"{"tag":"travel"}"#.into()),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![SettingRow {
                key: "default_currency".into(),
                value: "USD".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        let cbor = serde_cbor::to_vec(&snap).unwrap();
        let decoded: MintSnapshot = serde_cbor::from_slice(&cbor).unwrap();
        assert_eq!(snap, decoded);
    }

    #[test]
    fn tombstone_round_trips() {
        let row = TransactionRow {
            id: "tx-1".into(),
            transaction_date: "2026-05-01".into(),
            amount: "0".into(),
            currency: "USD".into(),
            account_id: "acct-1".into(),
            description: "x".into(),
            metadata: None,
            created_at: "2026-05-01T00:00:00Z".into(),
            updated_at: "2026-05-02T00:00:00Z".into(),
            deleted_at: Some("2026-05-02T00:00:00Z".into()),
        };
        let cbor = serde_cbor::to_vec(&row).unwrap();
        let decoded: TransactionRow = serde_cbor::from_slice(&cbor).unwrap();
        assert_eq!(row.deleted_at, decoded.deleted_at);
    }

    #[test]
    fn root_publish_payload_round_trips() {
        let payload = MintRootPublishPayload {
            root_cid: crate::owner_state_types::ContentId([1u8; 32]),
            at: crate::owner_state_types::Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 7,
                device_id: [42u8; 16],
            },
        };
        let cbor = serde_cbor::to_vec(&payload).unwrap();
        let decoded: MintRootPublishPayload = serde_cbor::from_slice(&cbor).unwrap();
        assert_eq!(payload, decoded);
    }
}
```

In `src-tauri/src/lib.rs`, add (alongside the existing `pub mod mint;`):

```rust
pub mod mint_sync_types;
```

- [ ] **Step 2: Verify the module compiles**

```
cd src-tauri && cargo check --locked --features test-fixtures
```

Expected: clean compile (no warnings about unused — `#[derive(...)]` keeps everything live).

If `serde_cbor` isn't already in `Cargo.toml`'s `[dependencies]`, check first:

```
cd src-tauri && grep serde_cbor Cargo.toml
```

If it's missing, the owner-state code already depends on it transitively — search where:

```
cd src-tauri && grep -rn "serde_cbor::" src/ | head -5
```

If it's used elsewhere but not in `Cargo.toml`, add `serde_cbor = "0.11"` to `[dependencies]`. (It is used by owner-state, so this should be a no-op.)

- [ ] **Step 3: Run the unit tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(mint_sync_types)'
```

Expected: 2 tests PASS (`mint_snapshot_round_trips_through_cbor`, `tombstone_round_trips`).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mint_sync_types.rs src-tauri/src/lib.rs
git commit -m "feat(mint-sync): mint_sync_types module — snapshot, rows, error

Pure types for Phase 2 sync. MintSnapshot, AccountRow, TransactionRow,
SettingRow round-trip through CBOR cleanly; MintSyncError covers I/O,
SQLite, crypto, missing-blob, and schema-too-new variants."
```

---

## Task 4: snapshot_current_db + apply_remote_snapshot (LWW + tombstones)

Build the two core merge-engine functions as free functions in `mint_sync.rs` so they're easy to unit-test without standing up an engine.

**Files:**
- Create: `src-tauri/src/mint_sync.rs` (initial scaffolding, just these two functions + tests)
- Modify: `src-tauri/src/lib.rs` (`pub mod mint_sync;`)

- [ ] **Step 1: Write the failing tests (inline in the new module)**

Create `src-tauri/src/mint_sync.rs`:

```rust
//! Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.

use crate::mint_sync_types::{
    AccountRow, MintSnapshot, MintSyncError, SettingRow, TransactionRow,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

/// Read the full ledger state into a [`MintSnapshot`].
///
/// Runs three SELECTs inside one transaction so the snapshot is a
/// consistent read across tables. Includes tombstoned rows so peers
/// see the tombstone and converge on delete.
pub(crate) fn snapshot_current_db(conn: &mut Connection) -> Result<MintSnapshot, MintSyncError> {
    let tx = conn.transaction()?;
    let accounts: Vec<AccountRow> = tx
        .prepare("SELECT id, name, created_at, updated_at FROM accounts ORDER BY id")?
        .query_map([], |r| {
            Ok(AccountRow {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let transactions: Vec<TransactionRow> = tx
        .prepare(
            "SELECT id, transaction_date, amount, currency, account_id, description, \
                    metadata, created_at, updated_at, deleted_at \
             FROM transactions ORDER BY id",
        )?
        .query_map([], |r| {
            Ok(TransactionRow {
                id: r.get(0)?,
                transaction_date: r.get(1)?,
                amount: r.get(2)?,
                currency: r.get(3)?,
                account_id: r.get(4)?,
                description: r.get(5)?,
                metadata: r.get(6)?,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
                deleted_at: r.get(9)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let settings: Vec<SettingRow> = tx
        .prepare("SELECT key, value, updated_at FROM settings ORDER BY key")?
        .query_map([], |r| {
            Ok(SettingRow {
                key: r.get(0)?,
                value: r.get(1)?,
                updated_at: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    tx.commit()?;
    Ok(MintSnapshot {
        schema_version: crate::mint_sync_types::MINT_SCHEMA_VERSION,
        accounts,
        transactions,
        settings,
        captured_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Apply a remote snapshot to the local DB. Runs in a single SQLite
/// transaction — either all rows merge or none do.
///
/// `account_deletion_floor` is the per-device map of hard-deleted
/// account IDs → deletion timestamp; peer rows older than the floor
/// are dropped to prevent zombie-resurrect. Pass an empty map until
/// Task 5 wires the real one.
pub(crate) fn apply_remote_snapshot(
    conn: &mut Connection,
    remote: &MintSnapshot,
    account_deletion_floor: &HashMap<String, String>,
) -> Result<(), MintSyncError> {
    let tx = conn.transaction()?;
    for r in &remote.accounts {
        upsert_account_lww(&tx, r, account_deletion_floor)?;
    }
    for r in &remote.transactions {
        upsert_transaction_lww(&tx, r)?;
    }
    for r in &remote.settings {
        upsert_setting_lww(&tx, r)?;
    }
    tx.commit()?;
    Ok(())
}

fn upsert_account_lww(
    tx: &rusqlite::Transaction,
    r: &AccountRow,
    floor: &HashMap<String, String>,
) -> Result<(), MintSyncError> {
    if let Some(floor_ts) = floor.get(&r.id) {
        if &r.updated_at <= floor_ts {
            return Ok(()); // peer's row is stale w.r.t. our delete
        }
    }
    let local_updated_at: Option<String> = tx
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&r.id],
            |row| row.get(0),
        )
        .optional()?;
    match local_updated_at {
        None => {
            tx.execute(
                "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![r.id, r.name, r.created_at, r.updated_at],
            )?;
        }
        Some(local) if r.updated_at > local => {
            tx.execute(
                "UPDATE accounts SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![r.name, r.updated_at, r.id],
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

fn upsert_transaction_lww(
    tx: &rusqlite::Transaction,
    r: &TransactionRow,
) -> Result<(), MintSyncError> {
    let local_updated_at: Option<String> = tx
        .query_row(
            "SELECT updated_at FROM transactions WHERE id = ?",
            [&r.id],
            |row| row.get(0),
        )
        .optional()?;
    match local_updated_at {
        None => {
            tx.execute(
                "INSERT INTO transactions \
                 (id, transaction_date, amount, currency, account_id, description, \
                  metadata, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    r.id,
                    r.transaction_date,
                    r.amount,
                    r.currency,
                    r.account_id,
                    r.description,
                    r.metadata,
                    r.created_at,
                    r.updated_at,
                    r.deleted_at,
                ],
            )?;
        }
        Some(local) if r.updated_at > local => {
            tx.execute(
                "UPDATE transactions SET \
                 transaction_date = ?1, amount = ?2, currency = ?3, account_id = ?4, \
                 description = ?5, metadata = ?6, updated_at = ?7, deleted_at = ?8 \
                 WHERE id = ?9",
                params![
                    r.transaction_date,
                    r.amount,
                    r.currency,
                    r.account_id,
                    r.description,
                    r.metadata,
                    r.updated_at,
                    r.deleted_at,
                    r.id,
                ],
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

fn upsert_setting_lww(
    tx: &rusqlite::Transaction,
    r: &SettingRow,
) -> Result<(), MintSyncError> {
    tx.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET \
           value = excluded.value, updated_at = excluded.updated_at \
         WHERE excluded.updated_at > settings.updated_at",
        params![r.key, r.value, r.updated_at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mint::{apply_migrations, open_in_memory};

    fn fresh_db() -> Connection {
        let mut conn = open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();
        conn
    }

    fn seed_account(conn: &mut Connection, id: &str, name: &str, updated_at: &str) {
        conn.execute(
            "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, "2026-05-01T00:00:00Z", updated_at],
        )
        .unwrap();
    }

    fn seed_tx(conn: &mut Connection, id: &str, acct: &str, desc: &str, updated_at: &str) {
        conn.execute(
            "INSERT INTO transactions \
             (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
             VALUES (?1, '2026-05-01', '1', 'USD', ?2, ?3, '2026-05-01T00:00:00Z', ?4)",
            params![id, acct, desc, updated_at],
        )
        .unwrap();
    }

    #[test]
    fn snapshot_round_trips_empty_db() {
        let mut conn = fresh_db();
        let snap = snapshot_current_db(&mut conn).unwrap();
        assert_eq!(snap.accounts.len(), 0);
        assert_eq!(snap.transactions.len(), 0);
        // settings may have a seeded default_currency row from migration; that's OK.
    }

    #[test]
    fn snapshot_includes_tombstones() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut conn, "t1", "a1", "x", "2026-05-01T00:00:00Z");
        conn.execute(
            "UPDATE transactions SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
            params!["2026-05-02T00:00:00Z", "t1"],
        )
        .unwrap();
        let snap = snapshot_current_db(&mut conn).unwrap();
        assert_eq!(snap.transactions.len(), 1);
        assert!(snap.transactions[0].deleted_at.is_some());
    }

    #[test]
    fn apply_inserts_new_rows() {
        let mut local = fresh_db();
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "-12.34".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "Coffee".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let snap = snapshot_current_db(&mut local).unwrap();
        assert_eq!(snap.accounts.len(), 1);
        assert_eq!(snap.transactions.len(), 1);
    }

    #[test]
    fn apply_lww_keeps_newer_local() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase Newer", "2026-05-02T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Older".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let name: String = local
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Chase Newer");
    }

    #[test]
    fn apply_lww_replaces_older_local() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase Older", "2026-05-01T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Newer".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-02T00:00:00Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let name: String = local
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Chase Newer");
    }

    #[test]
    fn apply_propagates_tombstone() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut local, "t1", "a1", "live", "2026-05-01T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "1".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "live".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-02T00:00:00Z".into(),
                deleted_at: Some("2026-05-02T00:00:00Z".into()),
            }],
            settings: vec![],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let deleted_at: Option<String> = local
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                ["t1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_some());
    }

    #[test]
    fn apply_resurrects_after_tombstone() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut local, "t1", "a1", "x", "2026-05-01T00:00:00Z");
        local
            .execute(
                "UPDATE transactions SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                params!["2026-05-02T00:00:00Z", "t1"],
            )
            .unwrap();
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "1".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "x".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-03T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let deleted_at: Option<String> = local
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                ["t1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_none());
    }
}
```

In `src-tauri/src/lib.rs`:

```rust
pub mod mint_sync;
```

- [ ] **Step 2: Run the tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(mint_sync::tests)'
```

Expected: all 6 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/mint_sync.rs src-tauri/src/lib.rs
git commit -m "feat(mint-sync): snapshot_current_db + apply_remote_snapshot

Pure functions, no engine yet. snapshot_current_db reads accounts +
transactions (including tombstones) + settings as a consistent snapshot.
apply_remote_snapshot runs the per-table LWW merge inside one SQLite
transaction. Six unit tests cover insert, LWW both directions, tombstone
propagation, and tombstone-then-resurrect."
```

---

## Task 5: Account deletion floor

Add the floor parameter to `delete_account`'s contract and persist floor entries on hard-delete. Wire the floor through `apply_remote_snapshot`.

**Files:**
- Modify: `src-tauri/src/mint.rs` (delete_account signature + impl)
- Modify: `src-tauri/src/mint_sync.rs` (deletion-floor test)
- The floor itself lives in `MintSyncState`, which Task 6 builds. For now, just take it as a parameter and add a unit test that proves it works.

- [ ] **Step 1: Add the deletion-floor unit test in `src-tauri/src/mint_sync.rs`**

Append to the `tests` mod:

```rust
#[test]
fn deletion_floor_blocks_stale_account_resurrect() {
    let mut local = fresh_db();
    // local has NO record of a1 (it was previously hard-deleted).
    let mut floor = HashMap::new();
    floor.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
    let remote = MintSnapshot {
        schema_version: 1,
        accounts: vec![AccountRow {
            id: "a1".into(),
            name: "Chase Zombie".into(),
            created_at: "2026-05-01T00:00:00Z".into(),
            updated_at: "2026-05-01T00:00:00Z".into(), // older than floor
        }],
        transactions: vec![],
        settings: vec![],
        captured_at: "2026-05-19T12:00:00Z".into(),
    };
    apply_remote_snapshot(&mut local, &remote, &floor).unwrap();
    let exists: Option<String> = local
        .query_row("SELECT id FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
        .optional()
        .unwrap();
    assert!(exists.is_none(), "floor should have blocked the resurrect");
}

#[test]
fn deletion_floor_allows_newer_remote_through() {
    let mut local = fresh_db();
    let mut floor = HashMap::new();
    floor.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
    let remote = MintSnapshot {
        schema_version: 1,
        accounts: vec![AccountRow {
            id: "a1".into(),
            name: "Chase Re-created".into(),
            created_at: "2026-05-03T00:00:00Z".into(),
            updated_at: "2026-05-03T00:00:00Z".into(), // newer than floor
        }],
        transactions: vec![],
        settings: vec![],
        captured_at: "2026-05-19T12:00:00Z".into(),
    };
    apply_remote_snapshot(&mut local, &remote, &floor).unwrap();
    let name: Option<String> = local
        .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
        .optional()
        .unwrap();
    assert_eq!(name.as_deref(), Some("Chase Re-created"));
}
```

- [ ] **Step 2: Verify both tests pass**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(deletion_floor_blocks_stale_account_resurrect) or test(deletion_floor_allows_newer_remote_through)'
```

Expected: both PASS (the floor logic is already in `upsert_account_lww` from Task 4).

- [ ] **Step 3: Update `delete_account` to record floor entries**

The floor needs to live somewhere `delete_account` can reach. Option A: pass a `&mut HashMap<String, String>` parameter. Option B: store it inside a wrapper struct.

For Phase 2 simplicity, use **Option A**: add a `floor: &mut HashMap<String, String>` parameter to `delete_account`. The IPC layer will own the floor (sourced from `MintSyncState`, built in Task 6); for now, plumb the parameter through.

In `src-tauri/src/mint.rs`, change the signature:

```rust
pub(crate) fn delete_account(
    conn: &mut Connection,
    id: &str,
    reassign_to: Option<&str>,
    floor: &mut std::collections::HashMap<String, String>,
) -> Result<(), MintError> {
    // ... existing reassign logic ...
    // After the reassign UPDATE (if any) and the DELETE FROM accounts:
    let now = chrono::Utc::now().to_rfc3339();
    floor.insert(id.to_string(), now);
    Ok(())
}
```

- [ ] **Step 4: Update the IPC handler `mint_delete_account` to thread the floor through**

In `src-tauri/src/mint.rs`, the IPC command needs to acquire `NodeState`'s floor (which Task 6 + Task 11 will introduce). For now, since `MintSyncState` doesn't exist yet, use a temporary thread-local or `RwLock<HashMap>` field on `NodeState`. We'll consolidate in Task 11.

Add a TEMPORARY field on the existing `NodeState` in `lib.rs`:

```rust
// In NodeState definition:
pub mint_pending_account_floor: std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
>,
```

Initialize it as `Arc::new(Mutex::new(HashMap::new()))` in `NodeState::new()` / `Default::impl` (wherever the existing NodeState is constructed).

In `mint_delete_account`:

```rust
#[tauri::command]
pub(crate) async fn mint_delete_account(
    id: String,
    reassign_to: Option<String>,
    state: tauri::State<'_, NodeState>,
) -> Result<(), String> {
    let db = mint_db_handle(&state).await.map_err(|e| e.to_string())?;
    let floor_arc = state.mint_pending_account_floor.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = db.lock().expect("mint_db lock poisoned");
        let mut floor = floor_arc.lock().expect("floor lock poisoned");
        crate::mint::delete_account(&mut conn, &id, reassign_to.as_deref(), &mut floor)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
```

(Task 11 will fold the temporary `mint_pending_account_floor` field into `mint_sync.floor` once the engine exists.)

- [ ] **Step 5: Update any existing call sites of `delete_account` (tests)**

In `src-tauri/tests/mint_integration.rs`, find every `delete_account(...)` call. Update each:

```rust
let mut floor = std::collections::HashMap::new();
harmony_app::mint::delete_account(&mut conn, &acct.id, Some(&other.id), &mut floor).unwrap();
```

- [ ] **Step 6: Run all mint tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(mint) or test(mint_sync))'
```

Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/mint.rs src-tauri/src/mint_sync.rs src-tauri/src/lib.rs src-tauri/tests/mint_integration.rs
git commit -m "feat(mint-sync): account deletion floor — block zombie resurrects

delete_account records (id, now()) into a per-device floor. apply_remote_snapshot's
account upsert checks the floor and drops peer rows whose updated_at is <= the
floor timestamp. Floor is plumbed as a parameter for now; Task 11 will fold it
into MintSyncState once the engine exists."
```

---

## Task 6: MintSyncPersist module

Load/save `mint_sync_state.cbor` with atomic-rename + fsync, following the same pattern as `state_root_replay.cbor` in `owner_state_persist.rs`.

**Files:**
- Create: `src-tauri/src/mint_sync_persist.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod mint_sync_persist;`)
- Modify: `src-tauri/src/mint_sync_types.rs` (add `MintSyncState` struct)

- [ ] **Step 1: Add `MintSyncState` to `mint_sync_types.rs`**

In `src-tauri/src/mint_sync_types.rs`, add (after the existing structs):

```rust
/// On-disk persisted state for the mint sync engine.
/// Stored at `<app_data_dir>/mint/mint_sync_state.cbor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSyncState {
    pub schema_version: u32,
    pub replay_tracker: crate::owner_state_types::RootReplayTracker,
    pub account_deletion_floor: std::collections::HashMap<String, String>,
}

impl Default for MintSyncState {
    fn default() -> Self {
        Self {
            schema_version: MINT_SCHEMA_VERSION,
            replay_tracker: crate::owner_state_types::RootReplayTracker::default(),
            account_deletion_floor: std::collections::HashMap::new(),
        }
    }
}
```

(If `RootReplayTracker` lives at a different path, adjust the use. Check with: `grep -rn "RootReplayTracker" src-tauri/src/` — likely in `owner_state_types.rs` or `owner_state_sync.rs`.)

- [ ] **Step 2: Write the failing test (inline in the new module)**

Create `src-tauri/src/mint_sync_persist.rs`:

```rust
//! On-disk persistence for MintSyncState.
//! Atomic-rename + fsync, mirroring owner_state_persist.

use crate::mint_sync_types::{MintSyncError, MintSyncState};
use std::path::Path;

/// File name for the persisted state. Lives at `<app_data_dir>/mint/mint_sync_state.cbor`.
pub const MINT_SYNC_STATE_FILENAME: &str = "mint_sync_state.cbor";

/// Load state from disk. Returns `Ok(default)` if the file doesn't exist yet.
pub fn load(path: &Path) -> Result<MintSyncState, MintSyncError> {
    if !path.exists() {
        return Ok(MintSyncState::default());
    }
    let bytes = std::fs::read(path)?;
    let state: MintSyncState = serde_cbor::from_slice(&bytes)
        .map_err(|e| MintSyncError::Cbor(format!("load: {e}")))?;
    Ok(state)
}

/// Save state to disk via atomic-rename. Writes `<path>.tmp`, fsyncs, then renames over `<path>`.
pub fn save(path: &Path, state: &MintSyncState) -> Result<(), MintSyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_cbor::to_vec(state)
        .map_err(|e| MintSyncError::Cbor(format!("save: {e}")))?;
    let tmp = path.with_extension("cbor.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let state = load(&path).unwrap();
        assert_eq!(state, MintSyncState::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let mut state = MintSyncState::default();
        state
            .account_deletion_floor
            .insert("a1".into(), "2026-05-02T00:00:00Z".into());
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn save_does_not_leave_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        save(&path, &MintSyncState::default()).unwrap();
        let tmp = path.with_extension("cbor.tmp");
        assert!(!tmp.exists(), "tmp file should be renamed away");
    }
}
```

In `src-tauri/src/lib.rs`:

```rust
pub mod mint_sync_persist;
```

- [ ] **Step 3: Run the tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(mint_sync_persist)'
```

Expected: 3 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mint_sync_persist.rs src-tauri/src/mint_sync_types.rs src-tauri/src/lib.rs
git commit -m "feat(mint-sync): mint_sync_persist — load/save mint_sync_state.cbor

MintSyncState bundles schema_version + RootReplayTracker (reused from
owner-state) + account_deletion_floor. Atomic-rename + fsync save mirrors
owner_state_persist's pattern."
```

---

## Task 7: MintSyncEngine scaffold (new, shutdown)

Stand up the engine struct with constructor and shutdown signal. No publish/subscribe yet — Tasks 8/9 add those.

**Files:**
- Modify: `src-tauri/src/mint_sync.rs`

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/mint_sync.rs`, add to the `tests` mod:

```rust
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

#[tokio::test]
async fn engine_new_and_shutdown_no_publish() {
    let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));
    let (engine, handle) = MintSyncEngine::new_for_test(conn, cs, sync_state).await;
    engine.shutdown().await.unwrap();
    // Handle joins without panic.
    handle.await.unwrap();
}
```

(Note: `new_for_test` is a constructor that takes an `InMemoryStub` ContentStore and skips the Zenoh wiring — we'll add the real `new` in Task 11 when we wire identity.)

- [ ] **Step 2: Add the engine scaffold**

In `src-tauri/src/mint_sync.rs`, before the `tests` mod, add:

```rust
use crate::mint_sync_types::MintSyncState;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex, Notify};

/// Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.
pub struct MintSyncEngine {
    dirty: Arc<Notify>,
    flush_now: mpsc::Sender<()>,
    shutdown: mpsc::Sender<()>,
}

pub struct MintSyncEngineHandle(tokio::task::JoinHandle<()>);

impl std::future::Future for MintSyncEngineHandle {
    type Output = Result<(), tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl MintSyncEngine {
    /// Test constructor: no Zenoh, just an in-memory ContentStore.
    /// The real `new` (Task 11) takes a Zenoh session + identity key.
    pub async fn new_for_test(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
    ) -> (Self, MintSyncEngineHandle) {
        let dirty = Arc::new(Notify::new());
        let (flush_tx, flush_rx) = mpsc::channel::<()>(1);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let dirty_for_task = dirty.clone();
        let handle = tokio::spawn(internal_task(
            mint_db,
            content_store,
            sync_state,
            dirty_for_task,
            flush_rx,
            shutdown_rx,
        ));
        (
            Self {
                dirty,
                flush_now: flush_tx,
                shutdown: shutdown_tx,
            },
            MintSyncEngineHandle(handle),
        )
    }

    pub fn notify_dirty(&self) {
        self.dirty.notify_one();
    }

    pub async fn flush_now(&self) -> Result<(), MintSyncError> {
        self.flush_now
            .send(())
            .await
            .map_err(|_| MintSyncError::Other("flush channel closed".into()))?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), MintSyncError> {
        let _ = self.shutdown.send(()).await;
        Ok(())
    }
}

/// Internal task loop. Task 8 fills in publish_root_now; for now this just
/// drains the channels and exits cleanly on shutdown.
async fn internal_task(
    _mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    _content_store: Arc<dyn crate::content_store::ContentStore>,
    _sync_state: Arc<TokioMutex<MintSyncState>>,
    dirty: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<()>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = dirty.notified() => {
                // Task 8 will schedule a debounced publish here.
            }
            _ = flush_rx.recv() => {
                // Task 8 will fire publish_root_now here.
            }
            _ = shutdown_rx.recv() => break,
        }
    }
}
```

- [ ] **Step 3: Run the test**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(engine_new_and_shutdown_no_publish)'
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mint_sync.rs
git commit -m "feat(mint-sync): MintSyncEngine scaffold — new_for_test + shutdown

Test constructor + drain-and-exit internal_task. Tasks 8/9 will plug in
publish_root_now and the subscriber. The Zenoh-wired `new` constructor
arrives in Task 11 once we have identity threaded through."
```

---

## Task 8: publish_root_now + empty-skip + debounce loop

Fill in the publisher path: serialize, encrypt, put to CAS, publish via Zenoh. Wire the 250ms debounce. Empty-snapshot skip.

**Files:**
- Modify: `src-tauri/src/mint_sync.rs`

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/mint_sync.rs::tests`, add:

```rust
#[tokio::test]
async fn publish_writes_to_content_store_and_zenoh_stub() {
    let mut conn = fresh_db();
    seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
    seed_tx(&mut conn, "t1", "a1", "x", "2026-05-01T00:00:00Z");
    let conn = Arc::new(std::sync::Mutex::new(conn));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine, handle) = MintSyncEngine::new_for_test(conn.clone(), cs.clone(), sync_state).await;
    engine.flush_now().await.unwrap();
    // Wait for the publish to land. (In real engine, Task 10's boot-hook would do this; here we yield.)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // ContentStore should now have exactly one blob.
    let count = cs.debug_count().await; // helper added below
    assert_eq!(count, 1);

    engine.shutdown().await.unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn publish_skips_when_snapshot_is_empty() {
    let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine, handle) = MintSyncEngine::new_for_test(conn, cs.clone(), sync_state).await;
    engine.flush_now().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let count = cs.debug_count().await;
    assert_eq!(count, 0, "empty-snapshot publish should be a no-op");

    engine.shutdown().await.unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn notify_dirty_triggers_debounced_publish() {
    let mut conn = fresh_db();
    seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
    let conn = Arc::new(std::sync::Mutex::new(conn));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine, handle) = MintSyncEngine::new_for_test_with_debounce(
        conn,
        cs.clone(),
        sync_state,
        std::time::Duration::from_millis(50),
    )
    .await;

    engine.notify_dirty();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(cs.debug_count().await, 1);

    engine.shutdown().await.unwrap();
    handle.await.unwrap();
}
```

The tests reference `cs.debug_count()` — extend `InMemoryStub` to expose this (only behind `#[cfg(test)]` or via a test-fixtures cfg gate). Check the current shape in `src-tauri/src/content_store.rs`. If `InMemoryStub` already has a `len()` or similar helper, use that name instead.

If `InMemoryStub` doesn't have a count method, add to `src-tauri/src/content_store.rs`:

```rust
#[cfg(any(test, feature = "test-fixtures"))]
impl InMemoryStub {
    pub async fn debug_count(&self) -> usize {
        self.inner.lock().await.len()
    }
}
```

(Match the existing field name on `InMemoryStub`.)

- [ ] **Step 2: Implement `publish_root_now`**

In `src-tauri/src/mint_sync.rs`, replace the `internal_task` body with a full implementation. Also add `new_for_test_with_debounce` for the override-debounce test:

```rust
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

impl MintSyncEngine {
    pub async fn new_for_test(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
    ) -> (Self, MintSyncEngineHandle) {
        Self::new_for_test_with_debounce(
            mint_db,
            content_store,
            sync_state,
            std::time::Duration::from_millis(DEFAULT_DEBOUNCE_MS),
        )
        .await
    }

    pub async fn new_for_test_with_debounce(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        debounce: std::time::Duration,
    ) -> (Self, MintSyncEngineHandle) {
        let dirty = Arc::new(Notify::new());
        let (flush_tx, flush_rx) = mpsc::channel::<()>(1);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let dirty_for_task = dirty.clone();
        let handle = tokio::spawn(internal_task(
            mint_db,
            content_store,
            sync_state,
            dirty_for_task,
            flush_rx,
            shutdown_rx,
            debounce,
        ));
        (
            Self { dirty, flush_now: flush_tx, shutdown: shutdown_tx },
            MintSyncEngineHandle(handle),
        )
    }
}

async fn internal_task(
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
    dirty: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<()>,
    mut shutdown_rx: mpsc::Receiver<()>,
    debounce: std::time::Duration,
) {
    let mut scheduled: Option<tokio::time::Instant> = None;
    loop {
        let next_wake = scheduled.unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        tokio::select! {
            _ = dirty.notified() => {
                scheduled = Some(tokio::time::Instant::now() + debounce);
            }
            _ = tokio::time::sleep_until(next_wake), if scheduled.is_some() => {
                scheduled = None;
                if let Err(e) = publish_root_now(&mint_db, &content_store, &sync_state).await {
                    tracing::warn!(target: "mint_sync", "publish_root_now failed: {e}");
                }
            }
            _ = flush_rx.recv() => {
                scheduled = None;
                if let Err(e) = publish_root_now(&mint_db, &content_store, &sync_state).await {
                    tracing::warn!(target: "mint_sync", "flush publish failed: {e}");
                }
            }
            _ = shutdown_rx.recv() => break,
        }
    }
}

async fn publish_root_now(
    mint_db: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: &Arc<dyn crate::content_store::ContentStore>,
    _sync_state: &Arc<TokioMutex<MintSyncState>>,
) -> Result<(), MintSyncError> {
    let mint_db = mint_db.clone();
    let snap = tokio::task::spawn_blocking(move || {
        let mut conn = mint_db.lock().expect("mint_db lock poisoned");
        snapshot_current_db(&mut conn)
    })
    .await
    .map_err(|e| MintSyncError::Other(format!("spawn_blocking: {e}")))??;

    if snap.accounts.is_empty() && snap.transactions.is_empty() && snap.settings.is_empty() {
        tracing::debug!(target: "mint_sync", "empty snapshot — skipping publish");
        return Ok(());
    }

    let cbor = serde_cbor::to_vec(&snap)
        .map_err(|e| MintSyncError::Cbor(format!("publish encode: {e}")))?;
    // TODO(Task 11): encrypt via space_lookup_key(&kt, b"mint-ledger-v1")
    // For Task 8, we put cleartext into the stub — encryption lands when the engine is wired
    // with identity. The InMemoryStub doesn't care.
    let root_cid = crate::owner_state_types::ContentId(blake3::hash(&cbor).into());
    content_store
        .put(root_cid, cbor)
        .await
        .map_err(|e| MintSyncError::Other(format!("content_store.put: {e}")))?;

    // TODO(Task 11): publish (root_cid, hlc) via Zenoh after encrypt_root_publish.
    tracing::info!(target: "mint_sync", root_cid = ?root_cid, "published mint snapshot");
    Ok(())
}
```

The `TODO(Task 11)` markers are deliberate scope-control: Tasks 8/9 build the engine plumbing against an in-process stub; Task 11 swaps in the real Zenoh + crypto wiring. Each TODO references the task that closes it — they are NOT placeholders in the "incomplete plan" sense.

- [ ] **Step 3: Run the tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(publish_writes_to_content_store_and_zenoh_stub) or test(publish_skips_when_snapshot_is_empty) or test(notify_dirty_triggers_debounced_publish)'
```

Expected: 3 tests PASS.

- [ ] **Step 4: Verify existing tests still pass**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(mint) or test(mint_sync))'
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mint_sync.rs src-tauri/src/content_store.rs
git commit -m "feat(mint-sync): publish_root_now + 250ms debounce + empty-skip

internal_task collapses bursts of notify_dirty into a single publish after
debounce. flush_now bypasses the timer. Empty-snapshot publishes are skipped
to prevent a new device from wiping peers via LWW. Encryption + Zenoh
publish remain stubbed pending Task 11's identity wiring (see TODO markers)."
```

---

## Task 9: subscriber_task — receive, fetch, merge

Add the subscriber path: simulate an envelope arriving, fetch the blob from CAS, decode + merge.

**Files:**
- Modify: `src-tauri/src/mint_sync.rs`

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/mint_sync.rs::tests`, add:

```rust
#[tokio::test]
async fn subscriber_applies_remote_snapshot_to_local() {
    // Two DBs, one shared ContentStore. Engine A publishes; engine B's
    // subscriber pulls and merges.
    let mut conn_a = fresh_db();
    seed_account(&mut conn_a, "a1", "Chase", "2026-05-01T00:00:00Z");
    seed_tx(&mut conn_a, "t1", "a1", "Coffee", "2026-05-01T00:00:00Z");
    let conn_a = Arc::new(std::sync::Mutex::new(conn_a));
    let conn_b = Arc::new(std::sync::Mutex::new(fresh_db()));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state_a = Arc::new(TokioMutex::new(MintSyncState::default()));
    let sync_state_b = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine_a, handle_a) = MintSyncEngine::new_for_test(conn_a, cs.clone(), sync_state_a).await;
    let (engine_b, handle_b) = MintSyncEngine::new_for_test(conn_b.clone(), cs.clone(), sync_state_b).await;

    // A publishes.
    engine_a.flush_now().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Simulate B's subscriber receiving the envelope. (Task 11 will fire this from real Zenoh.)
    // For now, we drive it directly via engine_b.handle_incoming_envelope_for_test(root_cid).
    let blobs = cs.debug_all_cids().await;
    assert_eq!(blobs.len(), 1);
    engine_b
        .handle_incoming_envelope_for_test(blobs[0])
        .await
        .unwrap();

    // B's DB should now contain a1 + t1.
    let snap_b = {
        let mut conn = conn_b.lock().unwrap();
        snapshot_current_db(&mut conn).unwrap()
    };
    assert_eq!(snap_b.accounts.len(), 1);
    assert_eq!(snap_b.transactions.len(), 1);

    engine_a.shutdown().await.unwrap();
    engine_b.shutdown().await.unwrap();
    handle_a.await.unwrap();
    handle_b.await.unwrap();
}
```

This requires extending `InMemoryStub` again:

```rust
#[cfg(any(test, feature = "test-fixtures"))]
impl InMemoryStub {
    pub async fn debug_all_cids(&self) -> Vec<crate::owner_state_types::ContentId> {
        self.inner.lock().await.keys().copied().collect()
    }
}
```

And a test-only entry point on `MintSyncEngine`:

- [ ] **Step 2: Implement `handle_incoming_envelope_for_test`**

In `src-tauri/src/mint_sync.rs`:

```rust
impl MintSyncEngine {
    /// Test entry point that simulates a Zenoh-delivered envelope.
    /// Task 11 replaces this with a real Zenoh subscriber.
    pub async fn handle_incoming_envelope_for_test(
        &self,
        root_cid: crate::owner_state_types::ContentId,
    ) -> Result<(), MintSyncError> {
        // We need the engine to hold a clone of its db/cs/state to do the fetch.
        // For Task 9 simplicity, expose them via a helper struct stored on Self.
        self.shared.handle_incoming(root_cid).await
    }
}

#[derive(Clone)]
struct EngineShared {
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
}

impl EngineShared {
    async fn handle_incoming(
        &self,
        root_cid: crate::owner_state_types::ContentId,
    ) -> Result<(), MintSyncError> {
        let blob = self
            .content_store
            .get(&root_cid)
            .await
            .map_err(|e| MintSyncError::Other(format!("content_store.get: {e}")))?
            .ok_or(MintSyncError::MissingBlob(root_cid))?;
        // TODO(Task 11): decrypt blob via decrypt_entry.
        let remote: crate::mint_sync_types::MintSnapshot = serde_cbor::from_slice(&blob)
            .map_err(|e| MintSyncError::Cbor(format!("subscriber decode: {e}")))?;

        if remote.schema_version > crate::mint_sync_types::LOCAL_MAX_SCHEMA_VERSION {
            return Err(MintSyncError::SchemaTooNew {
                remote: remote.schema_version,
                local_max: crate::mint_sync_types::LOCAL_MAX_SCHEMA_VERSION,
            });
        }

        let mint_db = self.mint_db.clone();
        let sync_state = self.sync_state.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MintSyncError> {
            let mut conn = mint_db.lock().expect("mint_db lock poisoned");
            let st = sync_state.blocking_lock();
            apply_remote_snapshot(&mut conn, &remote, &st.account_deletion_floor)
        })
        .await
        .map_err(|e| MintSyncError::Other(format!("spawn_blocking: {e}")))??;
        Ok(())
    }
}
```

Update `MintSyncEngine` to hold a `shared: EngineShared` field. Wire it up in `new_for_test_with_debounce`:

```rust
pub struct MintSyncEngine {
    dirty: Arc<Notify>,
    flush_now: mpsc::Sender<()>,
    shutdown: mpsc::Sender<()>,
    shared: EngineShared,
}

// In new_for_test_with_debounce, after the channel setup:
let shared = EngineShared { mint_db: mint_db.clone(), content_store: content_store.clone(), sync_state: sync_state.clone() };
let dirty_for_task = dirty.clone();
let handle = tokio::spawn(internal_task(
    mint_db, content_store, sync_state, dirty_for_task, flush_rx, shutdown_rx, debounce,
));
(Self { dirty, flush_now: flush_tx, shutdown: shutdown_tx, shared }, MintSyncEngineHandle(handle))
```

- [ ] **Step 3: Run the test**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(subscriber_applies_remote_snapshot_to_local)'
```

Expected: PASS.

- [ ] **Step 4: Verify the full suite**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(mint) or test(mint_sync))'
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mint_sync.rs src-tauri/src/content_store.rs
git commit -m "feat(mint-sync): subscriber path — fetch, decode, merge

handle_incoming_envelope_for_test simulates a Zenoh-delivered envelope:
fetches the blob from ContentStore, CBOR-decodes, checks schema_version,
and runs apply_remote_snapshot in a blocking task. Task 11 will replace
the _for_test entry point with a real Zenoh subscriber + envelope decrypt."
```

---

## Task 10: Boot-hook flush

Schedule a one-shot `flush_now` 500ms after engine init, so a device that boots after a peer still propagates its existing state.

**Files:**
- Modify: `src-tauri/src/mint_sync.rs`

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/mint_sync.rs::tests`:

```rust
#[tokio::test]
async fn boot_hook_publishes_when_non_empty() {
    let mut conn = fresh_db();
    seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
    let conn = Arc::new(std::sync::Mutex::new(conn));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine, handle) = MintSyncEngine::new_for_test_with_boot_delay(
        conn,
        cs.clone(),
        sync_state,
        std::time::Duration::from_millis(50),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(cs.debug_count().await, 1, "boot-hook should have published");

    engine.shutdown().await.unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn boot_hook_skips_when_empty() {
    let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
    let cs: Arc<dyn crate::content_store::ContentStore> =
        Arc::new(crate::content_store::InMemoryStub::default());
    let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine, handle) = MintSyncEngine::new_for_test_with_boot_delay(
        conn,
        cs.clone(),
        sync_state,
        std::time::Duration::from_millis(50),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(cs.debug_count().await, 0, "boot-hook on empty db should no-op");

    engine.shutdown().await.unwrap();
    handle.await.unwrap();
}
```

- [ ] **Step 2: Implement the boot-hook**

In `src-tauri/src/mint_sync.rs`:

```rust
pub const DEFAULT_BOOT_FLUSH_DELAY_MS: u64 = 500;

impl MintSyncEngine {
    pub async fn new_for_test_with_boot_delay(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        boot_delay: std::time::Duration,
    ) -> (Self, MintSyncEngineHandle) {
        let (engine, handle) = Self::new_for_test(mint_db, content_store, sync_state).await;
        let flush_tx = engine.flush_now.clone();
        tokio::spawn(async move {
            tokio::time::sleep(boot_delay).await;
            let _ = flush_tx.send(()).await;
        });
        (engine, handle)
    }
}
```

(The boot-hook scheduler is split from `new_for_test` so existing tests that don't want a boot-hook flush can use `new_for_test`. The "real" `new` in Task 11 always schedules the boot-hook with `DEFAULT_BOOT_FLUSH_DELAY_MS`.)

- [ ] **Step 3: Run the tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(boot_hook_publishes_when_non_empty) or test(boot_hook_skips_when_empty)'
```

Expected: both PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mint_sync.rs
git commit -m "feat(mint-sync): boot-hook flush 500ms after engine init

One-shot flush_now task fires DEFAULT_BOOT_FLUSH_DELAY_MS after engine
construction, ensuring a device that comes online after a peer still
propagates its current state without waiting for a user mutation.
Empty-snapshot skip (Task 8) means a brand-new empty device boot-hooks
into a no-op."
```

---

## Task 11: IPC notify_dirty hooks + NodeState wiring + real engine `new`

This is the heaviest task: wire the engine into `NodeState`, gate init on identity, attach `notify_dirty()` calls to every mint mutation IPC, fold the temporary `mint_pending_account_floor` field from Task 5 into `MintSyncState`, and finally add the real Zenoh-wired `MintSyncEngine::new` constructor.

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState field; engine init; shutdown hook)
- Modify: `src-tauri/src/mint.rs` (every IPC command picks up `engine.notify_dirty()`)
- Modify: `src-tauri/src/mint_sync.rs` (real `new`; encryption + Zenoh)

- [ ] **Step 1: Define the real `MintSyncEngine::new` signature**

Look at `owner_state_sync::SyncEngine::new` for the canonical shape. Locate it:

```
cd src-tauri && grep -n "pub fn new" src/owner_state_sync.rs | head -5
```

Mirror that signature in `mint_sync.rs`:

```rust
impl MintSyncEngine {
    pub async fn new(
        zenoh_session: Arc<zenoh::Session>,                  // from existing NodeState
        content_store: Arc<dyn crate::content_store::ContentStore>,
        key_tree: Arc<crate::owner_state_crypto::KeyTree>,
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        device_id: [u8; 16],                                  // from owner-state
        owner_addr_hex: String,
        sync_state_path: std::path::PathBuf,                  // for persist on tracker bump
    ) -> Result<(Self, MintSyncEngineHandle), MintSyncError> {
        // Topic + lookup key:
        let topic = format!("harmony/owner/{owner_addr_hex}/mint-root-v1");
        let lookup_key = crate::owner_state_crypto::space_lookup_key(
            &key_tree,
            b"mint-ledger-v1",
        );

        // Open Zenoh publisher + subscriber (mirror owner_state_sync's pattern).
        let publisher = zenoh_session.declare_publisher(&topic).res().await
            .map_err(|e| MintSyncError::Other(format!("zenoh publisher: {e}")))?;
        let subscriber = zenoh_session.declare_subscriber(&topic).res().await
            .map_err(|e| MintSyncError::Other(format!("zenoh subscriber: {e}")))?;

        // ... wire publish_root_now to encrypt + zenoh.put,
        //     wire subscriber loop to decrypt + handle_incoming,
        //     spawn boot-hook task.

        // The exact wiring details mirror src/owner_state_sync.rs::SyncEngine::new
        // — refer to that file as the authoritative pattern; the call shapes
        // are identical except for the topic name, lookup key, and snapshot type.

        todo!("Mirror owner_state_sync::SyncEngine::new — encryption + Zenoh wiring")
    }
}
```

**Implementation note for the subagent**: the `todo!()` macro is a placeholder for the engineer doing the work to fill in by copy-adapting from `owner_state_sync.rs`. The plan does NOT count this as a final-placeholder — Task 11's acceptance criterion below requires that line to be a working implementation.

The encryption step replaces the Task 8 `// TODO(Task 11)` comment:

```rust
// Replace in publish_root_now:
let ciphertext = crate::owner_state_crypto::encrypt_entry(
    &key_tree, &lookup_key, &cbor,
).map_err(|e| MintSyncError::Crypto(format!("encrypt: {e}")))?;
let root_cid = crate::owner_state_types::ContentId(blake3::hash(&ciphertext).into());
content_store.put(root_cid, ciphertext).await?;

// Then envelope:
let payload = MintRootPublishPayload { root_cid, at: next_hlc(device_id, &last_published_hlc) };
let payload_cbor = serde_cbor::to_vec(&payload)?;
let wire = crate::owner_state_crypto::encrypt_root_publish(&key_tree, &payload_cbor)?;
publisher.put(wire).res().await.map_err(|e| MintSyncError::Other(format!("zenoh put: {e}")))?;
```

The subscriber loop's decryption side mirrors:

```rust
let payload_pt = crate::owner_state_crypto::decrypt_root_publish(&key_tree, &wire)?;
let payload: MintRootPublishPayload = serde_cbor::from_slice(&payload_pt)?;
// Replay check:
if !sync_state.lock().await.replay_tracker.accept(&payload.at) { return; }
// Echo suppression:
if payload.at.device_id == device_id { return; }
// Fetch + decrypt blob:
let blob_ct = content_store.get(&payload.root_cid).await?
    .ok_or(MintSyncError::MissingBlob(payload.root_cid))?;
let blob_pt = crate::owner_state_crypto::decrypt_entry(&key_tree, &lookup_key, &blob_ct)?;
let remote: MintSnapshot = serde_cbor::from_slice(&blob_pt)?;
// Schema check + apply (Task 9 logic, unchanged):
self.shared.handle_incoming_decoded(remote).await?;
```

(Refactor `handle_incoming` to a `handle_incoming_decoded` that takes the already-decoded `MintSnapshot`; the test-only `handle_incoming_envelope_for_test` becomes a thin shim that decodes then delegates.)

- [ ] **Step 2: Add NodeState wiring in `src-tauri/src/lib.rs`**

Find the existing `NodeState` definition. Add:

```rust
pub mint_sync: tokio::sync::RwLock<Option<Arc<crate::mint_sync::MintSyncEngine>>>,
```

(Use `RwLock<Option<...>>` because the engine init is deferred until identity is available.)

REMOVE the temporary `mint_pending_account_floor` field from Task 5 — replace it with reading from `mint_sync_state.account_deletion_floor` via the engine's `sync_state` handle.

In `NodeState::new` (or equivalent constructor):

```rust
mint_sync: tokio::sync::RwLock::new(None),
```

In the identity-bootstrap hook (the same place owner-state's SyncEngine is constructed — look for it via `grep -n "OwnerStateSyncEngine\|SyncEngine::new" src-tauri/src/lib.rs`), add after owner-state engine setup:

```rust
// Mint sync engine
let mint_db = mint_db_handle(&state).await?;
let mint_sync_state_path = app_data_dir.join("mint").join("mint_sync_state.cbor");
let mint_sync_state = Arc::new(TokioMutex::new(
    crate::mint_sync_persist::load(&mint_sync_state_path)
        .unwrap_or_else(|e| {
            tracing::warn!(target: "mint_sync", "load mint_sync_state failed: {e}; using default");
            crate::mint_sync_types::MintSyncState::default()
        })
));
let (mint_engine, mint_handle) = crate::mint_sync::MintSyncEngine::new(
    zenoh_session.clone(),
    content_store.clone(),
    key_tree.clone(),
    mint_db,
    mint_sync_state,
    device_id,
    owner_addr_hex.clone(),
    mint_sync_state_path,
).await?;
*state.mint_sync.write().await = Some(Arc::new(mint_engine));
// Stash handle so shutdown hook can await it. Look for owner-state's pattern.
```

- [ ] **Step 3: Add `notify_dirty()` to every mint mutation IPC**

For each of the following IPC commands in `src-tauri/src/mint.rs`, add `engine.notify_dirty()` after the SQL succeeds and before returning the Ok response. The helper:

```rust
async fn notify_mint_dirty(state: &tauri::State<'_, NodeState>) {
    if let Some(engine) = state.mint_sync.read().await.as_ref() {
        engine.notify_dirty();
    }
}
```

Commands to update:

1. `mint_create_transaction`
2. `mint_update_transaction`
3. `mint_delete_transaction`
4. `mint_create_account`
5. `mint_rename_account`
6. `mint_delete_account`
7. `mint_set_default_currency`

(Read-only IPCs like `mint_list_transactions`, `mint_get_transaction`, `mint_list_accounts`, `mint_get_default_currency`, `mint_export_csv` do NOT need notify_dirty.)

For `mint_delete_account`, replace the temporary `mint_pending_account_floor` Task-5 thread with the real one:

```rust
#[tauri::command]
pub(crate) async fn mint_delete_account(
    id: String,
    reassign_to: Option<String>,
    state: tauri::State<'_, NodeState>,
) -> Result<(), String> {
    let db = mint_db_handle(&state).await.map_err(|e| e.to_string())?;
    let engine_guard = state.mint_sync.read().await;
    let sync_state = match engine_guard.as_ref() {
        Some(e) => e.sync_state_handle(),
        None => return Err("mint sync engine not yet initialized".into()),
    };
    drop(engine_guard); // release read lock before spawn_blocking

    let id_clone = id.clone();
    let reassign_clone = reassign_to.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = db.lock().expect("mint_db lock poisoned");
        let mut st = sync_state.blocking_lock();
        crate::mint::delete_account(
            &mut conn,
            &id_clone,
            reassign_clone.as_deref(),
            &mut st.account_deletion_floor,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    notify_mint_dirty(&state).await;
    Ok(())
}
```

Add a getter on `MintSyncEngine`:

```rust
impl MintSyncEngine {
    pub fn sync_state_handle(&self) -> Arc<TokioMutex<MintSyncState>> {
        self.shared.sync_state.clone()
    }
}
```

REMOVE the `mint_pending_account_floor` field from `NodeState` (it served Task 5 only; the real floor lives in `MintSyncState` now).

- [ ] **Step 4: Wire shutdown**

In the existing Tauri shutdown hook in `src-tauri/src/lib.rs` (where owner-state's `SyncEngine::shutdown` is awaited), add:

```rust
if let Some(engine) = state.mint_sync.write().await.take() {
    if let Err(e) = engine.shutdown().await {
        tracing::warn!(target: "mint_sync", "shutdown error: {e}");
    }
}
```

- [ ] **Step 5: Verify the full suite**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(mint) or test(mint_sync))'
```

Expected: all PASS.

```
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: zero warnings.

```
cd src-tauri && cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mint_sync.rs src-tauri/src/mint.rs src-tauri/src/lib.rs
git commit -m "feat(mint-sync): real engine wiring — Zenoh + AEAD + IPC notify_dirty

MintSyncEngine::new mirrors owner_state_sync::SyncEngine::new — opens a
Zenoh publisher + subscriber on harmony/owner/{addr_hex}/mint-root-v1,
encrypts/decrypts via space_lookup_key(&kt, b\"mint-ledger-v1\"), and runs
the same debounce + boot-hook pattern.

Every mint mutation IPC (create/update/delete transaction, create/rename/delete
account, set_default_currency) calls engine.notify_dirty() after its SQL succeeds.

Removed the Task-5 temporary mint_pending_account_floor on NodeState; the real
floor lives in MintSyncState behind the engine's sync_state_handle()."
```

---

## Task 12: Frontend mint-changed event

Emit a Tauri event after every successful subscriber merge so the Svelte UI re-fetches.

**Files:**
- Modify: `src-tauri/src/mint_sync.rs` (emit event after apply)
- Modify: `src/lib/components/MintLedger.svelte` (listen + reload)
- Create: `src/lib/components/__tests__/MintLedger.event.test.ts` (or extend an existing test file)

- [ ] **Step 1: Emit `mint-changed` after a successful merge**

The subscriber path needs an `AppHandle` to call `app.emit("mint-changed", ())`. Pass it into the engine constructor.

In `src-tauri/src/mint_sync.rs`, extend `MintSyncEngine::new` to accept `app_handle: tauri::AppHandle`. Store it on `EngineShared`:

```rust
#[derive(Clone)]
struct EngineShared {
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
    app_handle: Option<tauri::AppHandle>,  // Option so test constructors can pass None
}
```

In `handle_incoming_decoded`, after a successful `apply_remote_snapshot` call:

```rust
if let Some(app) = &self.app_handle {
    use tauri::Emitter;
    let _ = app.emit("mint-changed", ());
}
```

Test-only `new_for_test_*` constructors pass `app_handle: None`.

- [ ] **Step 2: Update `MintLedger.svelte` to listen for the event**

In `src/lib/components/MintLedger.svelte`, after the existing imports add:

```typescript
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
```

After the existing `onMount(() => { void load(); });` block, replace with:

```typescript
onMount(() => {
    void load();
    let unlisten: UnlistenFn | undefined;
    void (async () => {
        unlisten = await listen('mint-changed', () => {
            void load();
        });
    })();
    return () => {
        if (unlisten) unlisten();
    };
});
```

- [ ] **Step 3: Write the failing vitest case**

Create `src/lib/components/__tests__/MintLedger.event.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, waitFor } from '@testing-library/svelte';
import MintLedger from '../MintLedger.svelte';
import { createMockAdapter } from '../../test-utils';

// Stub @tauri-apps/api/event with a controllable emitter.
const listeners = new Map<string, Array<(payload: unknown) => void>>();
vi.mock('@tauri-apps/api/event', () => ({
    listen: (event: string, cb: (e: { payload: unknown }) => void) => {
        const arr = listeners.get(event) ?? [];
        const wrapped = (payload: unknown) => cb({ payload });
        arr.push(wrapped);
        listeners.set(event, arr);
        return Promise.resolve(() => {
            const idx = arr.indexOf(wrapped);
            if (idx >= 0) arr.splice(idx, 1);
        });
    },
}));
function emitMintChanged() {
    for (const cb of listeners.get('mint-changed') ?? []) cb(undefined);
}

describe('MintLedger reacts to mint-changed event', () => {
    beforeEach(() => listeners.clear());

    it('calls list_transactions again when mint-changed fires', async () => {
        const adapter = createMockAdapter({
            mint_list_transactions: vi.fn().mockResolvedValue([]),
            mint_list_accounts: vi.fn().mockResolvedValue([]),
            mint_get_default_currency: vi.fn().mockResolvedValue('USD'),
        });
        render(MintLedger, { props: { adapter } });

        // Initial onMount load:
        await waitFor(() =>
            expect(adapter.invoke).toHaveBeenCalledWith('mint_list_transactions', expect.any(Object))
        );
        const initialCalls = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.length;

        // Fire the synthetic event:
        emitMintChanged();

        // Verify another load() cycle fires:
        await waitFor(() => {
            const newCalls = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.length;
            expect(newCalls).toBeGreaterThan(initialCalls);
        });
    });
});
```

- [ ] **Step 4: Run the test**

```
npx vitest run src/lib/components/__tests__/MintLedger.event.test.ts
```

Expected: PASS.

- [ ] **Step 5: Run full frontend gates**

```
npx tsc --noEmit
npx vitest run
```

Expected: clean type check; all 1907+ tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mint_sync.rs src/lib/components/MintLedger.svelte src/lib/components/__tests__/MintLedger.event.test.ts
git commit -m "feat(mint-sync): mint-changed Tauri event drives UI reload

Subscriber merges emit 'mint-changed' via AppHandle.emit; MintLedger.svelte
listens in onMount and re-invokes load() on each event. Cleanup unlisten
on component teardown via onMount's return value."
```

---

## Task 13: Two-engine integration tests

The load-bearing test: stand up two `MintSyncEngine`s sharing one `InMemoryStub` ContentStore, drive them through the convergence scenarios from the spec, verify each one converges.

**Files:**
- Create: `src-tauri/tests/mint_sync_integration.rs`

- [ ] **Step 1: Write the integration test suite**

Create `src-tauri/tests/mint_sync_integration.rs`:

```rust
//! Two-engine convergence tests for Mint Phase 2 sync.

use harmony_app::content_store::{ContentStore, InMemoryStub};
use harmony_app::mint::{
    apply_migrations, create_account, create_transaction, delete_transaction,
    open_in_memory, set_default_currency, update_transaction, NewTransaction,
    UpdateTransaction,
};
use harmony_app::mint_sync::MintSyncEngine;
use harmony_app::mint_sync_types::MintSyncState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

struct Harness {
    conn_a: Arc<std::sync::Mutex<rusqlite::Connection>>,
    conn_b: Arc<std::sync::Mutex<rusqlite::Connection>>,
    cs: Arc<dyn ContentStore>,
    engine_a: Arc<MintSyncEngine>,
    engine_b: Arc<MintSyncEngine>,
    handle_a: harmony_app::mint_sync::MintSyncEngineHandle,
    handle_b: harmony_app::mint_sync::MintSyncEngineHandle,
}

async fn setup() -> Harness {
    let mut a = open_in_memory().unwrap();
    apply_migrations(&mut a).unwrap();
    let mut b = open_in_memory().unwrap();
    apply_migrations(&mut b).unwrap();

    let conn_a = Arc::new(std::sync::Mutex::new(a));
    let conn_b = Arc::new(std::sync::Mutex::new(b));
    let cs: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
    let state_a = Arc::new(TokioMutex::new(MintSyncState::default()));
    let state_b = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine_a, handle_a) =
        MintSyncEngine::new_for_test(conn_a.clone(), cs.clone(), state_a).await;
    let (engine_b, handle_b) =
        MintSyncEngine::new_for_test(conn_b.clone(), cs.clone(), state_b).await;
    Harness {
        conn_a,
        conn_b,
        cs,
        engine_a: Arc::new(engine_a),
        engine_b: Arc::new(engine_b),
        handle_a,
        handle_b,
    }
}

/// Drive engine_a → publish, then deliver to engine_b. Returns once b has applied.
async fn sync_once(h: &Harness) {
    h.engine_a.flush_now().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cids = (h.cs.clone() as Arc<dyn ContentStore>)
        .downcast_test_cids()
        .await; // helper added below
    for cid in cids {
        let _ = h.engine_b.handle_incoming_envelope_for_test(cid).await;
    }
}

#[tokio::test]
async fn two_engines_converge_on_inserts() {
    let h = setup().await;
    {
        let mut conn = h.conn_a.lock().unwrap();
        let acct = create_account(&mut conn, "Chase").unwrap();
        for i in 0..5 {
            create_transaction(
                &mut conn,
                NewTransaction {
                    transaction_date: "2026-05-01".into(),
                    amount: format!("-{i}"),
                    currency: "USD".into(),
                    account_id: acct.id.clone(),
                    description: format!("expense-{i}"),
                    metadata: None,
                },
            )
            .unwrap();
        }
    }
    sync_once(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM transactions WHERE deleted_at IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 5);
    }
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}

#[tokio::test]
async fn two_engines_converge_on_updates() {
    let h = setup().await;
    let tx_id = {
        let mut conn = h.conn_a.lock().unwrap();
        let acct = create_account(&mut conn, "Chase").unwrap();
        let tx = create_transaction(
            &mut conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-10".into(),
                currency: "USD".into(),
                account_id: acct.id.clone(),
                description: "original".into(),
                metadata: None,
            },
        )
        .unwrap();
        tx.id
    };
    sync_once(&h).await;
    // Edit on A
    {
        let mut conn = h.conn_a.lock().unwrap();
        update_transaction(
            &mut conn,
            &tx_id,
            UpdateTransaction {
                description: Some("edited".into()),
                ..Default::default()
            },
        )
        .unwrap();
    }
    sync_once(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let desc: String = conn
            .query_row(
                "SELECT description FROM transactions WHERE id = ?",
                [&tx_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(desc, "edited");
    }
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}

#[tokio::test]
async fn two_engines_converge_on_delete() {
    let h = setup().await;
    let tx_id = {
        let mut conn = h.conn_a.lock().unwrap();
        let acct = create_account(&mut conn, "Chase").unwrap();
        let tx = create_transaction(
            &mut conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-10".into(),
                currency: "USD".into(),
                account_id: acct.id.clone(),
                description: "x".into(),
                metadata: None,
            },
        )
        .unwrap();
        tx.id
    };
    sync_once(&h).await;
    {
        let mut conn = h.conn_a.lock().unwrap();
        delete_transaction(&mut conn, &tx_id).unwrap();
    }
    sync_once(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let deleted: Option<String> = conn
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                [&tx_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted.is_some());
        // Verify it's filtered from live reads:
        let live_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live_count, 0);
    }
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}

#[tokio::test]
async fn two_engines_converge_on_setting_change() {
    let h = setup().await;
    {
        let mut conn = h.conn_a.lock().unwrap();
        set_default_currency(&mut conn, "JPY").unwrap();
    }
    sync_once(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'default_currency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "JPY");
    }
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}

#[tokio::test]
async fn concurrent_writes_to_distinct_rows_both_land() {
    let h = setup().await;
    let acct_id = {
        let mut conn = h.conn_a.lock().unwrap();
        create_account(&mut conn, "Chase").unwrap().id
    };
    sync_once(&h).await;
    // Insert on A
    let tx_a = {
        let mut conn = h.conn_a.lock().unwrap();
        create_transaction(
            &mut conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-1".into(),
                currency: "USD".into(),
                account_id: acct_id.clone(),
                description: "from-A".into(),
                metadata: None,
            },
        )
        .unwrap()
        .id
    };
    // Insert on B
    let tx_b = {
        let mut conn = h.conn_b.lock().unwrap();
        create_transaction(
            &mut conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-2".into(),
                currency: "USD".into(),
                account_id: acct_id.clone(),
                description: "from-B".into(),
                metadata: None,
            },
        )
        .unwrap()
        .id
    };
    // A publishes; B applies. Then B publishes; A applies.
    sync_once(&h).await;
    h.engine_b.flush_now().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let cids = h.cs.downcast_test_cids().await;
        for cid in cids {
            let _ = h.engine_a.handle_incoming_envelope_for_test(cid).await;
        }
    }
    // Both DBs should have both transactions.
    for conn in [&h.conn_a, &h.conn_b] {
        let conn = conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE id IN (?, ?)",
                [&tx_a, &tx_b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}
```

Add the `downcast_test_cids` helper on `Arc<dyn ContentStore>` — it's a convenience that lists every CID currently in the stub. In `src-tauri/src/content_store.rs`:

```rust
#[cfg(any(test, feature = "test-fixtures"))]
pub trait ContentStoreTestExt {
    async fn downcast_test_cids(&self) -> Vec<crate::owner_state_types::ContentId>;
}

#[cfg(any(test, feature = "test-fixtures"))]
#[async_trait::async_trait]
impl ContentStoreTestExt for Arc<dyn ContentStore> {
    async fn downcast_test_cids(&self) -> Vec<crate::owner_state_types::ContentId> {
        // Downcast hint: in tests, we know this is an InMemoryStub.
        // If the stub doesn't expose its keys, add a `pub fn cids(&self) -> Vec<ContentId>` to it.
        unimplemented!("Add a debug_all_cids accessor on InMemoryStub from Task 9; \
                       re-export it through this trait.")
    }
}
```

(Realistically, the cleanest path is to NOT add an Ext trait and instead pass `cs` everywhere typed as `Arc<InMemoryStub>` in the tests, calling `.debug_all_cids()` directly. Refactor the harness accordingly if the trait gymnastics get too tangled.)

- [ ] **Step 2: Run the integration tests**

```
cd src-tauri && cargo nextest run --locked --features test-fixtures --test mint_sync_integration
```

Expected: 5 tests PASS.

- [ ] **Step 3: Run all gates**

```
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all -- --check
cd .. && npx tsc --noEmit
cd .. && npx vitest run
```

Expected: everything green.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/mint_sync_integration.rs src-tauri/src/content_store.rs
git commit -m "test(mint-sync): two-engine convergence integration suite

Five scenarios from the spec's testing plan:
  - two_engines_converge_on_inserts (5 rows)
  - two_engines_converge_on_updates (LWW preserves later edit)
  - two_engines_converge_on_delete (tombstone propagates + filters)
  - two_engines_converge_on_setting_change (default currency)
  - concurrent_writes_to_distinct_rows_both_land

Harness shares one InMemoryStub ContentStore between two engines and
drives them through the publish → apply cycle via flush_now +
handle_incoming_envelope_for_test, mirroring how Zenoh would deliver."
```

---

## Final acceptance gates

After Task 13 is committed:

- [ ] **All four CI-equivalent gates green:**

```
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all -- --check
cd .. && npx tsc --noEmit
cd .. && npx vitest run
```

- [ ] **Spec coverage**: every section of `docs/specs/2026-05-19-mint-sync-design.md` has a corresponding task above. Section ↔ task map:

  - "Module layout" → Tasks 3, 4, 6, 7
  - "Transport surface" → Task 11
  - "Disk surface" → Task 6
  - "Internal task loop" → Task 8
  - "Schema changes" → Tasks 1, 2
  - "Snapshot CBOR shape" → Task 3
  - "Snapshot lifecycle / Publisher path" → Tasks 8, 11
  - "Snapshot lifecycle / Subscriber path" → Tasks 9, 11
  - "Snapshot lifecycle / Lifecycle (boot + shutdown)" → Tasks 10, 11
  - "Merge semantics / Apply order, LWW, tombstones" → Task 4
  - "Merge semantics / Deletion floor" → Task 5
  - "First-run + bootstrap" → Task 10
  - "Error handling / Publisher + Subscriber" → Tasks 8, 9
  - "Error handling / Schema drift" → Tasks 3 (LOCAL_MAX_SCHEMA_VERSION), 9 (subscriber check)
  - "Testing plan / unit, integration, frontend, manual" → Tasks throughout + Task 13

- [ ] **Open PR**: branch is `mint-sync`; push and `gh pr create --title "Mint Phase 2 sync — CAS-backed multi-device ledger sync" --body ...`.

- [ ] **Manual smoke test** (Section 7 step 4 in the spec): build on Ildwyn + Koya, pair, verify two-device convergence on the four scenarios listed.

---

## Notes for the executing engineer

- **Architecture template**: when in doubt about how a piece should look (publisher loop, subscriber loop, replay tracker integration, error surface), look at `src-tauri/src/owner_state_sync.rs` first — Phase 2 mint sync is structurally a copy of it. Do not invent novel patterns; the goal is "owner-state's proven shape, mint's types."
- **Snake_case Rust / camelCase JS**: enforced per CLAUDE.md. Tauri IPC params are declared `snake_case` in Rust, called `camelCase` from JS, and Tauri auto-converts. Get this wrong and the parameter arrives as `undefined`.
- **`updated_at` always bumped**: every mutation that writes to accounts, transactions, or settings must bump `updated_at` to `chrono::Utc::now().to_rfc3339()`. This is load-bearing for per-row LWW. Spec D5 + D8.
- **No CI re-enable**: Jake's standing directive. Do not propose adding workflows back. Local gates (clippy, fmt, nextest, tsc, vitest) are the contract.
- **Never amend**: every fix is a new commit. Per CLAUDE.md and Jake's preference.
- **Windows DLL `STATUS_ENTRYPOINT_NOT_FOUND`** is NOT a real failure if it surfaces — known quirk.
- **`let _ = ...` on ALTER TABLE ADD COLUMN** is the idempotency idiom. Don't replace with explicit error checking — second-run idempotency depends on this exact pattern.
