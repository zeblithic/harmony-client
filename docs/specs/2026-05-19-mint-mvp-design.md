# Mint — Personal Finance Transaction Tracker (MVP)

**Date:** 2026-05-19
**Status:** Design approved, plan pending
**Author:** Jake Englund (via brainstorming session with Claude)

## Goal

Add a simple personal-finance transaction tracker to harmony-client. The user can record individual transactions (date, amount, currency, account, description, optional JSON metadata), browse them, edit/delete past entries, and export the full ledger to CSV for analysis in Excel or any other tool.

This is the foundation for a future "harmony-mint" feature surface that may grow analytical/budgeting capabilities, bank-CSV ingest, and CAS-backed multi-device sync. For now: a local, single-user, full-CRUD transaction ledger with CSV export.

## Non-goals (v1)

These are explicitly out of scope and either deferred to follow-ups or marked as never-going-to-build:

- **Multi-device sync via CAS.** Deferred to Phase 2 — the SQLite file lives only on the local machine in v1. The user can manually copy the file between devices if they wish.
- **Analytical features** (charts, category roll-ups, budgets, savings goals).
- **Bank CSV / Plaid ingest.** Manual entry only.
- **Multi-currency FX conversion.** Each transaction has its own currency string; we never convert between them.
- **Recurring transactions.**
- **First-class tags / categories.** Users can put tags in the `metadata` JSON field.
- **Soft-delete / undo.** Delete is hard delete.
- **Real currency validation against ISO 4217.** We accept any 1–5 character all-caps ASCII string.
- **Concurrent edits / multi-user.** Single-writer assumption.

## Context

Harmony-client is a Svelte 5 + Tauri 2 desktop app structured as a single-page app where `App.svelte` orchestrates feature panels via an `AppMode` enum. Each feature follows the pattern: one Tauri Rust module per backend concern + one TypeScript service file per frontend concern + one or more Svelte components.

Existing relevant infrastructure:

- **App data directory** is platform-specific via `tauri::api::path::app_data_dir()`. We can colocate new app files (e.g. SQLite) there.
- **Tauri IPC** uses snake_case Rust parameters and camelCase JS callers (CLAUDE.md doctrine).
- **CAS / file manager** infrastructure (folder ingest, pin cascade, content-addressed storage) is mature and recently shipped. It is **not** wired into mint v1 — Phase 2 will integrate.

## Architecture

### Module placement

```
harmony-client/
├── src-tauri/src/
│   └── mint.rs                     ← rusqlite layer + Tauri commands
├── src/lib/
│   ├── mint-service.ts             ← TS service wrapping Tauri invocations
│   ├── mint-types.ts               ← shared types (Transaction, Account, ...)
│   └── components/
│       ├── MintLedger.svelte       ← top-level panel
│       ├── MintTransactionTable.svelte
│       ├── MintTransactionDialog.svelte   ← add + edit
│       └── MintAccountManager.svelte
└── App.svelte                      ← add 'mint' to AppMode
```

SQLite database file: `<app_data_dir>/mint/ledger.db`. Created on first open if the parent directory does not exist; schema migrated idempotently on every open via `CREATE TABLE IF NOT EXISTS`.

### Database connection lifecycle

A single `rusqlite::Connection` per Tauri app instance, wrapped in `Arc<std::sync::Mutex<Connection>>` and held inside `NodeState`. All SQLite operations run inside `tokio::task::spawn_blocking` — rusqlite is synchronous and blocking, and holding it directly in async context would block the tokio executor on every query. The standard (non-async) Mutex inside `spawn_blocking` is correct because we never hold the lock across an `.await`.

```rust
// Pattern used in every Tauri command handler:
let conn = state.mint_db.clone();   // Arc<Mutex<Connection>>
tokio::task::spawn_blocking(move || {
    let mut conn = conn.lock().expect("mint_db lock poisoned");
    // ... rusqlite operations
})
.await
.map_err(|e| e.to_string())?
```

WAL mode is enabled at connection open (`PRAGMA journal_mode = WAL`) for crash safety. `PRAGMA foreign_keys = ON` enforces the `accounts.id` FK on transactions.

### Schema

