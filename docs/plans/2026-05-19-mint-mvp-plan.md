# Mint MVP — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task with the implementer → spec reviewer → code quality reviewer loop. Each task is sized to be one commit.

**Spec:** [`docs/specs/2026-05-19-mint-mvp-design.md`](../specs/2026-05-19-mint-mvp-design.md)
**Branch:** `mint-mvp`
**Goal:** Local-only personal-finance transaction tracker with full CRUD and CSV export, no network sync.
**Architecture:** Tauri-managed SQLite database via `rusqlite` (`bundled` feature). Synchronous rusqlite operations wrapped in `tokio::task::spawn_blocking`. Frontend: new Svelte feature panel + service mirroring existing `*-service.ts` patterns.
**Tech stack:** Rust (rusqlite, rust_decimal, chrono, csv, serde), TypeScript, Svelte 5, Tauri 2.

---

## Task overview

```text
Task 1: Cargo deps + mint.rs scaffolding + schema migration + settings + unit tests
      ↓
Task 2: Account CRUD (mint.rs sync layer + unit tests)
      ↓
Task 3: Transaction CRUD (mint.rs sync layer + unit tests)
      ↓
Task 4: Tauri command layer (spawn_blocking wrappers, NodeState wiring, integration tests)
      ↓
Task 5: CSV export (Rust side: writer + Tauri command + integration tests)
      ↓
Task 6: TypeScript types + MintService + vitest
      ↓
Task 7: MintLedger top-level panel + transaction table + component tests
      ↓
Task 8: Transaction add/edit dialog + component tests
      ↓
Task 9: Account manager dialog + component tests
      ↓
Task 10: AppMode wiring + CSV export UI flow + smoke test
```

Each task is one commit (10 commits total). Subagent dispatches one implementer + two reviewers per task.

---

## Task 1 — Cargo deps + `mint.rs` scaffolding + schema + settings

**Spec sections:** "Architecture > Module placement", "Architecture > Database connection lifecycle", "Architecture > Schema" (just the `settings` table and migration setup).

**Files:**
- `src-tauri/Cargo.toml` — add three new dependencies (rusqlite, rust_decimal, csv); chrono is **not** added because the spec only uses ISO 8601 date strings handled as `String` until Task 3 introduces validation.
  ```toml
  rusqlite = { version = "0.31", features = ["bundled"] }
  rust_decimal = "1"
  csv = "1"
  chrono = "0.4"
  ```
  (Rationale: rusqlite `bundled` ships SQLite source so we don't depend on a system library; `csv` is for Task 5; `chrono::NaiveDate` is the date validator; `rust_decimal::Decimal` is the amount validator. All four are imported in this task even though only some are used immediately — adding them in one bump avoids three Cargo.toml churns.)
- `src-tauri/src/mint.rs` (new) — module skeleton with these public items in this order:
  1. Module doc comment describing the feature and pointing to spec
  2. `pub struct MintError { ... }` — thiserror enum with variants: `Sqlite(rusqlite::Error)`, `Validation(String)`, `NotFound(String)`, `Other(String)`. `impl From<MintError> for String` for the IPC seam.
  3. `pub fn open_database(path: &std::path::Path) -> Result<rusqlite::Connection, MintError>` — opens (creates if absent), sets `PRAGMA journal_mode = WAL`, sets `PRAGMA foreign_keys = ON`, runs `apply_migrations(&conn)`.
  4. `fn apply_migrations(conn: &Connection) -> Result<(), MintError>` — runs the schema from spec section "Schema" via `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`. Idempotent. Seeds the `settings` table with `('default_currency', 'USD')` via `INSERT OR IGNORE`.
  5. `pub fn get_default_currency(conn: &Connection) -> Result<Option<String>, MintError>` — selects from `settings`.
  6. `pub fn set_default_currency(conn: &Connection, currency: &str) -> Result<(), MintError>` — validates the input is `^[A-Z]{1,5}$`, then `INSERT OR REPLACE`.
  7. `pub fn validate_currency(s: &str) -> Result<(), MintError>` — regex match (use `s.chars().all(|c| c.is_ascii_uppercase()) && (1..=5).contains(&s.len())` — no `regex` crate needed for this trivial check).
- `src-tauri/src/lib.rs` — add `pub mod mint;` next to the existing `pub mod folder_ingest;` line (alphabetical placement is fine — between `mail_sync` and `owner_commands`).
- `src-tauri/src/mint.rs` (same file, unit-test module at bottom):
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use rusqlite::Connection;

      fn fresh_db() -> Connection {
          let conn = Connection::open_in_memory().unwrap();
          apply_migrations(&conn).unwrap();
          conn
      }

      #[test]
      fn migration_creates_expected_tables() { /* SELECT name FROM sqlite_master WHERE type='table' */ }

      #[test]
      fn migration_is_idempotent() { /* run apply_migrations twice, no error */ }

      #[test]
      fn default_currency_seeded() { /* get_default_currency returns Some("USD") */ }

      #[test]
      fn set_default_currency_round_trip() { /* set to "JPY", get returns Some("JPY") */ }

      #[test]
      fn set_default_currency_rejects_lowercase() { /* assert Err(MintError::Validation(_)) */ }

      #[test]
      fn set_default_currency_rejects_too_long() { /* "USDXYZ" — 6 chars — rejected */ }

      #[test]
      fn set_default_currency_rejects_empty() { /* "" rejected */ }

      #[test]
      fn validate_currency_accepts_valid() { /* USD, JPY, AUD, BTC, UAVF all OK */ }
  }
  ```

**Constraints:**
- DO NOT add Tauri commands yet — Task 4 handles the IPC layer. Functions in this task are plain sync `fn`s.
- DO NOT add `mint_db` to `NodeState` yet — Task 4 handles that wiring.
- **DO put all three `CREATE TABLE` statements (accounts + transactions + settings) into `apply_migrations` in this task**, copied verbatim from the spec's Schema section. Tasks 2 and 3 only add CRUD functions on top — they don't modify the schema. One exception: Task 2 will retrofit `UNIQUE(name)` onto the `accounts` table; for Task 1, write the `accounts` table WITHOUT the UNIQUE constraint (Task 2 adds it). This minor split keeps each task's changes review-sized.
- Use `rusqlite::params!` macro (not positional `&[]` or named) for SQL parameter binding throughout.
- `MintError::Sqlite` should `#[from]` derive so `?` works from rusqlite calls.

**Test gates:**
- `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(mint::)'` — all unit tests green.
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — zero warnings.
- `cd src-tauri && cargo fmt --all -- --check` — clean.