```sql
CREATE TABLE IF NOT EXISTS accounts (
    id          TEXT PRIMARY KEY,    -- UUIDv4
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL        -- ISO 8601 timestamp (RFC 3339)
);

CREATE TABLE IF NOT EXISTS transactions (
    id                TEXT PRIMARY KEY,     -- UUIDv4
    transaction_date  TEXT NOT NULL,         -- ISO 8601 'YYYY-MM-DD' (date only, no TZ)
    amount            TEXT NOT NULL,         -- decimal string, e.g. '-42.50', '1234.56'
    currency          TEXT NOT NULL,         -- 1-5 all-caps ASCII, e.g. 'USD', 'JPY', 'AUD'
    account_id        TEXT NOT NULL REFERENCES accounts(id),
    description       TEXT NOT NULL,
    metadata          TEXT,                  -- optional JSON string (no schema enforced)
    created_at        TEXT NOT NULL,         -- ISO 8601 timestamp
    updated_at        TEXT NOT NULL          -- ISO 8601 timestamp
);

CREATE INDEX IF NOT EXISTS idx_tx_date    ON transactions(transaction_date);
CREATE INDEX IF NOT EXISTS idx_tx_account ON transactions(account_id);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Seeded on first open: ('default_currency', 'USD')
```

#### Type rationale

**Amount as TEXT decimal string** — `REAL` (float) silently rounds money (`0.1 + 0.2 != 0.3`). `INTEGER` minor-units would require per-currency scale tracking (JPY uses 0 decimal places, BHD uses 3, USD/EUR use 2). TEXT decimal is currency-agnostic, preserves precision, and Excel reads it natively when CSV-exported. The Rust side parses with `rust_decimal::Decimal` for arithmetic (we do almost no arithmetic in v1 — sums for export summaries, possibly).

**Date as TEXT `YYYY-MM-DD`** — date-only granularity (no time, no time zone). ISO 8601 lex ordering equals chronological ordering, so the date index works for sorted retrieval. Human-readable in raw CSV exports.

**Currency as TEXT, validated `^[A-Z]{1,5}$`** — both client (TypeScript) and server (Rust) validate; the Rust validator is authoritative. We don't enforce against an ISO 4217 list because users may want to use crypto symbols (`BTC`, `ETH`), reward points (`UAVF` for United miles), or any other unit of value.

**JSON metadata as raw TEXT** — no JSON schema enforced. Rust validates that the field, if present, parses as valid JSON via `serde_json::from_str::<serde_json::Value>` before writing. No JSON1 query indexing in v1 — that's a Phase 2 enhancement if users want to slice on metadata fields.

**UUIDv4 for IDs** — stable across renames/moves, sync-friendly when Phase 2 lands (no integer-PK collision risk across devices).

**`updated_at` on transactions** — dead weight for v1 (we never read it), but cheap to maintain and saves a schema migration when Phase 2 sync needs to answer "which row is newer."

### API surface

#### Tauri command set (Rust → JS)

All commands are `#[tauri::command]`-annotated functions in `src-tauri/src/mint.rs`. All names are `snake_case` per CLAUDE.md.

```rust
// Transactions
async fn mint_list_transactions(
    date_from: Option<String>,     // 'YYYY-MM-DD' inclusive
    date_to: Option<String>,       // 'YYYY-MM-DD' inclusive
    account_id: Option<String>,
    state: State<'_, NodeState>,
) -> Result<Vec<Transaction>, String>;

async fn mint_get_transaction(
    id: String,
    state: State<'_, NodeState>,
) -> Result<Option<Transaction>, String>;

async fn mint_create_transaction(
    payload: NewTransaction,
    state: State<'_, NodeState>,
) -> Result<Transaction, String>;

async fn mint_update_transaction(
    id: String,
    payload: UpdateTransaction,
    state: State<'_, NodeState>,
) -> Result<Transaction, String>;

async fn mint_delete_transaction(
    id: String,
    state: State<'_, NodeState>,
) -> Result<(), String>;

// Accounts
async fn mint_list_accounts(
    state: State<'_, NodeState>,
) -> Result<Vec<Account>, String>;

async fn mint_create_account(
    name: String,
    state: State<'_, NodeState>,
) -> Result<Account, String>;

async fn mint_rename_account(
    id: String,
    name: String,
    state: State<'_, NodeState>,
) -> Result<Account, String>;

async fn mint_delete_account(
    id: String,
    reassign_to: Option<String>,   // if None and account has transactions, returns Err
    state: State<'_, NodeState>,
) -> Result<(), String>;

// Settings
async fn mint_get_default_currency(
    state: State<'_, NodeState>,
) -> Result<Option<String>, String>;

async fn mint_set_default_currency(
    currency: String,
    state: State<'_, NodeState>,
) -> Result<(), String>;

// Export
async fn mint_export_csv(
    output_path: String,
    date_from: Option<String>,
    date_to: Option<String>,
    state: State<'_, NodeState>,
) -> Result<ExportSummary, String>;
```

#### Types

```rust
#[derive(Serialize, Deserialize, Clone)]
struct Transaction {
    id: String,
    transaction_date: String,
    amount: String,
    currency: String,
    account_id: String,
    account_name: String,          // denormalized on read for UI convenience
    description: String,
    metadata: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct NewTransaction {
    transaction_date: String,
    amount: String,
    currency: String,
    account_id: String,
    description: String,
    metadata: Option<String>,
}

#[derive(Deserialize)]
struct UpdateTransaction {
    transaction_date: Option<String>,
    amount: Option<String>,
    currency: Option<String>,
    account_id: Option<String>,
    description: Option<String>,
    metadata: Option<Option<String>>,   // double Option: outer = "should this field be updated", inner = "new value, possibly null to clear"
}

#[derive(Serialize)]
struct Account {
    id: String,
    name: String,
    created_at: String,
    transaction_count: u64,         // computed on read for UI display
}

#[derive(Serialize)]
struct ExportSummary {
    rows_written: u64,
    output_path: String,
    byte_size: u64,
}
```

#### TypeScript types (mirror of Rust)

```typescript
export interface Transaction {
    id: string;
    transactionDate: string;        // ISO 8601 'YYYY-MM-DD'
    amount: string;                  // decimal string
    currency: string;
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

// ...mirror remaining types

export interface Account {
    id: string;
    name: string;
    createdAt: string;
    transactionCount: number;
}

export interface ExportSummary {
    rowsWritten: number;
    outputPath: string;
    byteSize: number;
}
```

### Validation

All validation happens server-side (Rust) in `mint.rs`. Client-side checks in `mint-service.ts` exist for UX (immediate feedback) but are not authoritative.

| Field | Validation rule |
|---|---|
| `transaction_date` | Matches `^\d{4}-\d{2}-\d{2}$`, parses as valid `chrono::NaiveDate` |
| `amount` | Matches `^-?\d+(\.\d+)?$`, parses as valid `rust_decimal::Decimal` |
| `currency` | Matches `^[A-Z]{1,5}$` |
| `description` | Non-empty after trim, max 4096 bytes |
| `metadata` | If present, parses as valid JSON (`serde_json::Value`), max 64 KiB serialized |
| `account_id` | Exists in `accounts` table |
| `name` (account) | Non-empty after trim, max 256 bytes, unique within accounts (case-sensitive) |

Validation errors are returned as `Err(String)` with a human-readable message; the TS layer surfaces these in the UI.

### CSV export format

One row per transaction; columns in this order:

```
date,account_name,amount,currency,description,metadata
2026-05-19,Chase Checking,-42.50,USD,"Coffee at the airport","{""tag"":""travel""}"
2026-05-18,United Miles,1500,UAVF,"Booking bonus",
```

- Standard RFC 4180 escaping (double-quote-wrap fields containing comma/quote/newline; embedded quotes doubled)
- Metadata column contains the raw JSON string verbatim, RFC 4180-escaped
- Header row is always emitted (no opt-out in v1)
- UTF-8 encoding, LF line endings (not CRLF — Excel handles both fine)

CSV writing uses the `csv` crate. Output streams directly to the destination file (no in-memory buffering of the whole result set) — keeps export usable even at large ledger sizes.

### UI flow

The MintLedger panel layout:

```
┌─ MintLedger ────────────────────────────────────────────────┐
│ [Date range: All ▾]  [Account: All ▾]  Default: USD ▾       │
│ [+ Add Transaction]   [Manage Accounts]   [Export CSV]      │
├─────────────────────────────────────────────────────────────┤
│ Date        Account         Amount      Currency  Description│
│ 2026-05-19  Chase Checking  -42.50      USD       Coffee...  │
│ 2026-05-18  United Miles    1500        UAVF      Booking... │
│ ...                                                          │
└─────────────────────────────────────────────────────────────┘
```

Click a row → opens the edit dialog. Right-click (or row hover menu) → delete with confirm.

The Add/Edit dialog has fields for date (defaulting to today), amount, currency (defaulting to user default), account (dropdown with "+ New account" inline option), description, and a collapsible JSON metadata textarea.

Manage Accounts dialog shows all accounts with their transaction counts, inline rename, delete (with reassign-to dropdown if the account has transactions).