**Commit message:**
```
feat(mint): scaffold module with schema migration and default-currency setting

Adds rusqlite/rust_decimal/csv/chrono dependencies, mint.rs module with
open_database/apply_migrations, settings table CRUD, and currency
validation. Account and transaction CRUD land in subsequent tasks.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 2 — Account CRUD

**Spec sections:** "Architecture > Schema" (accounts table), "API surface" (Account struct, account commands at the spec level — but we're not building Tauri commands yet, just sync fns), "Validation" (account name rules).

**Files:**
- `src-tauri/src/mint.rs` — extend with:
  1. `pub struct Account { pub id: String, pub name: String, pub created_at: String, pub transaction_count: u64 }` — derive `Debug, Clone, Serialize, Deserialize`.
  2. `pub fn create_account(conn: &Connection, name: &str) -> Result<Account, MintError>` — validates name, generates UUIDv4 (use existing `uuid` crate already in Cargo.toml), generates `created_at` from `chrono::Utc::now().to_rfc3339()`, inserts. Returns the new Account with `transaction_count: 0`. Enforces case-sensitive name uniqueness via `UNIQUE(name)` violation handling — if rusqlite returns `ErrorCode::ConstraintViolation`, map to `MintError::Validation("account name already exists")`.
     - To enforce uniqueness, ALSO update `apply_migrations` in Task 1's scope to add `UNIQUE(name)` to the accounts table definition. (Note: Task 1 wrote the migration without `UNIQUE(name)` — this task amends it. Since the migration uses `CREATE TABLE IF NOT EXISTS`, existing databases won't pick up the new constraint until they're recreated, which is fine for v1 since no real users exist yet.)
  3. `pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>, MintError>` — joins with transactions to compute `transaction_count` per account in a single query:
     ```sql
     SELECT a.id, a.name, a.created_at, COUNT(t.id) AS tx_count
     FROM accounts a
     LEFT JOIN transactions t ON t.account_id = a.id
     GROUP BY a.id
     ORDER BY a.name
     ```
  4. `pub fn rename_account(conn: &Connection, id: &str, new_name: &str) -> Result<Account, MintError>` — validates name, UPDATE, returns refreshed Account (re-query). If id not found → `MintError::NotFound`.
  5. `pub fn delete_account(conn: &Connection, id: &str, reassign_to: Option<&str>) -> Result<(), MintError>`:
     - If `reassign_to` is `Some(target_id)`: UPDATE transactions SET account_id = target_id WHERE account_id = id, then DELETE the account. Both inside a transaction (`conn.transaction()?`).
     - If `reassign_to` is `None`: first SELECT COUNT(*) FROM transactions WHERE account_id = id. If > 0, return `MintError::Validation("account has transactions; pass reassign_to")`. Else DELETE.
     - Validate that `reassign_to` (if Some) exists and is different from `id`.
  6. `pub fn validate_account_name(s: &str) -> Result<(), MintError>` — trim non-empty, byte length ≤ 256.
- `src-tauri/src/mint.rs` test module — add:
  ```rust
  #[test]
  fn create_account_basic() { /* one account, list shows it with count 0 */ }

  #[test]
  fn create_account_rejects_empty_name() { /* "" and "   " both rejected */ }

  #[test]
  fn create_account_rejects_oversized_name() { /* 257 bytes rejected */ }

  #[test]
  fn create_account_rejects_duplicate_name() { /* "Chase" twice → second errors */ }

  #[test]
  fn list_accounts_includes_transaction_count() {
      // Create account, insert raw transactions via SQL, verify count.
      // Use raw SQL here since transactions::create lands in Task 3.
  }

  #[test]
  fn rename_account_round_trip() { /* rename "Chase" → "Chase Checking", list shows new name */ }

  #[test]
  fn rename_account_not_found() { /* unknown UUID → NotFound */ }

  #[test]
  fn delete_account_empty_succeeds() { /* delete with no transactions, no reassign */ }

  #[test]
  fn delete_account_with_txns_no_reassign_fails() { /* insert a raw txn, delete blocks */ }

  #[test]
  fn delete_account_with_reassign_moves_txns() { /* insert 2 txns to A, delete A reassign-to B, all 2 now on B */ }

  #[test]
  fn delete_account_reassign_to_same_id_fails() { /* validation error */ }

  #[test]
  fn delete_account_reassign_to_missing_target_fails() { /* unknown target id */ }
  ```

**Constraints:**
- All functions are sync `fn`s — NO async, NO Tauri attributes. The Tauri layer in Task 4 wraps these with `spawn_blocking`.
- Use `conn.transaction()` for `delete_account_with_reassign` to make it atomic.
- UUID generation: `uuid::Uuid::new_v4().to_string()` (the `uuid` crate is already in Cargo.toml with the `v4` feature).
- Timestamps: `chrono::Utc::now().to_rfc3339()` — explicit RFC 3339 string for portability.

**Test gates:**
- `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(mint::)'` — all Task 1 + Task 2 unit tests green.
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — clean.
- `cd src-tauri && cargo fmt --all -- --check` — clean.

**Commit message:**
```
feat(mint): account CRUD with name uniqueness and reassign-on-delete

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 3 — Transaction CRUD

**Spec sections:** "Architecture > Schema" (transactions table), "API surface > Types" (Transaction, NewTransaction, UpdateTransaction), "Validation" (transaction_date, amount, metadata rules).

**Files:**
- `src-tauri/src/mint.rs` — extend with:
  1. `pub struct Transaction` — id, transaction_date, amount, currency, account_id, account_name, description, metadata, created_at, updated_at. All `String`/`Option<String>`. Derive `Debug, Clone, Serialize, Deserialize`.
  2. `pub struct NewTransaction` — transaction_date, amount, currency, account_id, description, metadata. Derive `Deserialize` only.
  3. `pub struct UpdateTransaction` — all fields are `Option<...>` (and `metadata: Option<Option<String>>` per spec). Derive `Deserialize`.
  4. `pub struct ListFilter { pub date_from: Option<String>, pub date_to: Option<String>, pub account_id: Option<String> }` — derive `Deserialize` only.
  5. Validation helpers:
     - `fn validate_date(s: &str) -> Result<(), MintError>` — regex `^\d{4}-\d{2}-\d{2}$` (use simple parse: `chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")`).
     - `fn validate_amount(s: &str) -> Result<(), MintError>` — parse via `rust_decimal::Decimal::from_str(s)` (the `Display` of `Decimal` round-trips); reject if it fails. Reasonable scale not enforced — Excel will handle arbitrary precision.
     - `fn validate_description(s: &str) -> Result<(), MintError>` — non-empty after trim, byte len ≤ 4096.
     - `fn validate_metadata(s: &str) -> Result<(), MintError>` — `serde_json::from_str::<serde_json::Value>(s)`; reject malformed; reject if serialized bytes > 65_536 (64 KiB).
  6. CRUD functions:
     - `pub fn create_transaction(conn: &Connection, payload: NewTransaction) -> Result<Transaction, MintError>` — validate all fields, verify account_id exists, generate UUID + timestamps, INSERT, return Transaction (re-query to populate `account_name`).
     - `pub fn get_transaction(conn: &Connection, id: &str) -> Result<Option<Transaction>, MintError>` — SELECT with JOIN to accounts for name, `Option::None` if no rows.
     - `pub fn list_transactions(conn: &Connection, filter: &ListFilter) -> Result<Vec<Transaction>, MintError>` — dynamic WHERE clause built from filter, ORDER BY transaction_date DESC, id DESC. Always JOINs accounts for `account_name`.
     - `pub fn update_transaction(conn: &Connection, id: &str, payload: UpdateTransaction) -> Result<Transaction, MintError>` — validate each present field; build dynamic UPDATE; refresh `updated_at`; verify account_id (if changed) exists; return refreshed Transaction. `metadata: Some(Some(s))` → set to s; `metadata: Some(None)` → set to NULL; `metadata: None` → leave alone.
     - `pub fn delete_transaction(conn: &Connection, id: &str) -> Result<(), MintError>` — DELETE; if 0 rows affected → `MintError::NotFound`.

**Constraints:**
- All amount strings must round-trip through `rust_decimal::Decimal`. So `"42.50"` in → stored as `"42.50"`, returned as `"42.50"`. (`Decimal::to_string()` preserves trailing zeros only if you use `Decimal::round_dp_with_strategy` first; simplest is to store the input string verbatim AFTER validation succeeds, not re-format.)
- The dynamic UPDATE in `update_transaction` should use a `Vec<&dyn ToSql>` parameter array. Reference pattern (clean):
  ```rust
  let mut sets: Vec<&'static str> = Vec::new();
  let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
  if let Some(d) = &payload.transaction_date {
      validate_date(d)?;
      sets.push("transaction_date = ?");
      params.push(Box::new(d.clone()));
  }
  // ... other fields
  sets.push("updated_at = ?");
  params.push(Box::new(now_rfc3339()));
  params.push(Box::new(id.to_string()));
  let sql = format!("UPDATE transactions SET {} WHERE id = ?", sets.join(", "));
  conn.execute(&sql, rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())))?;
  ```
- For `list_transactions`, also build WHERE dynamically with the same pattern.
- DO NOT add Tauri commands yet — Task 4.
- The metadata `Option<Option<String>>` distinction is critical — make sure your `Deserialize` derive on `UpdateTransaction` uses `#[serde(default, deserialize_with = "deserialize_some")]` or equivalent so `{"metadata": null}` deserializes as `Some(None)` and absent field deserializes as `None`. Reference helper:
  ```rust
  fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
  where T: serde::Deserialize<'de>, D: serde::Deserializer<'de> {
      T::deserialize(deserializer).map(Some)
  }
  ```
  Then on UpdateTransaction: `#[serde(default, deserialize_with = "deserialize_some")] pub metadata: Option<Option<String>>`.

**Test gates:**
- Add ≥ 20 unit tests in `mint.rs::tests`:
  - validate_date accepts `2026-05-19`, rejects `2026-13-01`, `26-05-19`, ` 2026-05-19 ` (with whitespace), empty
  - validate_amount accepts `42.50`, `-42.50`, `0`, `0.00001`; rejects `abc`, `4,5` (comma), `1e5`, empty
  - validate_description accepts `"Coffee"`, rejects empty, all-whitespace, 4097-byte
  - validate_metadata accepts `{}`, `{"tag":"travel"}`, `null`, `[]`; rejects `not json`, `{`, oversized (>64KiB)
  - create_transaction happy path
  - create_transaction with each invalid field rejected
  - create_transaction with unknown account_id rejected
  - get_transaction returns None for unknown id
  - list_transactions: empty filter, date_from only, date_to only, both, account_id, all three combined
  - list_transactions order: most recent date first, secondary by id
  - update_transaction: each field individually
  - update_transaction metadata distinction: Some(Some) sets, Some(None) clears, None leaves alone — 3 separate tests
  - update_transaction with unknown account_id rejected
  - update_transaction returns NotFound for unknown id
  - update_transaction bumps updated_at
  - delete_transaction happy path
  - delete_transaction NotFound for unknown id
- All test commands from Task 2 — green.

**Commit message:**
```
feat(mint): transaction CRUD with full field validation

Adds Transaction/NewTransaction/UpdateTransaction types, validation
helpers for date/amount/currency/description/metadata, and the
sync rusqlite functions for create/get/list/update/delete.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 4 — Tauri command layer + NodeState wiring + integration tests

**Spec sections:** "Architecture > Database connection lifecycle" (spawn_blocking pattern), "API surface > Tauri command set".

**Files:**
- `src-tauri/src/lib.rs` — modifications:
  1. NodeState struct (around line 323): add field
     ```rust
     /// Mint personal-finance database. Lazily opened from
     /// app_data_dir/mint/ledger.db on first invocation of any mint_*
     /// command. None until first use; subsequent calls reuse.
     mint_db: Option<std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>>,
     ```
  2. NodeState::default() (or wherever the struct is constructed with `..Default::default()` semantics): initialize `mint_db: None`.
  3. Helper: `fn mint_db_handle(app: &tauri::AppHandle, state: &State<'_, Mutex<NodeState>>) -> Result<Arc<Mutex<Connection>>, String>` — lazy-init pattern. Resolves `app_data_dir`, joins `mint/ledger.db`, creates parent dirs, calls `mint::open_database`, stores Arc back into NodeState. Returns the Arc. Subsequent calls return the cached Arc. (Place this helper inside lib.rs near the existing Tauri command helpers — search for the `pin_content` function as a placement landmark.)
  4. Register all mint_* commands in the existing `invoke_handler![ ... ]` block at lib.rs:21877 — add them at the bottom of the list (alphabetical not strictly maintained in that block).
- `src-tauri/src/mint.rs` — add Tauri command layer at the bottom of the file (after the sync functions, before the `#[cfg(test)] mod tests` block):
  ```rust
  // ── Tauri command layer ──────────────────────────────────────
  // All commands run sync rusqlite work inside spawn_blocking to
  // avoid blocking the tokio executor (see spec § Database connection
  // lifecycle).

  #[tauri::command]
  pub async fn mint_list_accounts(
      app: tauri::AppHandle,
      state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
  ) -> Result<Vec<Account>, String> {
      let conn = crate::mint_db_handle(&app, &state)?;
      tokio::task::spawn_blocking(move || {
          let conn = conn.lock().expect("mint_db lock poisoned");
          list_accounts(&conn).map_err(|e| e.to_string())
      })
      .await
      .map_err(|e| format!("join error: {e}"))?
  }

  // ... mint_create_account, mint_rename_account, mint_delete_account,
  // mint_list_transactions, mint_get_transaction, mint_create_transaction,
  // mint_update_transaction, mint_delete_transaction,
  // mint_get_default_currency, mint_set_default_currency
  ```
  Use the same spawn_blocking pattern for every command. Parameter names are `snake_case` (per CLAUDE.md) — Tauri auto-converts to camelCase on the JS side.
- `src-tauri/tests/mint_integration.rs` (new) — integration tests via Tauri's mock app:
  ```rust
  use harmony_app::mint::*;
  use rusqlite::Connection;

  // Integration tests at this level exercise the sync layer through
  // public API surface; Tauri command dispatch is covered by the
  // existing test-fixtures mock-app pattern (see add_dm_ipc_handlers
  // for reference). For v1 we focus on lifecycle scenarios.

  fn fresh_in_memory_db() -> Connection {
      let conn = Connection::open_in_memory().unwrap();
      apply_migrations(&conn).unwrap();
      conn
  }

  #[test]
  fn full_lifecycle_account_plus_transactions() {
      let conn = fresh_in_memory_db();
      let a = create_account(&conn, "Chase Checking").unwrap();
      let b = create_account(&conn, "United Miles").unwrap();
      let t1 = create_transaction(&conn, NewTransaction {
          transaction_date: "2026-05-19".into(),
          amount: "-42.50".into(),
          currency: "USD".into(),
          account_id: a.id.clone(),
          description: "Coffee".into(),
          metadata: Some(r#"{"tag":"travel"}"#.into()),
      }).unwrap();
      let t2 = create_transaction(&conn, NewTransaction {
          transaction_date: "2026-05-18".into(),
          amount: "1500".into(),
          currency: "UAVF".into(),
          account_id: b.id.clone(),
          description: "Booking bonus".into(),
          metadata: None,
      }).unwrap();
      // List
      let all = list_transactions(&conn, &ListFilter { date_from: None, date_to: None, account_id: None }).unwrap();
      assert_eq!(all.len(), 2);
      assert_eq!(all[0].id, t1.id); // 2026-05-19 first (DESC)
      // Filter by account
      let only_a = list_transactions(&conn, &ListFilter { date_from: None, date_to: None, account_id: Some(a.id.clone()) }).unwrap();
      assert_eq!(only_a.len(), 1);
      assert_eq!(only_a[0].id, t1.id);
      // Update
      let updated = update_transaction(&conn, &t1.id, UpdateTransaction {
          transaction_date: None,
          amount: Some("-99.99".into()),
          currency: None,
          account_id: None,
          description: None,
          metadata: Some(None), // clear
      }).unwrap();
      assert_eq!(updated.amount, "-99.99");
      assert!(updated.metadata.is_none());
      // Delete
      delete_transaction(&conn, &t2.id).unwrap();
      let after = list_transactions(&conn, &ListFilter::default()).unwrap();
      assert_eq!(after.len(), 1);
  }

  #[test]
  fn account_delete_with_reassign_moves_transactions() {
      let conn = fresh_in_memory_db();
      let a = create_account(&conn, "A").unwrap();
      let b = create_account(&conn, "B").unwrap();
      for i in 0..3 {
          create_transaction(&conn, NewTransaction {
              transaction_date: format!("2026-05-{:02}", 10 + i),
              amount: format!("{}.00", i + 1),
              currency: "USD".into(),
              account_id: a.id.clone(),
              description: format!("Tx {i}"),
              metadata: None,
          }).unwrap();
      }
      delete_account(&conn, &a.id, Some(&b.id)).unwrap();
      let on_b = list_transactions(&conn, &ListFilter { date_from: None, date_to: None, account_id: Some(b.id.clone()) }).unwrap();
      assert_eq!(on_b.len(), 3);
      assert!(list_accounts(&conn).unwrap().iter().all(|acc| acc.id != a.id));
  }

  #[test]
  fn migration_idempotent_across_reopens() {
      // Open in-memory, migrate, close. Open again, migrate, no error.
      // Verify default_currency persists (well — it doesn't, in-memory.
      // Test with tempfile-backed DB instead for this case.)
      let tmpdir = tempfile::tempdir().unwrap();
      let path = tmpdir.path().join("ledger.db");
      {
          let conn = open_database(&path).unwrap();
          set_default_currency(&conn, "JPY").unwrap();
      }
      let conn = open_database(&path).unwrap();
      assert_eq!(get_default_currency(&conn).unwrap(), Some("JPY".into()));
  }
  ```
  - `ListFilter::default()` — derive `Default` on it.
- Need to make `mint::open_database`, `apply_migrations`, `Account`, etc. `pub` (they probably already are from Tasks 1-3, but verify).

**Constraints:**
- The lazy-init pattern in `mint_db_handle` must be safe under concurrent first-callers: hold the `NodeState` `std::sync::Mutex` while checking `is_none()`, opening the DB, and storing the Arc, so two concurrent first-callers don't open two databases. The lock is held only across the cheap clone-or-init, not across any DB operation.
- The `tauri::State<'_, std::sync::Mutex<NodeState>>` is the existing managed state — match the signature used by other mint commands. Look at `pin_content` for a pattern reference.
- ALL mint commands MUST take `app: tauri::AppHandle` as their first parameter (or wherever it fits) because `mint_db_handle` needs it to resolve `app_data_dir`.
- The first-call DB-open might fail if `app_data_dir` doesn't exist; the helper creates it via `std::fs::create_dir_all`.

**Test gates:**
- `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(mint)'` — Task 1-3 unit tests + Task 4 integration tests all green.
- `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — full workspace test, no regressions.
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- `cd src-tauri && cargo fmt --all -- --check`.

**Commit message:**
```
feat(mint): Tauri command layer with spawn_blocking + integration tests

Wires NodeState.mint_db with lazy initialization, registers all
mint_* commands in invoke_handler. Adds tests/mint_integration.rs
covering full lifecycle and account reassign scenarios.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 5 — CSV export (Rust side)

**Spec sections:** "Architecture > CSV export format", "API surface > Tauri command set" (`mint_export_csv`).

**Files:**
- `src-tauri/src/mint.rs`:
  1. Add `pub struct ExportSummary { rows_written: u64, output_path: String, byte_size: u64 }` — derive `Debug, Clone, Serialize`.
  2. Add `pub fn export_csv(conn: &Connection, output_path: &std::path::Path, date_from: Option<&str>, date_to: Option<&str>) -> Result<ExportSummary, MintError>`:
     - Validate optional `date_from` / `date_to` if provided.
     - Open the output file via `std::fs::File::create(output_path)`.
     - Wrap in `csv::WriterBuilder::new().terminator(csv::Terminator::Any(b'\n')).from_writer(file)`.
     - Write header row: `["date", "account_name", "amount", "currency", "description", "metadata"]`.
     - Execute the streaming JOIN query:
       ```sql
       SELECT t.transaction_date, a.name, t.amount, t.currency, t.description, COALESCE(t.metadata, '')
       FROM transactions t
       JOIN accounts a ON a.id = t.account_id
       WHERE (?1 IS NULL OR t.transaction_date >= ?1)
         AND (?2 IS NULL OR t.transaction_date <= ?2)
       ORDER BY t.transaction_date ASC, t.id ASC
       ```
     - For each row, `writer.write_record(&[...])`.
     - Flush, count rows, get final byte size via `std::fs::metadata(output_path)?.len()`.
     - Return `ExportSummary`.
  3. Add Tauri command `mint_export_csv` (alongside the others from Task 4):
     ```rust
     #[tauri::command]
     pub async fn mint_export_csv(
         output_path: String,
         date_from: Option<String>,
         date_to: Option<String>,
         app: tauri::AppHandle,
         state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
     ) -> Result<ExportSummary, String> {
         let conn = crate::mint_db_handle(&app, &state)?;
         tokio::task::spawn_blocking(move || {
             let conn = conn.lock().expect("mint_db lock poisoned");
             export_csv(&conn, std::path::Path::new(&output_path), date_from.as_deref(), date_to.as_deref())
                 .map_err(|e| e.to_string())
         })
         .await
         .map_err(|e| format!("join error: {e}"))?
     }
     ```
  4. Register `mint_export_csv` in `lib.rs` `invoke_handler!`.
- `src-tauri/tests/mint_integration.rs` — add tests:
  - `export_csv_round_trips_via_csv_reader`: create 5 accounts, 50 transactions, export, parse the file back with `csv::Reader`, verify row count, header, and that all 50 transactions are present.
  - `export_csv_escapes_special_characters`: create a transaction with description `Lunch, "deluxe" combo\nwith soup` and metadata `{"note":"line\nbreak"}`. Export, parse back, verify the description and metadata round-trip byte-exactly.
  - `export_csv_respects_date_filter`: 10 transactions across 10 dates. Export with `date_from = "2026-05-15"` and `date_to = "2026-05-17"`. Verify exactly 3 rows.
  - `export_csv_empty_ledger`: zero accounts, zero transactions. Export. File has only header row. `rows_written = 0`, `byte_size > 0` (header is present).

**Constraints:**
- The CSV writer streams; do NOT collect all rows into a Vec first. Use `conn.prepare(...)?.query_map(...)?` iterator pattern.
- `csv` crate handles RFC 4180 escaping automatically — no manual quoting.
- `tempfile::tempdir()` for test output paths.
- Use `csv::Terminator::Any(b'\n')` for LF line endings per spec.

**Test gates:**
- Task 4's plus the new tests.

**Commit message:**
```
feat(mint): CSV export with streaming JOIN query

Adds export_csv sync function and mint_export_csv Tauri command.
Streams rows directly into csv::Writer (no in-memory accumulation).
RFC 4180 escaping handled by csv crate.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 6 — TypeScript types + MintService + vitest

**Spec sections:** "API surface > Types" (TypeScript types), "API surface > Tauri command set" (the JS-facing surface).

**Files:**
- `src/lib/mint-types.ts` (new):
  ```typescript
  // Mirrors src-tauri/src/mint.rs types. See spec §API surface > Types.

  export interface Transaction {
    id: string;
    transactionDate: string;  // ISO 8601 'YYYY-MM-DD'
    amount: string;            // decimal string, e.g. '-42.50'
    currency: string;          // 1-5 all-caps ASCII
    accountId: string;
    accountName: string;
    description: string;
    metadata: string | null;
    createdAt: string;
    updatedAt: string;
  }

  export interface NewTransaction {
    transactionDate: string;
    amount: string;
    currency: string;
    accountId: string;
    description: string;
    metadata?: string;
  }

  export interface UpdateTransactionPayload {
    transactionDate?: string;
    amount?: string;
    currency?: string;
    accountId?: string;
    description?: string;
    metadata?: string | null;  // null = clear; absent = leave alone
  }

  export interface Account {
    id: string;
    name: string;
    createdAt: string;
    transactionCount: number;
  }

  export interface ListFilter {
    dateFrom?: string;
    dateTo?: string;
    accountId?: string;
  }

  export interface ExportSummary {
    rowsWritten: number;
    outputPath: string;
    byteSize: number;
  }
  ```
- `src/lib/mint-service.ts` (new):
  ```typescript
  import type { TauriAdapter } from './zenoh-service';
  import type {
    Transaction, NewTransaction, UpdateTransactionPayload,
    Account, ListFilter, ExportSummary
  } from './mint-types';

  export class MintService {
    constructor(private adapter: TauriAdapter) {}

    async listTransactions(filter: ListFilter = {}): Promise<Transaction[]> {
      return this.adapter.invoke('mint_list_transactions', {
        dateFrom: filter.dateFrom ?? null,
        dateTo: filter.dateTo ?? null,
        accountId: filter.accountId ?? null,
      });
    }

    async getTransaction(id: string): Promise<Transaction | null> {
      return this.adapter.invoke('mint_get_transaction', { id });
    }

    async createTransaction(payload: NewTransaction): Promise<Transaction> {
      return this.adapter.invoke('mint_create_transaction', { payload });
    }

    async updateTransaction(id: string, payload: UpdateTransactionPayload): Promise<Transaction> {
      return this.adapter.invoke('mint_update_transaction', { id, payload });
    }

    async deleteTransaction(id: string): Promise<void> {
      return this.adapter.invoke('mint_delete_transaction', { id });
    }

    async listAccounts(): Promise<Account[]> {
      return this.adapter.invoke('mint_list_accounts', {});
    }

    async createAccount(name: string): Promise<Account> {
      return this.adapter.invoke('mint_create_account', { name });
    }

    async renameAccount(id: string, name: string): Promise<Account> {
      return this.adapter.invoke('mint_rename_account', { id, name });
    }

    async deleteAccount(id: string, reassignTo: string | null = null): Promise<void> {
      return this.adapter.invoke('mint_delete_account', { id, reassignTo });
    }

    async getDefaultCurrency(): Promise<string | null> {
      return this.adapter.invoke('mint_get_default_currency', {});
    }

    async setDefaultCurrency(currency: string): Promise<void> {
      return this.adapter.invoke('mint_set_default_currency', { currency });
    }

    async exportCsv(
      outputPath: string,
      filter: { dateFrom?: string; dateTo?: string } = {}
    ): Promise<ExportSummary> {
      return this.adapter.invoke('mint_export_csv', {
        outputPath,
        dateFrom: filter.dateFrom ?? null,
        dateTo: filter.dateTo ?? null,
      });
    }
  }
  ```
- `src/lib/mint-service.test.ts` (new):
  ```typescript
  import { describe, it, expect, vi } from 'vitest';
  import { MintService } from './mint-service';
  import type { TauriAdapter } from './zenoh-service';

  function mockAdapter(): TauriAdapter & { invoke: ReturnType<typeof vi.fn> } {
    return { invoke: vi.fn().mockResolvedValue(undefined) } as any;
  }

  describe('MintService', () => {
    it('listTransactions converts filter to camelCase JSON nulls', async () => {
      const a = mockAdapter();
      a.invoke.mockResolvedValueOnce([]);
      const svc = new MintService(a);
      await svc.listTransactions({ dateFrom: '2026-01-01' });
      expect(a.invoke).toHaveBeenCalledWith('mint_list_transactions', {
        dateFrom: '2026-01-01',
        dateTo: null,
        accountId: null,
      });
    });

    it('createTransaction wraps payload in { payload }', async () => {
      const a = mockAdapter();
      a.invoke.mockResolvedValueOnce({});
      const svc = new MintService(a);
      await svc.createTransaction({
        transactionDate: '2026-05-19',
        amount: '-42.50',
        currency: 'USD',
        accountId: 'abc',
        description: 'Coffee',
      });
      expect(a.invoke).toHaveBeenCalledWith('mint_create_transaction', {
        payload: {
          transactionDate: '2026-05-19',
          amount: '-42.50',
          currency: 'USD',
          accountId: 'abc',
          description: 'Coffee',
        },
      });
    });

    it('updateTransaction with metadata: null clears the field', async () => {
      const a = mockAdapter();
      a.invoke.mockResolvedValueOnce({});
      const svc = new MintService(a);
      await svc.updateTransaction('tx-id', { metadata: null });
      expect(a.invoke).toHaveBeenCalledWith('mint_update_transaction', {
        id: 'tx-id',
        payload: { metadata: null },
      });
    });

    it('deleteAccount defaults reassignTo to null', async () => {
      const a = mockAdapter();
      const svc = new MintService(a);
      await svc.deleteAccount('acc-id');
      expect(a.invoke).toHaveBeenCalledWith('mint_delete_account', {
        id: 'acc-id',
        reassignTo: null,
      });
    });

    it('exportCsv passes outputPath verbatim', async () => {
      const a = mockAdapter();
      a.invoke.mockResolvedValueOnce({ rowsWritten: 0, outputPath: '/tmp/o.csv', byteSize: 50 });
      const svc = new MintService(a);
      const r = await svc.exportCsv('/tmp/o.csv');
      expect(r.outputPath).toBe('/tmp/o.csv');
      expect(a.invoke).toHaveBeenCalledWith('mint_export_csv', {
        outputPath: '/tmp/o.csv',
        dateFrom: null,
        dateTo: null,
      });
    });
  });
  ```

**Constraints:**
- All JS parameter names are camelCase per CLAUDE.md — Tauri auto-converts to snake_case Rust side.
- `TauriAdapter` interface is already defined in `src/lib/zenoh-service.ts` (used by every other `*-service.ts`).
- Optional fields → explicit `null` in invoke payload (not `undefined`) so they serialize cleanly across IPC.

**Test gates:**
- `npx tsc --noEmit` from repo root — no errors.
- `npx vitest run src/lib/mint-service.test.ts` — all tests green.

**Commit message:**
```
feat(mint): TypeScript types + MintService

Mirrors Rust types in camelCase; wraps all mint_* Tauri commands.
Vitest coverage of payload shape (camelCase, null vs undefined,
metadata clear semantics).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 7 — MintLedger panel + transaction table

**Spec sections:** "Architecture > UI flow" (MintLedger layout).

**Files:**
- `src/lib/components/MintLedger.svelte` (new) — top-level panel. Layout:
  ```svelte
  <script lang="ts">
    import type { TauriAdapter } from '$lib/zenoh-service';
    import { MintService } from '$lib/mint-service';
    import type { Transaction, Account } from '$lib/mint-types';
    import MintTransactionTable from './MintTransactionTable.svelte';

    let { adapter }: { adapter: TauriAdapter } = $props();

    const service = new MintService(adapter);
    let transactions = $state<Transaction[]>([]);
    let accounts = $state<Account[]>([]);
    let defaultCurrency = $state<string>('USD');
    let loading = $state(true);
    let error = $state<string | null>(null);

    // Filters
    let filterDateFrom = $state<string>('');
    let filterDateTo = $state<string>('');
    let filterAccountId = $state<string>('');

    async function load() {
      loading = true;
      error = null;
      try {
        const [txs, accs, def] = await Promise.all([
          service.listTransactions({
            dateFrom: filterDateFrom || undefined,
            dateTo: filterDateTo || undefined,
            accountId: filterAccountId || undefined,
          }),
          service.listAccounts(),
          service.getDefaultCurrency(),
        ]);
        transactions = txs;
        accounts = accs;
        defaultCurrency = def ?? 'USD';
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      } finally {
        loading = false;
      }
    }

    $effect(() => { load(); });

    // Add/Edit dialog state — wired in Task 8
    let showAddEdit = $state(false);
    let editingTxId = $state<string | null>(null);

    // Account manager state — wired in Task 9
    let showAccountManager = $state(false);

    // CSV export state — wired in Task 10
    let exportInProgress = $state(false);
  </script>

  <section aria-label="Mint personal finance ledger" class="mint-ledger">
    <header class="mint-toolbar">
      <div class="filters">
        <label>From <input type="date" bind:value={filterDateFrom} onchange={load} /></label>
        <label>To <input type="date" bind:value={filterDateTo} onchange={load} /></label>
        <label>
          Account
          <select bind:value={filterAccountId} onchange={load}>
            <option value="">All</option>
            {#each accounts as a}<option value={a.id}>{a.name}</option>{/each}
          </select>
        </label>
        <span class="default-currency">Default: {defaultCurrency}</span>
      </div>
      <div class="actions">
        <button onclick={() => { editingTxId = null; showAddEdit = true; }}>+ Add Transaction</button>
        <button onclick={() => { showAccountManager = true; }}>Manage Accounts</button>
        <button onclick={() => { /* Task 10 */ }} disabled={exportInProgress}>Export CSV</button>
      </div>
    </header>

    {#if loading}
      <p>Loading…</p>
    {:else if error}
      <p role="alert" class="error">{error}</p>
    {:else}
      <MintTransactionTable
        {transactions}
        onEdit={(id) => { editingTxId = id; showAddEdit = true; }}
        onDelete={async (id) => {
          if (!confirm('Delete this transaction?')) return;
          await service.deleteTransaction(id);
          await load();
        }}
      />
    {/if}

    <!-- Add/Edit dialog: rendered in Task 8 -->
    <!-- Account manager dialog: rendered in Task 9 -->
  </section>

  <style>
    .mint-ledger { display: flex; flex-direction: column; height: 100%; padding: 1rem; }
    .mint-toolbar { display: flex; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: 1rem; }
    .filters { display: flex; gap: 0.75rem; align-items: center; flex-wrap: wrap; }
    .default-currency { color: var(--color-text-secondary, #888); font-size: 0.9rem; }
    .actions { display: flex; gap: 0.5rem; }
    .error { color: var(--color-error, #c53030); }
  </style>
  ```
- `src/lib/components/MintTransactionTable.svelte` (new):
  ```svelte
  <script lang="ts">
    import type { Transaction } from '$lib/mint-types';

    let { transactions, onEdit, onDelete }: {
      transactions: Transaction[];
      onEdit: (id: string) => void;
      onDelete: (id: string) => void;
    } = $props();
  </script>

  <table aria-label="Transactions" class="mint-tx-table">
    <thead>
      <tr>
        <th>Date</th>
        <th>Account</th>
        <th>Amount</th>
        <th>Currency</th>
        <th>Description</th>
        <th>Metadata</th>
        <th aria-label="Actions"></th>
      </tr>
    </thead>
    <tbody>
      {#each transactions as tx (tx.id)}
        <tr>
          <td>{tx.transactionDate}</td>
          <td>{tx.accountName}</td>
          <td class="amount">{tx.amount}</td>
          <td>{tx.currency}</td>
          <td>{tx.description}</td>
          <td class="metadata">
            {#if tx.metadata}<code title={tx.metadata}>{tx.metadata.slice(0, 40)}{tx.metadata.length > 40 ? '…' : ''}</code>{/if}
          </td>
          <td>
            <button onclick={() => onEdit(tx.id)} aria-label="Edit transaction">Edit</button>
            <button onclick={() => onDelete(tx.id)} aria-label="Delete transaction">Delete</button>
          </td>
        </tr>
      {:else}
        <tr><td colspan="7" class="empty">No transactions yet.</td></tr>
      {/each}
    </tbody>
  </table>

  <style>
    .mint-tx-table { width: 100%; border-collapse: collapse; }
    .mint-tx-table th, .mint-tx-table td { padding: 0.4rem 0.6rem; text-align: left; border-bottom: 1px solid var(--color-border, #eee); }
    .amount { font-variant-numeric: tabular-nums; text-align: right; }
    .metadata code { font-size: 0.85em; }
    .empty { text-align: center; color: var(--color-text-secondary, #888); padding: 2rem; }
  </style>
  ```
- `src/lib/components/__tests__/MintTransactionTable.test.ts` (new):
  ```typescript
  import { describe, it, expect } from 'vitest';
  import { render, fireEvent, screen } from '@testing-library/svelte';
  import MintTransactionTable from '../MintTransactionTable.svelte';

  const sample = (over: any = {}) => ({
    id: 't1',
    transactionDate: '2026-05-19',
    amount: '-42.50',
    currency: 'USD',
    accountId: 'a1',
    accountName: 'Chase',
    description: 'Coffee',
    metadata: null,
    createdAt: '2026-05-19T10:00:00Z',
    updatedAt: '2026-05-19T10:00:00Z',
    ...over,
  });

  describe('MintTransactionTable', () => {
    it('renders one row per transaction', () => {
      render(MintTransactionTable, { transactions: [sample(), sample({ id: 't2' })], onEdit: () => {}, onDelete: () => {} });
      expect(screen.getAllByRole('row')).toHaveLength(3); // header + 2
    });

    it('shows empty state when no transactions', () => {
      render(MintTransactionTable, { transactions: [], onEdit: () => {}, onDelete: () => {} });
      expect(screen.getByText(/No transactions yet/)).toBeInTheDocument();
    });

    it('truncates metadata over 40 characters with ellipsis', () => {
      const longMeta = '{"description":"' + 'x'.repeat(100) + '"}';
      render(MintTransactionTable, { transactions: [sample({ metadata: longMeta })], onEdit: () => {}, onDelete: () => {} });
      const code = screen.getByTitle(longMeta);
      expect(code.textContent).toContain('…');
      expect(code.textContent!.length).toBeLessThanOrEqual(41);
    });

    it('calls onEdit when Edit button clicked', async () => {
      let edited: string | null = null;
      render(MintTransactionTable, {
        transactions: [sample({ id: 'tx-7' })],
        onEdit: (id) => { edited = id; },
        onDelete: () => {},
      });
      await fireEvent.click(screen.getByLabelText('Edit transaction'));
      expect(edited).toBe('tx-7');
    });

    it('calls onDelete when Delete button clicked', async () => {
      let deleted: string | null = null;
      render(MintTransactionTable, {
        transactions: [sample({ id: 'tx-99' })],
        onEdit: () => {},
        onDelete: (id) => { deleted = id; },
      });
      await fireEvent.click(screen.getByLabelText('Delete transaction'));
      expect(deleted).toBe('tx-99');
    });
  });
  ```

**Constraints:**
- Svelte 5 runes syntax: `$state`, `$props`, `$effect`, `$derived`. NOT Svelte 4 stores.
- All component event props use the `on<EventName>: (...) => void` callback pattern (match existing components like `FileBrowser.svelte` which we read earlier).
- Use the existing `TauriAdapter` interface from `zenoh-service.ts`.
- Accessibility: `aria-label` on the section, `aria-label` on the table, `aria-label="Edit transaction"` etc. on buttons. (The team's `__tests__/AriaAnnouncer.test.ts` pattern suggests accessibility is taken seriously.)
- DO NOT wire AppMode yet — Task 10 handles that. For now, the component just needs to render correctly in isolation.

**Test gates:**
- `npx tsc --noEmit` — clean.
- `npx vitest run src/lib/components/__tests__/MintTransactionTable.test.ts` — green.
- `npx vitest run` (full vitest run) — no regressions.

**Commit message:**
```
feat(mint): MintLedger panel + MintTransactionTable component

Top-level Svelte 5 panel with filters, action buttons, and an
inline transaction table. Dialogs are stubbed (state declared but
no dialog component rendered yet) — Tasks 8 and 9 wire them up.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 8 — Transaction add/edit dialog

**Spec sections:** "Architecture > UI flow" (Add/Edit dialog).

**Files:**
- `src/lib/components/MintTransactionDialog.svelte` (new):
  ```svelte
  <script lang="ts">
    import type { MintService } from '$lib/mint-service';
    import type { Transaction, Account, NewTransaction, UpdateTransactionPayload } from '$lib/mint-types';

    let {
      service,
      accounts,
      defaultCurrency,
      editingId,
      onClose,
      onSaved,
    }: {
      service: MintService;
      accounts: Account[];
      defaultCurrency: string;
      editingId: string | null;
      onClose: () => void;
      onSaved: () => void;
    } = $props();

    let date = $state(new Date().toISOString().slice(0, 10));
    let amount = $state('');
    let currency = $state(defaultCurrency);
    let accountId = $state(accounts[0]?.id ?? '');
    let description = $state('');
    let metadata = $state('');
    let error = $state<string | null>(null);
    let saving = $state(false);

    // Load existing transaction when in edit mode
    $effect(() => {
      if (editingId) {
        service.getTransaction(editingId).then((tx) => {
          if (!tx) return;
          date = tx.transactionDate;
          amount = tx.amount;
          currency = tx.currency;
          accountId = tx.accountId;
          description = tx.description;
          metadata = tx.metadata ?? '';
        });
      }
    });

    // Client-side validation (server is authoritative — these just enable Save)
    let isValid = $derived(
      /^\d{4}-\d{2}-\d{2}$/.test(date) &&
      /^-?\d+(\.\d+)?$/.test(amount) &&
      /^[A-Z]{1,5}$/.test(currency) &&
      accountId !== '' &&
      description.trim().length > 0 &&
      description.length <= 4096 &&
      (metadata === '' || isJsonValid(metadata))
    );

    function isJsonValid(s: string): boolean {
      try { JSON.parse(s); return true; } catch { return false; }
    }

    async function save() {
      saving = true;
      error = null;
      try {
        if (editingId) {
          const payload: UpdateTransactionPayload = {
            transactionDate: date,
            amount,
            currency,
            accountId,
            description,
            metadata: metadata === '' ? null : metadata,
          };
          await service.updateTransaction(editingId, payload);
        } else {
          const payload: NewTransaction = {
            transactionDate: date,
            amount,
            currency,
            accountId,
            description,
            metadata: metadata === '' ? undefined : metadata,
          };
          await service.createTransaction(payload);
        }
        onSaved();
        onClose();
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      } finally {
        saving = false;
      }
    }
  </script>

  <div role="dialog" aria-modal="true" aria-label={editingId ? 'Edit transaction' : 'Add transaction'} class="mint-dialog">
    <div class="dialog-body">
      <h2>{editingId ? 'Edit transaction' : 'Add transaction'}</h2>
      <label>Date <input type="date" bind:value={date} /></label>
      <label>Amount <input type="text" inputmode="decimal" bind:value={amount} placeholder="-42.50" /></label>
      <label>Currency <input type="text" bind:value={currency} maxlength="5" /></label>
      <label>
        Account
        <select bind:value={accountId}>
          {#each accounts as a}<option value={a.id}>{a.name}</option>{/each}
        </select>
      </label>
      <label>Description <textarea bind:value={description} rows="2"></textarea></label>
      <label>Metadata (JSON, optional) <textarea bind:value={metadata} rows="4" placeholder='{"tag":"travel"}'></textarea></label>
      {#if error}<p role="alert" class="error">{error}</p>{/if}
      <div class="dialog-actions">
        <button onclick={onClose} disabled={saving}>Cancel</button>
        <button onclick={save} disabled={!isValid || saving}>{saving ? 'Saving…' : 'Save'}</button>
      </div>
    </div>
  </div>

  <style>
    .mint-dialog { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 1000; }
    .dialog-body { background: var(--color-bg, #fff); padding: 1.5rem; border-radius: 8px; min-width: 400px; max-width: 90vw; display: flex; flex-direction: column; gap: 0.75rem; }
    .dialog-body label { display: flex; flex-direction: column; gap: 0.25rem; }
    .dialog-actions { display: flex; justify-content: flex-end; gap: 0.5rem; margin-top: 0.5rem; }
    .error { color: var(--color-error, #c53030); }
  </style>
  ```
- `src/lib/components/MintLedger.svelte` — render the dialog when `showAddEdit` is true:
  Add inside the `<section>`, after the table:
  ```svelte
  {#if showAddEdit}
    <MintTransactionDialog
      {service}
      {accounts}
      {defaultCurrency}
      editingId={editingTxId}
      onClose={() => { showAddEdit = false; editingTxId = null; }}
      onSaved={load}
    />
  {/if}
  ```
  And add `import MintTransactionDialog from './MintTransactionDialog.svelte';` at top.
- `src/lib/components/__tests__/MintTransactionDialog.test.ts` (new):
  ```typescript
  import { describe, it, expect, vi } from 'vitest';
  import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';
  import MintTransactionDialog from '../MintTransactionDialog.svelte';

  function mockService() {
    return {
      getTransaction: vi.fn(),
      createTransaction: vi.fn().mockResolvedValue({}),
      updateTransaction: vi.fn().mockResolvedValue({}),
    } as any;
  }

  describe('MintTransactionDialog', () => {
    it('disables Save when amount is invalid', async () => {
      render(MintTransactionDialog, {
        service: mockService(),
        accounts: [{ id: 'a1', name: 'Chase', createdAt: '', transactionCount: 0 }],
        defaultCurrency: 'USD',
        editingId: null,
        onClose: () => {},
        onSaved: () => {},
      });
      const save = screen.getByRole('button', { name: 'Save' });
      expect(save).toBeDisabled();
      const amount = screen.getByLabelText(/Amount/);
      await fireEvent.input(amount, { target: { value: 'not a number' } });
      const desc = screen.getByLabelText(/Description/);
      await fireEvent.input(desc, { target: { value: 'Coffee' } });
      expect(save).toBeDisabled();
      await fireEvent.input(amount, { target: { value: '-42.50' } });
      expect(save).not.toBeDisabled();
    });

    it('disables Save when description is empty', async () => { /* similar pattern */ });

    it('disables Save when metadata is invalid JSON', async () => { /* fill in all valid then set metadata to "{" */ });

    it('calls createTransaction with form data when not editing', async () => {
      const service = mockService();
      const onSaved = vi.fn();
      render(MintTransactionDialog, {
        service,
        accounts: [{ id: 'a1', name: 'Chase', createdAt: '', transactionCount: 0 }],
        defaultCurrency: 'USD',
        editingId: null,
        onClose: () => {},
        onSaved,
      });
      await fireEvent.input(screen.getByLabelText(/Amount/), { target: { value: '12.00' } });
      await fireEvent.input(screen.getByLabelText(/Description/), { target: { value: 'Lunch' } });
      await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(service.createTransaction).toHaveBeenCalled());
      const call = service.createTransaction.mock.calls[0][0];
      expect(call.amount).toBe('12.00');
      expect(call.description).toBe('Lunch');
      expect(call.metadata).toBeUndefined();
      expect(onSaved).toHaveBeenCalled();
    });

    it('calls updateTransaction with metadata: null when metadata field cleared in edit mode', async () => {
      const service = mockService();
      service.getTransaction.mockResolvedValueOnce({
        id: 'tx-1', transactionDate: '2026-05-19', amount: '10.00', currency: 'USD',
        accountId: 'a1', accountName: 'Chase', description: 'Old', metadata: '{"tag":"x"}',
        createdAt: '', updatedAt: '',
      });
      render(MintTransactionDialog, {
        service,
        accounts: [{ id: 'a1', name: 'Chase', createdAt: '', transactionCount: 0 }],
        defaultCurrency: 'USD',
        editingId: 'tx-1',
        onClose: () => {},
        onSaved: () => {},
      });
      await waitFor(() => expect((screen.getByLabelText(/Metadata/) as HTMLTextAreaElement).value).toBe('{"tag":"x"}'));
      await fireEvent.input(screen.getByLabelText(/Metadata/), { target: { value: '' } });
      await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(service.updateTransaction).toHaveBeenCalled());
      const [, payload] = service.updateTransaction.mock.calls[0];
      expect(payload.metadata).toBeNull();
    });
  });
  ```

**Constraints:**
- The metadata field in create mode uses `undefined` (omitted from payload); in edit mode `null` (explicit clear) per spec semantics.
- Server-side validation is authoritative; client validation just controls the disabled state of the Save button. Don't duplicate server validation logic 1:1 — keep client validation conservative (might block some inputs server would accept; that's fine since the form's intent is unambiguous).

**Test gates:**
- `npx tsc --noEmit` — clean.
- `npx vitest run src/lib/components/__tests__/MintTransactionDialog.test.ts` — green.
- Full `npx vitest run` — no regressions.

**Commit message:**
```
feat(mint): MintTransactionDialog for add/edit with client-side validation

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 9 — Account manager dialog

**Spec sections:** "Architecture > UI flow" (Manage Accounts dialog).

**Files:**
- `src/lib/components/MintAccountManager.svelte` (new):
  ```svelte
  <script lang="ts">
    import type { MintService } from '$lib/mint-service';
    import type { Account } from '$lib/mint-types';

    let { service, accounts, onClose, onChanged }: {
      service: MintService;
      accounts: Account[];
      onClose: () => void;
      onChanged: () => void;
    } = $props();

    let newName = $state('');
    let creating = $state(false);
    let error = $state<string | null>(null);

    // Per-account rename state, keyed by id
    let editingId = $state<string | null>(null);
    let editingName = $state('');

    // Per-account delete confirm state
    let confirmDeleteId = $state<string | null>(null);
    let reassignTo = $state<string>(''); // '' = no reassign

    async function create() {
      if (!newName.trim()) return;
      creating = true;
      error = null;
      try {
        await service.createAccount(newName.trim());
        newName = '';
        onChanged();
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      } finally {
        creating = false;
      }
    }

    function startRename(a: Account) {
      editingId = a.id;
      editingName = a.name;
    }

    async function commitRename() {
      if (!editingId) return;
      error = null;
      try {
        await service.renameAccount(editingId, editingName.trim());
        editingId = null;
        onChanged();
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
    }

    function startDelete(a: Account) {
      confirmDeleteId = a.id;
      reassignTo = '';
    }

    async function commitDelete() {
      if (!confirmDeleteId) return;
      error = null;
      try {
        await service.deleteAccount(confirmDeleteId, reassignTo || null);
        confirmDeleteId = null;
        onChanged();
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
    }
  </script>

  <div role="dialog" aria-modal="true" aria-label="Manage accounts" class="mint-dialog">
    <div class="dialog-body">
      <h2>Manage accounts</h2>
      <div class="new-account">
        <input
          type="text"
          bind:value={newName}
          placeholder="New account name"
          maxlength="256"
          onkeydown={(e) => { if (e.key === 'Enter') create(); }}
        />
        <button onclick={create} disabled={!newName.trim() || creating}>Add</button>
      </div>
      <ul class="accounts-list">
        {#each accounts as a (a.id)}
          <li>
            {#if editingId === a.id}
              <input type="text" bind:value={editingName} />
              <button onclick={commitRename}>Save</button>
              <button onclick={() => { editingId = null; }}>Cancel</button>
            {:else}
              <span class="name">{a.name}</span>
              <span class="count">({a.transactionCount} txn{a.transactionCount === 1 ? '' : 's'})</span>
              <button onclick={() => startRename(a)} aria-label="Rename {a.name}">Rename</button>
              <button onclick={() => startDelete(a)} aria-label="Delete {a.name}">Delete</button>
            {/if}
          </li>
        {/each}
      </ul>
      {#if confirmDeleteId}
        <div class="confirm-delete">
          <p>
            Delete account "{accounts.find((a) => a.id === confirmDeleteId)?.name}"?
            {@const cnt = accounts.find((a) => a.id === confirmDeleteId)?.transactionCount ?? 0}
            {#if cnt > 0}
              <br />Reassign {cnt} transaction{cnt === 1 ? '' : 's'} to:
              <select bind:value={reassignTo}>
                <option value="">— select account —</option>
                {#each accounts.filter((a) => a.id !== confirmDeleteId) as opt}
                  <option value={opt.id}>{opt.name}</option>
                {/each}
              </select>
            {/if}
          </p>
          <button onclick={commitDelete} disabled={(accounts.find((a) => a.id === confirmDeleteId)?.transactionCount ?? 0) > 0 && !reassignTo}>Confirm Delete</button>
          <button onclick={() => { confirmDeleteId = null; }}>Cancel</button>
        </div>
      {/if}
      {#if error}<p role="alert" class="error">{error}</p>{/if}
      <div class="dialog-actions">
        <button onclick={onClose}>Close</button>
      </div>
    </div>
  </div>

  <style>
    .mint-dialog { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 1000; }
    .dialog-body { background: var(--color-bg, #fff); padding: 1.5rem; border-radius: 8px; min-width: 480px; max-width: 90vw; display: flex; flex-direction: column; gap: 0.75rem; }
    .new-account { display: flex; gap: 0.5rem; }
    .new-account input { flex: 1; }
    .accounts-list { list-style: none; padding: 0; margin: 0; }
    .accounts-list li { display: flex; align-items: center; gap: 0.5rem; padding: 0.4rem 0; border-bottom: 1px solid var(--color-border, #eee); }
    .name { flex: 1; }
    .count { color: var(--color-text-secondary, #888); font-size: 0.85em; }
    .confirm-delete { padding: 0.75rem; background: var(--color-bg-warning, #fff8e1); border-radius: 4px; }
    .dialog-actions { display: flex; justify-content: flex-end; }
    .error { color: var(--color-error, #c53030); }
  </style>
  ```
- `src/lib/components/MintLedger.svelte` — render the account manager:
  ```svelte
  {#if showAccountManager}
    <MintAccountManager
      {service}
      {accounts}
      onClose={() => { showAccountManager = false; }}
      onChanged={load}
    />
  {/if}
  ```
  And add `import MintAccountManager from './MintAccountManager.svelte';` at top.
- `src/lib/components/__tests__/MintAccountManager.test.ts` (new): tests for:
  - Add button disabled until name typed
  - Create account invokes service.createAccount with trimmed name
  - Rename starts in-place edit, Save commits
  - Delete with 0 transactions: confirm button enabled
  - Delete with N transactions: confirm button disabled until reassign-to selected
  - Delete commits with selected reassignTo when set, with null when 0 transactions

**Constraints:**
- The `accounts.filter` in the reassign dropdown excludes the account being deleted (you can't reassign to yourself).
- The "0 transactions, no reassign needed" case: `reassignTo = ''` → service called with `null`.
- The "N transactions, must reassign" case: button disabled until `reassignTo !== ''`.

**Test gates:**
- `npx tsc --noEmit` — clean.
- `npx vitest run src/lib/components/__tests__/MintAccountManager.test.ts` — green.
- Full `npx vitest run` — no regressions.

**Commit message:**
```
feat(mint): MintAccountManager dialog with rename and reassign-on-delete

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Task 10 — AppMode wiring + CSV export UI flow + smoke test

**Spec sections:** "Architecture > Module placement" (AppMode wiring), "Architecture > UI flow" (Export CSV button).

**Files:**
- `src/lib/types.ts:153` — extend the AppMode union:
  ```typescript
  export type AppMode = 'messages' | 'vines' | 'files' | 'spellbook' | 'mail' | 'mint';
  ```
- `src/App.svelte` — wire MintLedger into the app:
  1. Import `MintLedger`:
     ```typescript
     import MintLedger from './lib/components/MintLedger.svelte';
     ```
  2. Render conditionally based on `appMode`. Locate the existing pattern (look for `{#if appMode === 'mail'}` or similar) and add a parallel `{#if appMode === 'mint'}` block:
     ```svelte
     {#if appMode === 'mint'}
       <MintLedger adapter={tauriAdapter} />
     {/if}
     ```
  3. Add a navigation entry. Search NavPanel.svelte or the App.svelte mode-switcher for how other modes (e.g. `'mail'`) get a nav entry. Add a 'mint' entry with a coin-or-dollar icon (use an emoji 💰 if the icon set doesn't already have a coin icon — match the existing approach for `'spellbook'` and `'vines'` which use emoji).
- `src/lib/components/MintLedger.svelte` — wire the Export CSV button:
  1. Add import:
     ```typescript
     import { save as saveDialog } from '@tauri-apps/plugin-dialog';
     ```
  2. Replace the placeholder onclick for the Export CSV button:
     ```svelte
     <button onclick={exportCsv} disabled={exportInProgress}>Export CSV</button>
     ```
  3. Add the function:
     ```typescript
     async function exportCsv() {
       const path = await saveDialog({
         defaultPath: `mint-export-${new Date().toISOString().slice(0, 10)}.csv`,
         filters: [{ name: 'CSV', extensions: ['csv'] }],
       });
       if (!path) return; // user cancelled
       exportInProgress = true;
       error = null;
       try {
         const summary = await service.exportCsv(path, {
           dateFrom: filterDateFrom || undefined,
           dateTo: filterDateTo || undefined,
         });
         alert(`Exported ${summary.rowsWritten} transactions to ${summary.outputPath} (${summary.byteSize} bytes)`);
       } catch (e) {
         error = e instanceof Error ? e.message : String(e);
       } finally {
         exportInProgress = false;
       }
     }
     ```
- `src-tauri/Cargo.toml` — `tauri-plugin-dialog = "2"` is already listed; no change needed.
- `src-tauri/src/lib.rs` — verify `tauri_plugin_dialog::init()` is registered in the Tauri builder (it should already be, per the existing save dialog usage in `save_dialog.rs`).
- Smoke test manifest (NOT automated, lives in the PR description). After all code changes, the implementer should manually:
  1. `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — full Rust test pass
  2. `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  3. `cd src-tauri && cargo fmt --all -- --check`
  4. `npx tsc --noEmit` from repo root
  5. `npx vitest run`
  6. **Manual Windows smoke test** (the implementer subagent will skip this — Jake performs after merge):
     - `npm run tauri dev`
     - Open app, click 'mint' in nav
     - Create accounts: "Chase Checking", "United Miles"
     - Set default currency to USD (via Settings... actually, there's no UI for setting default currency in v1; that's a gap — default stays "USD" unless set via Tauri command directly. Note this in the PR description as a known followup.)
     - Add 5 transactions, including one with JPY currency and one with metadata JSON
     - Edit one transaction (change amount)
     - Delete one
     - Filter by date range
     - Filter by account
     - Export CSV to Desktop, open in Excel, verify
- `App.svelte` — final integration verification: the import resolves, the appMode union accepts 'mint', the panel renders.

**Constraints:**
- `tauri-plugin-dialog` is already a runtime dependency (it's used by save_dialog.rs); the plugin must be initialized in `tauri::Builder` before the panel can call `saveDialog`. Verify by searching `lib.rs` for `tauri_plugin_dialog`.
- The smoke-test items 1-5 above are blocking — the implementer subagent runs them all and fixes any failures before declaring DONE.
- Item 6 (manual UI test) is for Jake post-merge; the subagent does NOT block on this.
- No new dependencies in this task — everything was added in earlier tasks.

**Test gates:**
- All previous task gates, plus:
- `npx tsc --noEmit` — passes with the new AppMode value and the App.svelte changes.
- `npx vitest run` — full suite green.

**Commit message:**
```
feat(mint): AppMode wiring + CSV export UI + nav entry

Adds 'mint' to AppMode, wires MintLedger into App.svelte, and
implements the Export CSV button using tauri-plugin-dialog's
save dialog.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Post-task gates (before opening PR)

After Task 10 lands, the controller (not a subagent) runs these from the repo root:

```powershell
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
cargo nextest run --locked --workspace --all-targets --features test-fixtures   # may hit Windows DLL quirk per CLAUDE.md — Linux CI authoritative
cd ..
npx tsc --noEmit
npx vitest run
```

All green except the Windows-local nextest run, which may surface the known DLL `STATUS_ENTRYPOINT_NOT_FOUND` quirk per CLAUDE.md. If only that quirk fails, proceed — Linux CI is authoritative.

## PR description checklist

When opening the PR:

- Reference the spec (`docs/specs/2026-05-19-mint-mvp-design.md`) and plan (`docs/plans/2026-05-19-mint-mvp-plan.md`).
- Summarize: 10 commits, one per task. ~2000 lines added (Rust + TS + Svelte).
- Test coverage: ~50 Rust unit + integration tests, ~15 TypeScript/Svelte tests.
- Note Phase 2 follow-ups: CAS sync, analytical views, bank CSV ingest. Each becomes its own ZEB ticket.
- Note the known gap: default-currency setting has no UI in v1 (only the Tauri command exists). Add a Linear ticket for "Settings UI" if Jake wants v1.x polish.
- Note that local Windows nextest run may surface the DLL quirk per CLAUDE.md.

## Out-of-scope follow-ups (mention in PR body)

- **CAS sync** (Phase 2 — separate ZEB ticket): debounced full-DB snapshot to CAS on writes, latest-snapshot CID tracked via app settings, peer pull-on-start, LWW per whole-DB with conflict warning.
- **Default-currency settings UI** (v1.x polish ticket): currently only changeable via Tauri command; needs a settings panel or inline control.
- **Analytical view** (Phase 3): charts, monthly aggregates, category roll-ups (would promote tags out of metadata JSON).
- **Bank CSV ingest** (Phase 3): mapping UI + dedup by hash.
- **Soft delete / undo** (v1.x polish): adds `deleted_at` column, trash view.
- **Tags as first-class** (Phase 3): promote out of metadata JSON once usage patterns stabilize.