Export CSV opens a native save dialog via Tauri's `dialog::save`, then calls `mint_export_csv` and shows a toast with the rows-written summary.

## Test plan

### Rust unit tests (in `mint.rs`)

- Schema migration on empty database creates expected tables and indexes
- Schema migration on existing database is a no-op
- Account CRUD: create / list / rename / delete (with and without transactions)
- Transaction CRUD: create / get / list (with all filter permutations) / update (partial and full) / delete
- Validation rejects malformed date, amount, currency, account_id, metadata JSON
- `metadata: Some(None)` on update correctly clears the field
- Default currency setting round-trips

### Rust integration tests (`tests/mint_integration.rs`)

- Full lifecycle: create accounts, add transactions, edit, delete, list-with-filters
- CSV export from a fixture of ~100 transactions; output parses correctly and round-trips through `csv::Reader`
- CSV escaping: transactions with commas, quotes, embedded newlines in description and metadata
- Account delete refuses when transactions exist and `reassign_to=None`
- Account delete with `reassign_to=Some(other)` correctly reassigns transactions

### TypeScript tests (`src/lib/mint-service.test.ts`)

- `MintService` correctly invokes Tauri commands with camelCase parameters
- Error extraction handles both `Error` objects (test mode) and plain strings (production)
- Optional fields serialize correctly

### Svelte component tests

- `MintTransactionDialog` validates inputs and disables Save until valid
- `MintTransactionTable` renders, sorts, and filters as expected
- `MintAccountManager` blocks delete-without-reassign when transactions exist

### Manual smoke test

On Windows (Ildwyn), via `npm run tauri dev`:

1. Open app, switch to Mint mode
2. Create two accounts: "Chase Checking", "United Miles"
3. Set default currency to USD
4. Add 5 transactions across both accounts, including one with metadata JSON and one in JPY
5. Edit one transaction (change amount), delete one
6. Filter by date range and by account
7. Export CSV to Desktop, open in Excel, verify all data round-trips

## Decisions log

- **D1: Single SQLite file vs CAS-native event log.** Chose SQLite for v1; deferred CAS integration to Phase 2. Rationale: full CRUD requirement makes event log unnatural (every edit/delete becomes a tombstone-or-update event), and local-only v1 ships in ~1 day vs ~3-4 days for event-log MVP. CAS sync remains the long-term target as a separate ticket.
- **D2: Amount as TEXT decimal vs INTEGER minor units.** Chose TEXT because v1 supports arbitrary currencies (including non-ISO ones like crypto and reward points) and we don't want to maintain a per-currency-scale table. The slight DB-side query inconvenience (no SQL arithmetic on text) is irrelevant because we don't do analytical queries in v1 and the CSV export is the analytical path.
- **D3: Separate accounts table vs inline string.** Chose separate table with UUID PK. Renames update one row; transactions auto-show the new name. Adds ~10 lines of code, prevents the "I renamed Chase to Chase Checking and now my historical transactions are split" footgun.
- **D4: UUID PKs vs autoincrement integers.** Chose UUIDs to be sync-ready for Phase 2 (no integer-PK collision risk when reconciling two devices' independent inserts).
- **D5: `updated_at` column included in v1 despite no v1 reader.** Costs nothing to maintain; saves a schema migration when Phase 2 sync needs it. The schema is also forward-extensible — Phase 2 can add columns for sync metadata (e.g. `last_synced_at`, `origin_device_id`) without disturbing v1.

## Out-of-scope follow-ups

Track these as future tickets if v1 proves useful:

- **Phase 2: CAS sync.** Snapshot the SQLite file to CAS on debounced writes, track latest snapshot CID in app settings, peer pull on app start. Last-write-wins per whole-DB; offline-multi-write produces a "your other device made conflicting changes" warning. Likely its own ZEB ticket once v1 ships.
- **Bank CSV ingest.** Plaid is out of scope (third-party SaaS, OAuth, money) but plain bank CSV import is plausible — map columns, dedupe by hash, write through.
- **Analytical view.** Charts, category breakdowns, monthly aggregates. JSON metadata becomes interesting here.
- **Tags as first-class.** Promote out of metadata JSON into their own join table once usage stabilizes.
- **Multi-currency reporting.** Hold off until users actually want to see "total spent" across currencies. FX rates make this hard.
- **Soft delete / undo.** Add `deleted_at` column to transactions; UI hides them; "trash" view shows them with restore.
