//! Mint — personal-finance transaction tracker backend.
//!
//! This module owns the SQLite layer for the Mint feature: schema
//! migration, database connection lifecycle, settings management, and
//! account CRUD.  Transaction CRUD and IPC wiring are added in subsequent
//! tasks.
//!
//! Spec: `docs/specs/2026-05-19-mint-mvp-design.md`
//! Plan: `docs/plans/2026-05-19-mint-mvp-plan.md`

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

const DEFAULT_CURRENCY_KEY: &str = "default_currency";

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum MintError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Error: {0}")]
    Other(String),
}

/// Convert `MintError` to a plain `String` for the Tauri IPC seam.
/// Tauri commands return `Result<T, String>` so callers get a human-readable
/// rejection message without needing to know the internal error enum.
impl From<MintError> for String {
    fn from(e: MintError) -> String {
        e.to_string()
    }
}

// ── Database lifecycle ────────────────────────────────────────────────────────

/// 16-byte header of a plaintext SQLite database file. An SQLCipher-
/// encrypted database starts with its random per-file salt instead, so
/// this magic doubles as the pre-ZEB-985 plaintext detector.
const SQLITE_PLAINTEXT_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Open (or create) the Mint SQLite database at `path`, encrypted at rest
/// with SQLCipher under `key` (ZEB-985).
///
/// The key is applied raw (`PRAGMA key = "x'…'"` — no PBKDF2 pass; the
/// caller hands us uniformly random HKDF output, see
/// `derive_mint_ledger_key`). A pre-ZEB-985 plaintext database found at
/// `path` is migrated in place first (see
/// [`migrate_plaintext_to_encrypted`]). Applies WAL journaling and
/// foreign-key enforcement pragmas, then runs `apply_migrations` to ensure
/// the schema is up to date.
///
/// A wrong key — or a corrupt file — surfaces as a `Sqlite` error from the
/// first statement after keying (SQLCipher reports "file is not a
/// database"); callers keep their existing disarm-don't-brick contract.
pub fn open_database(path: &std::path::Path, key: &[u8; 32]) -> Result<Connection, MintError> {
    // Ensure parent directory exists (create_dir_all is a no-op if already
    // present, and propagates Io errors via the #[from] impl above).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if is_plaintext_sqlite(path)? {
        migrate_plaintext_to_encrypted(path, key)?;
    }

    let conn = Connection::open(path)?;
    apply_sqlcipher_key(&conn, key)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    apply_migrations(&conn)?;
    Ok(conn)
}

/// Key a freshly opened connection with the raw SQLCipher key. MUST run
/// before any other statement touches the file — SQLCipher only honours
/// the key while the database is still unread.
fn apply_sqlcipher_key(conn: &Connection, key: &[u8; 32]) -> Result<(), MintError> {
    // Raw-key form (`x'…'` blob literal inside double quotes, per the
    // SQLCipher docs): exactly 32 bytes of key material, no KDF.
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex_lower(key)))?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Does the file at `path` start with the plaintext SQLite magic?
/// A missing (or shorter-than-header) file is simply "no" — a fresh
/// database is created encrypted directly.
fn is_plaintext_sqlite(path: &std::path::Path) -> Result<bool, MintError> {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut header = [0u8; 16];
    match f.read_exact(&mut header) {
        Ok(()) => Ok(&header == SQLITE_PLAINTEXT_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// One-time ZEB-985 migration: rebuild the plaintext database at `path`
/// into an SQLCipher-encrypted sibling via `sqlcipher_export`, then
/// atomically swap it into place.
///
/// The plaintext connection's journal mode is flipped to DELETE first,
/// which checkpoints and removes its `-wal` sidecar, so no plaintext rows
/// outlive the swap in a journal file; the `-shm` index is removed
/// defensively afterwards. The rename is the commit point — a crash
/// before it leaves the plaintext database intact and the migration
/// re-runs on next open; a stale temp file from an interrupted attempt is
/// discarded up front.
fn migrate_plaintext_to_encrypted(path: &std::path::Path, key: &[u8; 32]) -> Result<(), MintError> {
    let tmp = path.with_extension("db.enc-migrate");
    let _ = std::fs::remove_file(&tmp);
    {
        let plain = Connection::open(path)?;
        plain.pragma_update(None, "journal_mode", "DELETE")?;
        // Single-quote SQL string escaping for the temp path (app-data
        // paths are tame, but tests run under arbitrary tempdirs).
        let tmp_sql = tmp.to_string_lossy().replace('\'', "''");
        plain.execute_batch(&format!(
            "ATTACH DATABASE '{tmp_sql}' AS encrypted KEY \"x'{}'\";
             SELECT sqlcipher_export('encrypted');
             DETACH DATABASE encrypted;",
            hex_lower(key)
        ))?;
    }
    std::fs::rename(&tmp, path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    Ok(())
}

/// Opens a fresh in-memory SQLite database and applies all
/// migrations. Useful for tests that don't need on-disk persistence.
/// Production code should call `open_database` with a file path.
pub fn open_in_memory() -> Result<Connection, MintError> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    apply_migrations(&conn)?;
    Ok(conn)
}

/// Apply all schema migrations idempotently.
///
/// Uses `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` so this
/// can be called on every app start without error.  The `accounts` table
/// includes a `UNIQUE(name)` constraint; we are pre-launch so all test
/// databases are in-memory and no on-disk migration is required.
pub fn apply_migrations(conn: &Connection) -> Result<(), MintError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS accounts (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS transactions (
            id                TEXT PRIMARY KEY,
            transaction_date  TEXT NOT NULL,
            amount            TEXT NOT NULL,
            currency          TEXT NOT NULL,
            account_id        TEXT NOT NULL REFERENCES accounts(id),
            description       TEXT NOT NULL,
            metadata          TEXT,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_tx_date    ON transactions(transaction_date);
        CREATE INDEX IF NOT EXISTS idx_tx_account ON transactions(account_id);

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES (?, 'USD')",
        params![DEFAULT_CURRENCY_KEY],
    )?;

    // --- Schema v2 (Phase 2 sync) ---
    //
    // `let _ = conn.execute("ALTER TABLE ...")` swallows the "column already
    // exists" error that SQLite fires on every run after the first. This is
    // the idempotency idiom for ADD COLUMN — do NOT replace `let _ =` with
    // `?`. The subsequent CREATE INDEX IF NOT EXISTS and UPDATE statements are
    // safe to chain with `?` because they are inherently idempotent.

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

    // settings.updated_at — backfilled to epoch for legacy rows.
    // Use epoch (1970-01-01T00:00:00Z) rather than wall-clock now() so that
    // any explicit user change (which always carries chrono::Utc::now()) wins
    // unconditionally in LWW merge. Per-device wall-clock backfill was causing
    // a peer whose migration ran later (T_B > T_A) to silently revert another
    // peer's earlier intentional edit (T_A) on next sync.
    let _ = conn.execute(
        "ALTER TABLE settings ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
        [],
    );
    conn.execute(
        "UPDATE settings SET updated_at = '1970-01-01T00:00:00Z' WHERE updated_at = ''",
        [],
    )?;

    Ok(())
}

// ── Settings ──────────────────────────────────────────────────────────────────

/// Read the current default currency from the settings table.
///
/// Returns `None` if the row has been deleted (unlikely in normal operation
/// because `apply_migrations` seeds it, but defensive is correct here).
pub fn get_default_currency(conn: &Connection) -> Result<Option<String>, MintError> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?")?;
    let mut rows = stmt.query(params![DEFAULT_CURRENCY_KEY])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Persist a new default currency.
///
/// Validates the currency string before writing; returns
/// `MintError::Validation` on failure.
pub fn set_default_currency(conn: &Connection, currency: &str) -> Result<(), MintError> {
    validate_currency(currency)?;
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![DEFAULT_CURRENCY_KEY, currency, now],
    )?;
    Ok(())
}

// ── Validators ────────────────────────────────────────────────────────────────

/// Validate a currency string: 1–5 ASCII uppercase letters.
///
/// Accepts ISO 4217 codes (`USD`, `JPY`, `AUD`) and non-ISO symbols
/// (`BTC`, `UAVF` for United miles, etc.) per spec § Validation.
pub fn validate_currency(s: &str) -> Result<(), MintError> {
    if (1..=5).contains(&s.len()) && s.chars().all(|c| c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(MintError::Validation(
            "currency must be 1-5 ASCII uppercase letters".into(),
        ))
    }
}

// ── Account types ─────────────────────────────────────────────────────────────

/// A named account that transactions are posted against.
///
/// `rename_all = "camelCase"` ensures the Tauri IPC seam emits camelCase field
/// names to the frontend (matches CLAUDE.md doctrine).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub transaction_count: u64,
}

// ── Account CRUD ──────────────────────────────────────────────────────────────

/// Create a new account with the given name.
///
/// Returns a `MintError::Validation` if the name fails validation or if an
/// account with the same name already exists.
pub fn create_account(conn: &Connection, name: &str) -> Result<Account, MintError> {
    let trimmed_name = validate_account_name(name)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        rusqlite::params![id, trimmed_name, now],
    )
    .map_err(map_account_name_constraint)?;
    Ok(Account {
        id,
        name: trimmed_name,
        created_at: now,
        transaction_count: 0,
    })
}

/// Return all accounts ordered case-insensitively by name, each annotated with
/// the number of transactions posted to it.
pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>, MintError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, COUNT(t.id) AS tx_count
         FROM accounts a
         LEFT JOIN transactions t ON t.account_id = a.id AND t.deleted_at IS NULL
         GROUP BY a.id, a.name, a.created_at
         ORDER BY a.name COLLATE NOCASE",
    )?;
    let accounts = stmt
        .query_map([], |row| {
            Ok(Account {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                transaction_count: row.get::<_, u64>(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(accounts)
}

/// Rename an existing account.
///
/// Returns `MintError::NotFound` if `id` does not exist, or
/// `MintError::Validation` if the new name fails validation or is already
/// taken by another account.
pub fn rename_account(conn: &Connection, id: &str, new_name: &str) -> Result<Account, MintError> {
    let trimmed_name = validate_account_name(new_name)?;
    let now = chrono::Utc::now().to_rfc3339();
    let affected = conn
        .execute(
            "UPDATE accounts SET name = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![trimmed_name, now, id],
        )
        .map_err(map_account_name_constraint)?;
    if affected == 0 {
        return Err(MintError::NotFound("account not found".into()));
    }
    // Point-query re-fetch (includes accurate transaction_count).
    get_account_by_id(conn, id)?
        .ok_or_else(|| MintError::Other("account vanished between rename and read".into()))
}

/// Delete an account.
///
/// If `reassign_to` is `None` and the account has any transactions the call
/// fails with `MintError::Validation`.  If `reassign_to` is `Some(target_id)`
/// all transactions are moved to the target before deletion, atomically.
///
/// All preconditions are checked before any mutation so that a validation
/// failure always leaves the database unchanged.
pub fn delete_account(
    conn: &Connection,
    id: &str,
    reassign_to: Option<&str>,
) -> Result<(), MintError> {
    // `unchecked_transaction` is correct here: `conn` is a `&Connection`
    // already obtained from the `Mutex<Connection>` guard, so we know we
    // are the sole user of this connection.
    //
    // NOTE: the caller (mint_delete_account IPC handler) is responsible for
    // inserting the deletion-floor entry and persisting it to disk BEFORE
    // calling this function. This ordering ensures that a crash between floor
    // persist and SQLite commit leaves a "phantom" floor entry (minor
    // inconvenience) rather than a committed SQLite delete with no floor entry
    // (zombie resurrection risk).
    let tx = conn.unchecked_transaction()?;

    // ── Validate first, before any mutation ───────────────────────────────────

    let exists: i64 = tx.query_row(
        "SELECT COUNT(*) FROM accounts WHERE id = ?",
        params![id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(MintError::NotFound("account not found".into()));
    }

    if let Some(target) = reassign_to {
        if target == id {
            return Err(MintError::Validation(
                "cannot reassign to the account being deleted".into(),
            ));
        }
        let target_exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?",
            params![target],
            |r| r.get(0),
        )?;
        if target_exists == 0 {
            return Err(MintError::Validation(
                "reassign_to target does not exist".into(),
            ));
        }
    }

    if reassign_to.is_none() {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM transactions WHERE account_id = ? AND deleted_at IS NULL",
            params![id],
            |r| r.get(0),
        )?;
        if count > 0 {
            return Err(MintError::Validation(
                "account has transactions; pass reassign_to".into(),
            ));
        }
        // No LIVE transactions — but tombstoned (soft-deleted) rows may still
        // reference this account through the enforced FK (the schema has no
        // ON DELETE CASCADE). Without purging them, the account DELETE below
        // fails with a foreign-key violation, and in the command path the
        // deletion floor has already been persisted → a phantom floor for an
        // account that still exists. Purge the dead rows first, mirroring the
        // sync-apply path (`mint_sync::apply_remote_snapshot`, which likewise
        // DELETEs an account's transactions before the account itself).
        tx.execute("DELETE FROM transactions WHERE account_id = ?", params![id])?;
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    if let Some(target) = reassign_to {
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE transactions SET account_id = ?, updated_at = ? WHERE account_id = ?",
            rusqlite::params![target, now, id],
        )?;
    }
    tx.execute("DELETE FROM accounts WHERE id = ?", params![id])?;

    tx.commit()?;

    Ok(())
}

// ── Account validators ────────────────────────────────────────────────────────

/// Validate an account name: non-empty after trim, max 256 bytes.
///
/// Returns the trimmed string so callers store the canonical form, which
/// ensures `" Chase "` and `"Chase"` are treated as duplicates by the
/// `UNIQUE(name)` constraint.
pub fn validate_account_name(s: &str) -> Result<String, MintError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(MintError::Validation("account name cannot be empty".into()));
    }
    if trimmed.len() > 256 {
        return Err(MintError::Validation(
            "account name exceeds 256 bytes".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Maps SQLite UNIQUE-constraint violations to a friendly validation error
/// for the `accounts.name` column.  Other rusqlite errors pass through.
fn map_account_name_constraint(e: rusqlite::Error) -> MintError {
    match e {
        rusqlite::Error::SqliteFailure(ref fe, _)
            if fe.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            MintError::Validation("account name already exists".into())
        }
        other => other.into(),
    }
}

// ── Transaction types ─────────────────────────────────────────────────────────

/// Summary returned by `export_csv` and `mint_export_csv`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub rows_written: u64,
    pub output_path: String,
    pub byte_size: u64,
}

/// A posted transaction.
///
/// `account_name` is derived from the accounts JOIN — it is never stored in the
/// transactions table directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub id: String,
    pub transaction_date: String,
    pub amount: String,
    pub currency: String,
    pub account_id: String,
    pub account_name: String,
    pub description: String,
    pub metadata: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Input payload for creating a transaction.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTransaction {
    pub transaction_date: String,
    pub amount: String,
    pub currency: String,
    pub account_id: String,
    pub description: String,
    pub metadata: Option<String>,
}

/// Input payload for updating a transaction.
///
/// Every field is optional so callers can update individual fields.
/// `metadata` uses double-Option so callers can distinguish:
/// - `Some(Some(json_str))` — set the metadata value
/// - `Some(None)` — clear the field to NULL
/// - `None` (absent from JSON) — leave the field alone
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTransaction {
    pub transaction_date: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub account_id: Option<String>,
    pub description: Option<String>,
    /// Outer `Some` = caller wants to update metadata; inner `None` = set to NULL.
    /// Absent (`None`) = leave the field alone.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub metadata: Option<Option<String>>,
}

/// Serde helper: deserializes `T` into `Some(T)` so that a JSON `null` maps to
/// `Some(None)` and an absent field (via `#[serde(default)]`) maps to `None`.
/// Without this helper, both absent and `null` collapse to `None`.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Filter for `list_transactions`.  All fields are optional; absent fields are
/// not applied to the WHERE clause.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilter {
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub account_id: Option<String>,
}

// ── Transaction validators ────────────────────────────────────────────────────

/// Validate a transaction date string: must be parseable as `YYYY-MM-DD`.
fn validate_date(s: &str) -> Result<(), MintError> {
    // chrono's `%Y` parser is greedy and accepts shorter years like "26"
    // (interpreted as year 26 AD). For our YYYY-MM-DD contract we require
    // exactly 10 characters in the `NNNN-NN-NN` shape before delegating
    // to chrono for the month/day range validation.
    let bytes = s.as_bytes();
    let well_formed = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit());
    if !well_formed {
        return Err(MintError::Validation(format!(
            "invalid date '{s}'; expected YYYY-MM-DD"
        )));
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| MintError::Validation(format!("invalid date '{s}'; expected YYYY-MM-DD")))
}

/// Validate an amount string: must be a plain decimal number (no scientific
/// notation, no thousands separators, optional leading sign, optional `.`
/// fractional part).
///
/// `rust_decimal::Decimal::from_str` accepts scientific notation (`"1e5"` →
/// 100000) as of v1.x. Our user-facing amount fields are typed into a numeric
/// keypad context where scientific notation is never intentional; accepting
/// it would silently convert a mis-typed `e` into a 5+-magnitude number.
/// We pre-check for `e`/`E` and reject before handing off to Decimal.
fn validate_amount(s: &str) -> Result<(), MintError> {
    use std::str::FromStr;
    if s.bytes().any(|b| b == b'e' || b == b'E') {
        return Err(MintError::Validation(format!(
            "invalid amount '{s}': scientific notation not allowed"
        )));
    }
    rust_decimal::Decimal::from_str(s)
        .map(|_| ())
        .map_err(|_| MintError::Validation(format!("invalid amount '{s}'")))
}

/// Validate a description string: non-empty after trim, max 4096 bytes.
fn validate_description(s: &str) -> Result<(), MintError> {
    if s.trim().is_empty() {
        return Err(MintError::Validation("description cannot be empty".into()));
    }
    if s.len() > 4096 {
        return Err(MintError::Validation(
            "description exceeds 4096 bytes".into(),
        ));
    }
    Ok(())
}

/// Validate a metadata string: must be valid JSON and ≤ 64 KiB.
fn validate_metadata(s: &str) -> Result<(), MintError> {
    if s.len() > 65_536 {
        return Err(MintError::Validation("metadata exceeds 64 KiB".into()));
    }
    serde_json::from_str::<serde_json::Value>(s)
        .map(|_| ())
        .map_err(|e| MintError::Validation(format!("metadata is not valid JSON: {e}")))
}

/// Validate the user-controllable fields of a transaction, applying exactly the
/// same rules as `create_transaction` / `update_transaction`.
///
/// Exposed within the crate so the sync ingest path
/// (`mint_sync::upsert_transaction_lww`) can reject a malformed peer row before
/// it persists invalid ledger data (bad date/amount/currency/description/
/// metadata) that list/export would then surface — the local command path
/// already enforces these, but a peer snapshot bypasses it (ZEB-989/CA4).
pub(crate) fn validate_transaction_fields(
    transaction_date: &str,
    amount: &str,
    currency: &str,
    description: &str,
    metadata: Option<&str>,
) -> Result<(), MintError> {
    validate_date(transaction_date)?;
    validate_amount(amount)?;
    validate_currency(currency)?;
    validate_description(description)?;
    if let Some(m) = metadata {
        validate_metadata(m)?;
    }
    Ok(())
}

// ── Transaction CRUD ──────────────────────────────────────────────────────────

/// Column list + JOIN used by all transaction reads. The 10-column
/// projection order is: id, transaction_date, amount, currency,
/// account_id, account_name (from JOIN), description, metadata,
/// created_at, updated_at — matching the `Transaction` struct field
/// order.
/// Base SELECT + FROM + JOIN for all transaction reads. Always includes the
/// soft-delete filter so no caller can accidentally surface tombstoned rows.
/// Additional predicates must be appended with `AND` (not a second `WHERE`).
const TRANSACTION_SELECT: &str = "SELECT t.id, t.transaction_date, t.amount, t.currency, \
    t.account_id, a.name, t.description, t.metadata, t.created_at, t.updated_at \
    FROM transactions t JOIN accounts a ON a.id = t.account_id \
    WHERE t.deleted_at IS NULL";

fn map_transaction_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Transaction> {
    Ok(Transaction {
        id: row.get(0)?,
        transaction_date: row.get(1)?,
        amount: row.get(2)?,
        currency: row.get(3)?,
        account_id: row.get(4)?,
        account_name: row.get(5)?,
        description: row.get(6)?,
        metadata: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// Create a new transaction from the given payload.
///
/// Validates all fields, verifies that `account_id` exists, then inserts and
/// returns the freshly-created Transaction (with `account_name` populated via
/// JOIN).
pub fn create_transaction(
    conn: &Connection,
    payload: NewTransaction,
) -> Result<Transaction, MintError> {
    validate_date(&payload.transaction_date)?;
    validate_amount(&payload.amount)?;
    validate_currency(&payload.currency)?;
    validate_description(&payload.description)?;
    if let Some(ref m) = payload.metadata {
        validate_metadata(m)?;
    }

    // Verify that the account exists.
    let account_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM accounts WHERE id = ?",
        params![payload.account_id],
        |r| r.get(0),
    )?;
    if account_exists == 0 {
        return Err(MintError::Validation("account does not exist".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO transactions \
         (id, transaction_date, amount, currency, account_id, description, metadata, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id,
            payload.transaction_date,
            payload.amount,
            payload.currency,
            payload.account_id,
            payload.description,
            payload.metadata,
            now,
            now,
        ],
    )?;

    get_transaction(conn, &id)?
        .ok_or_else(|| MintError::Other("transaction vanished immediately after insert".into()))
}

/// Return the transaction with the given `id`, or `None` if no such row exists.
/// Soft-deleted rows are excluded.
pub fn get_transaction(conn: &Connection, id: &str) -> Result<Option<Transaction>, MintError> {
    let sql = format!("{TRANSACTION_SELECT} AND t.id = ?");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(map_transaction_row(row)?))
    } else {
        Ok(None)
    }
}

/// Return all transactions that match `filter`, ordered by transaction_date DESC
/// then id DESC.
///
/// Filter fields that are `None` are omitted from the WHERE clause.  Date bounds
/// are validated before the query runs.
pub fn list_transactions(
    conn: &Connection,
    filter: &ListFilter,
) -> Result<Vec<Transaction>, MintError> {
    let mut conditions: Vec<&'static str> = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(d) = &filter.date_from {
        validate_date(d)?;
        conditions.push("t.transaction_date >= ?");
        params_vec.push(Box::new(d.clone()));
    }
    if let Some(d) = &filter.date_to {
        validate_date(d)?;
        conditions.push("t.transaction_date <= ?");
        params_vec.push(Box::new(d.clone()));
    }
    if let Some(a) = &filter.account_id {
        conditions.push("t.account_id = ?");
        params_vec.push(Box::new(a.clone()));
    }

    // TRANSACTION_SELECT already contains `WHERE t.deleted_at IS NULL`.
    // Additional conditions are appended with `AND`.
    let extra = if conditions.is_empty() {
        String::new()
    } else {
        format!("AND {}", conditions.join(" AND "))
    };
    let sql = format!("{TRANSACTION_SELECT} {extra} ORDER BY t.transaction_date DESC, t.id DESC");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())),
        map_transaction_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(MintError::from)
}

/// Update the given transaction with any present fields in `payload`.
///
/// Returns `MintError::NotFound` if `id` does not match any row.  Always bumps
/// `updated_at`.  If `account_id` is being changed the new account is verified
/// to exist before the UPDATE runs.
pub fn update_transaction(
    conn: &Connection,
    id: &str,
    payload: UpdateTransaction,
) -> Result<Transaction, MintError> {
    let mut sets: Vec<&'static str> = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(d) = &payload.transaction_date {
        validate_date(d)?;
        sets.push("transaction_date = ?");
        params_vec.push(Box::new(d.clone()));
    }
    if let Some(a) = &payload.amount {
        validate_amount(a)?;
        sets.push("amount = ?");
        params_vec.push(Box::new(a.clone()));
    }
    if let Some(c) = &payload.currency {
        validate_currency(c)?;
        sets.push("currency = ?");
        params_vec.push(Box::new(c.clone()));
    }
    if let Some(acc) = &payload.account_id {
        // Verify the new account exists before mutating.
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?",
            params![acc],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(MintError::Validation("account does not exist".into()));
        }
        sets.push("account_id = ?");
        params_vec.push(Box::new(acc.clone()));
    }
    if let Some(desc) = &payload.description {
        validate_description(desc)?;
        sets.push("description = ?");
        params_vec.push(Box::new(desc.clone()));
    }
    match payload.metadata {
        Some(Some(ref m)) => {
            validate_metadata(m)?;
            sets.push("metadata = ?");
            params_vec.push(Box::new(m.clone()));
        }
        Some(None) => {
            sets.push("metadata = ?");
            params_vec.push(Box::new(rusqlite::types::Null));
        }
        None => {
            // Leave metadata untouched.
        }
    }

    if sets.is_empty() {
        // Nothing to update — just re-fetch and return.
        return get_transaction(conn, id)?
            .ok_or_else(|| MintError::NotFound("transaction not found".into()));
    }

    sets.push("updated_at = ?");
    let now = chrono::Utc::now().to_rfc3339();
    params_vec.push(Box::new(now));
    // WHERE id = ? AND deleted_at IS NULL — must come last.
    // Exclude tombstoned rows: a soft-deleted transaction is not updatable.
    params_vec.push(Box::new(id.to_string()));

    let sql = format!(
        "UPDATE transactions SET {} WHERE id = ? AND deleted_at IS NULL",
        sets.join(", ")
    );
    let affected = conn.execute(
        &sql,
        rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())),
    )?;

    if affected == 0 {
        return Err(MintError::NotFound("transaction not found".into()));
    }

    get_transaction(conn, id)?
        .ok_or_else(|| MintError::Other("transaction vanished between update and read".into()))
}

/// Soft-delete a transaction by id.
///
/// Sets `deleted_at` and bumps `updated_at` to the current UTC time.
/// If the row is already tombstoned the call is a no-op (original
/// `deleted_at` timestamp is preserved — idempotent).
/// Returns `MintError::NotFound` if the row never existed.
pub fn delete_transaction(conn: &Connection, id: &str) -> Result<(), MintError> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = conn.execute(
        "UPDATE transactions SET deleted_at = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
        rusqlite::params![now, now, id],
    )?;
    if rows == 0 {
        // Either the row doesn't exist or it's already tombstoned; both are OK
        // from the caller's perspective. Distinguish so we can return NotFound
        // for the "never existed" case (matches v1 hard-delete semantics for
        // missing rows).
        // No TOCTOU concern in practice: Mint is single-writer and UUIDs are not reused.
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM transactions WHERE id = ?",
            [id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(MintError::NotFound(format!("transaction {id}")));
        }
        // Already tombstoned: no-op success — preserves the original
        // deleted_at timestamp.
    }
    Ok(())
}

// ── CSV export ────────────────────────────────────────────────────────────────

/// Streams the transaction ledger to a CSV file at `output_path`.
///
/// Header row is always emitted. Date filters are inclusive on both
/// bounds and validated up front. The query joins accounts for the
/// human-readable account name. RFC 4180 escaping is handled by the
/// `csv` crate.
///
/// Streams row-by-row from the SQLite cursor into csv::Writer to keep
/// memory bounded regardless of ledger size.
pub fn export_csv(
    conn: &Connection,
    output_path: &std::path::Path,
    date_from: Option<&str>,
    date_to: Option<&str>,
    account_id: Option<&str>,
) -> Result<ExportSummary, MintError> {
    if let Some(d) = date_from {
        validate_date(d)?;
    }
    if let Some(d) = date_to {
        validate_date(d)?;
    }

    // Ensure parent directory exists. If the caller passed a path inside
    // a directory that doesn't exist, create it. This matches the
    // behavior of `open_database`.
    let parent = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&parent)?;

    // Write to a sibling tempfile and atomically rename on success so a
    // query failure never leaves a partial file at `output_path`.
    // NamedTempFile::new_in places the temp file in the same directory
    // as the target — required for the rename in `.persist()` to be
    // atomic (cross-device renames aren't atomic on POSIX).
    let tmp = tempfile::NamedTempFile::new_in(&parent)
        .map_err(|e| MintError::Other(format!("temp file: {e}")))?;

    // csv::Writer takes ownership of the writer. Use a &File reference
    // borrowed from the NamedTempFile so the tempfile guard outlives the
    // writer and we can `.persist(...)` it after the writer is dropped.
    let mut count: u64 = 0;
    {
        let mut writer = csv::WriterBuilder::new()
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(tmp.as_file());

        writer
            .write_record([
                "date",
                "account_name",
                "amount",
                "currency",
                "description",
                "metadata",
            ])
            .map_err(|e| MintError::Other(format!("csv header: {e}")))?;

        let sql = "SELECT t.transaction_date, a.name, t.amount, t.currency, \
            t.description, COALESCE(t.metadata, '') \
            FROM transactions t JOIN accounts a ON a.id = t.account_id \
            WHERE t.deleted_at IS NULL \
              AND (?1 IS NULL OR t.transaction_date >= ?1) \
              AND (?2 IS NULL OR t.transaction_date <= ?2) \
              AND (?3 IS NULL OR t.account_id = ?3) \
            ORDER BY t.transaction_date ASC, t.id ASC";

        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params![date_from, date_to, account_id])?;
        while let Some(row) = rows.next()? {
            let date: String = row.get(0)?;
            let account: String = row.get(1)?;
            let amount: String = row.get(2)?;
            let currency: String = row.get(3)?;
            let description: String = row.get(4)?;
            let metadata: String = row.get(5)?;
            writer
                .write_record([&date, &account, &amount, &currency, &description, &metadata])
                .map_err(|e| MintError::Other(format!("csv row: {e}")))?;
            count += 1;
        }

        writer
            .flush()
            .map_err(|e| MintError::Other(format!("csv flush: {e}")))?;
        // writer drops here, releasing the &File borrow
    }

    // Atomically rename the tempfile into place. persist returns an error
    // if the rename fails (e.g., disk full, permissions); the temp file is
    // then automatically cleaned up on Drop.
    tmp.persist(output_path)
        .map_err(|e| MintError::Other(format!("persist: {}", e.error)))?;

    let byte_size = std::fs::metadata(output_path)?.len();
    Ok(ExportSummary {
        rows_written: count,
        output_path: output_path.display().to_string(),
        byte_size,
    })
}

// ── Account helpers (private) ─────────────────────────────────────────────────

/// Return the account with the given `id`, annotated with its transaction
/// count.  Returns `Ok(None)` if no such account exists.
fn get_account_by_id(conn: &Connection, id: &str) -> Result<Option<Account>, MintError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, COUNT(t.id)
         FROM accounts a
         LEFT JOIN transactions t ON t.account_id = a.id AND t.deleted_at IS NULL
         WHERE a.id = ?
         GROUP BY a.id, a.name, a.created_at",
    )?;
    let mut rows = stmt.query(rusqlite::params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Account {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            transaction_count: row.get::<_, i64>(3)? as u64,
        }))
    } else {
        Ok(None)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a fresh in-memory database with migrations applied.
    fn fresh_db() -> Connection {
        super::open_in_memory().expect("open_in_memory")
    }

    // ── ZEB-985: SQLCipher at-rest encryption ────────────────────────────

    const KEY_A: [u8; 32] = [0xA1; 32];
    const KEY_B: [u8; 32] = [0xB2; 32];

    fn read_header(path: &std::path::Path) -> [u8; 16] {
        use std::io::Read;
        let mut f = std::fs::File::open(path).expect("open db file");
        let mut header = [0u8; 16];
        f.read_exact(&mut header).expect("read header");
        header
    }

    #[test]
    fn encrypted_open_creates_ciphertext_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        {
            let conn = open_database(&path, &KEY_A).expect("first open");
            create_account(&conn, "groceries").expect("create account");
        }
        // At rest the file must NOT carry the plaintext SQLite magic.
        assert_ne!(
            &read_header(&path),
            SQLITE_PLAINTEXT_MAGIC,
            "ledger.db is plaintext on disk"
        );
        // Same key round-trips.
        let conn = open_database(&path, &KEY_A).expect("reopen");
        let accounts = list_accounts(&conn).expect("list");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "groceries");
    }

    #[test]
    fn wrong_key_fails_open_without_damaging_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        {
            let conn = open_database(&path, &KEY_A).expect("first open");
            create_account(&conn, "savings").expect("create account");
        }
        assert!(
            open_database(&path, &KEY_B).is_err(),
            "wrong key must fail to open (disarm-mint contract)"
        );
        // The failed attempt must not have clobbered the file.
        let conn = open_database(&path, &KEY_A).expect("correct key still opens");
        assert_eq!(list_accounts(&conn).expect("list").len(), 1);
    }

    #[test]
    fn plaintext_database_migrates_preserving_rows_and_removing_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        // Build the database exactly as the pre-ZEB-985 open did: plain
        // SQLite, WAL journaling, migrations, then a real row.
        {
            let conn = Connection::open(&path).expect("plain open");
            conn.pragma_update(None, "journal_mode", "WAL")
                .expect("wal");
            conn.pragma_update(None, "foreign_keys", "ON").expect("fk");
            apply_migrations(&conn).expect("migrations");
            create_account(&conn, "checking").expect("create account");
        }
        assert_eq!(
            &read_header(&path),
            SQLITE_PLAINTEXT_MAGIC,
            "precondition: pre-migration file is plaintext"
        );
        // Stale sidecars from the plaintext era (zero-length WAL/SHM are
        // valid-and-ignored by SQLite, so they don't perturb the open).
        let wal = dir.path().join("ledger.db-wal");
        let shm = dir.path().join("ledger.db-shm");
        std::fs::write(&wal, b"").expect("stale wal");
        std::fs::write(&shm, b"").expect("stale shm");

        // Keyed open performs the one-time migration.
        {
            let conn = open_database(&path, &KEY_A).expect("migrating open");
            let accounts = list_accounts(&conn).expect("list");
            assert_eq!(accounts.len(), 1, "migrated rows survive");
            assert_eq!(accounts[0].name, "checking");
        }
        assert_ne!(
            &read_header(&path),
            SQLITE_PLAINTEXT_MAGIC,
            "post-migration file is still plaintext"
        );
        assert!(!wal.exists(), "stale plaintext -wal not removed");
        assert!(!shm.exists(), "stale plaintext -shm not removed");
        assert!(
            !path.with_extension("db.enc-migrate").exists(),
            "migration temp file left behind"
        );

        // Second open: migration is one-shot; data still there.
        let conn = open_database(&path, &KEY_A).expect("post-migration reopen");
        assert_eq!(list_accounts(&conn).expect("list").len(), 1);
    }

    #[test]
    fn migration_creates_expected_tables() {
        let conn = fresh_db();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(names.contains(&"accounts".to_string()));
        assert!(names.contains(&"transactions".to_string()));
        assert!(names.contains(&"settings".to_string()));
    }

    #[test]
    fn migration_is_idempotent() {
        // fresh_db() (via super::open_in_memory()) applies migrations once
        // and enables the FK pragma. Call apply_migrations a second time to
        // verify idempotency.
        let conn = fresh_db();
        // Second call must not error.
        apply_migrations(&conn).unwrap();
        // Idempotency means more than "no error" — the seed row must not duplicate.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM settings WHERE key = ?",
                rusqlite::params![DEFAULT_CURRENCY_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "seed row must not duplicate on repeat migration");
    }

    #[test]
    fn default_currency_seeded() {
        let conn = fresh_db();
        let currency = get_default_currency(&conn).unwrap();
        assert_eq!(currency, Some("USD".into()));
    }

    #[test]
    fn set_default_currency_round_trip() {
        let conn = fresh_db();
        set_default_currency(&conn, "JPY").unwrap();
        let currency = get_default_currency(&conn).unwrap();
        assert_eq!(currency, Some("JPY".into()));
    }

    #[test]
    fn set_default_currency_rejects_lowercase() {
        let conn = fresh_db();
        let result = set_default_currency(&conn, "usd");
        assert!(matches!(result, Err(MintError::Validation(_))));
    }

    #[test]
    fn set_default_currency_rejects_too_long() {
        let conn = fresh_db();
        // "USDXYZ" is 6 characters — one over the limit.
        let result = set_default_currency(&conn, "USDXYZ");
        assert!(matches!(result, Err(MintError::Validation(_))));
    }

    #[test]
    fn set_default_currency_rejects_empty() {
        let conn = fresh_db();
        let result = set_default_currency(&conn, "");
        assert!(matches!(result, Err(MintError::Validation(_))));
    }

    #[test]
    fn validate_currency_accepts_valid() {
        for code in &["USD", "JPY", "AUD", "BTC", "UAVF"] {
            assert!(
                validate_currency(code).is_ok(),
                "expected {} to be accepted",
                code
            );
        }
    }

    // ── Account CRUD tests ────────────────────────────────────────────────────

    #[test]
    fn create_account_basic() {
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();
        assert!(!account.id.is_empty(), "id must be a non-empty UUID");
        assert_eq!(account.name, "Chase");
        assert_eq!(account.transaction_count, 0);

        let list = list_accounts(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Chase");
    }

    #[test]
    fn create_account_rejects_empty_name() {
        let conn = fresh_db();
        assert!(matches!(
            create_account(&conn, ""),
            Err(MintError::Validation(_))
        ));
        assert!(matches!(
            create_account(&conn, "   "),
            Err(MintError::Validation(_))
        ));
    }

    #[test]
    fn create_account_rejects_oversized_name() {
        let conn = fresh_db();
        let long_name = "a".repeat(257);
        assert!(matches!(
            create_account(&conn, &long_name),
            Err(MintError::Validation(_))
        ));
    }

    #[test]
    fn create_account_rejects_duplicate_name() {
        let conn = fresh_db();
        create_account(&conn, "Chase").unwrap();
        let err = create_account(&conn, "Chase").unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("already exists")),
            "expected 'already exists' in Validation error, got: {:?}",
            err
        );
    }

    #[test]
    fn list_accounts_empty_initially() {
        let conn = fresh_db();
        let list = list_accounts(&conn).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_accounts_includes_transaction_count() {
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();

        // Insert two raw transactions directly.
        for i in 0..2u32 {
            let tx_id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO transactions \
                 (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    tx_id,
                    "2026-05-19",
                    format!("-{}.00", i + 1),
                    "USD",
                    account.id,
                    format!("test txn {}", i),
                    now,
                    now,
                ],
            )
            .unwrap();
        }

        let list = list_accounts(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].transaction_count, 2);
    }

    #[test]
    fn rename_account_round_trip() {
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();
        let renamed = rename_account(&conn, &account.id, "Chase Checking").unwrap();
        assert_eq!(renamed.name, "Chase Checking");

        let list = list_accounts(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Chase Checking");
    }

    #[test]
    fn rename_account_not_found() {
        let conn = fresh_db();
        let err =
            rename_account(&conn, "00000000-0000-0000-0000-000000000000", "New Name").unwrap_err();
        assert!(matches!(err, MintError::NotFound(_)));
    }

    #[test]
    fn rename_account_rejects_duplicate() {
        let conn = fresh_db();
        let a = create_account(&conn, "A").unwrap();
        create_account(&conn, "B").unwrap();
        let err = rename_account(&conn, &a.id, "B").unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("already exists")),
            "expected 'already exists' in Validation error, got: {:?}",
            err
        );
    }

    #[test]
    fn delete_account_empty_succeeds() {
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();
        delete_account(&conn, &account.id, None).unwrap();

        let list = list_accounts(&conn).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn delete_account_with_txns_no_reassign_fails() {
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();

        let tx_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO transactions \
             (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![tx_id, "2026-05-19", "-10.00", "USD", account.id, "test", now, now],
        )
        .unwrap();

        let err = delete_account(&conn, &account.id, None).unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("has transactions")),
            "expected 'has transactions' in Validation error, got: {:?}",
            err
        );
    }

    #[test]
    fn delete_account_with_only_tombstoned_txns_succeeds() {
        // ZEB-989/CA3: an account whose only transactions are soft-deleted
        // (tombstoned) has no LIVE transactions, so delete-without-reassign is
        // allowed — but the tombstoned rows still reference the account through
        // the enforced FK (no ON DELETE CASCADE). delete_account must purge them
        // first so the account DELETE cannot fail with a foreign-key violation
        // (which, in the command path, would leave a persisted phantom floor).
        let conn = fresh_db();
        let account = create_account(&conn, "Chase").unwrap();

        // Create then soft-delete a transaction, leaving a tombstoned row.
        let txn = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-19".into(),
                amount: "-10.00".into(),
                currency: "USD".into(),
                account_id: account.id.clone(),
                description: "coffee".into(),
                metadata: None,
            },
        )
        .unwrap();
        delete_transaction(&conn, &txn.id).unwrap();

        // The tombstoned row physically remains and still FK-references the account.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = ?",
                params![account.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 1,
            "tombstoned row must still be present pre-delete"
        );

        // Delete without reassign must SUCCEED (previously failed with an FK error).
        delete_account(&conn, &account.id, None).unwrap();

        assert!(
            list_accounts(&conn).unwrap().is_empty(),
            "account must be gone"
        );
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = ?",
                params![account.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphans, 0,
            "tombstoned rows must be purged with the account"
        );
    }

    #[test]
    fn delete_account_with_reassign_moves_transactions() {
        let conn = fresh_db();
        let a = create_account(&conn, "A").unwrap();
        let b = create_account(&conn, "B").unwrap();

        // Insert 3 transactions into A.
        for i in 0..3u32 {
            let tx_id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO transactions \
                 (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    tx_id,
                    "2026-05-19",
                    format!("-{}.00", i + 1),
                    "USD",
                    a.id,
                    format!("txn {}", i),
                    now,
                    now,
                ],
            )
            .unwrap();
        }

        delete_account(&conn, &a.id, Some(&b.id)).unwrap();

        let list = list_accounts(&conn).unwrap();
        assert_eq!(list.len(), 1, "only B should remain");
        assert_eq!(list[0].id, b.id);
        assert_eq!(
            list[0].transaction_count, 3,
            "B should have inherited A's 3 txns"
        );
    }

    #[test]
    fn delete_account_reassign_to_same_id_fails() {
        let conn = fresh_db();
        let a = create_account(&conn, "A").unwrap();
        let err = delete_account(&conn, &a.id, Some(&a.id)).unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("cannot reassign")),
            "expected 'cannot reassign' in Validation error, got: {:?}",
            err
        );
    }

    #[test]
    fn delete_account_reassign_to_missing_target_fails() {
        let conn = fresh_db();
        let a = create_account(&conn, "A").unwrap();

        // Insert a transaction so we'd need reassign_to in normal flow.
        let tx_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO transactions \
             (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![tx_id, "2026-05-19", "-5.00", "USD", a.id, "test", now, now],
        )
        .unwrap();

        let err =
            delete_account(&conn, &a.id, Some("00000000-0000-0000-0000-000000000000")).unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("does not exist")),
            "expected 'does not exist' in Validation error, got: {:?}",
            err
        );
    }

    #[test]
    fn delete_account_not_found() {
        let conn = fresh_db();
        let err = delete_account(&conn, "00000000-0000-0000-0000-000000000000", None).unwrap_err();
        assert!(matches!(err, MintError::NotFound(_)));
    }

    #[test]
    fn delete_account_reassign_bumps_updated_at_on_moved_txns() {
        let conn = fresh_db();
        let a = create_account(&conn, "A").unwrap();
        let b = create_account(&conn, "B").unwrap();
        let t = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-19".into(),
                amount: "10.00".into(),
                currency: "USD".into(),
                account_id: a.id.clone(),
                description: "X".into(),
                metadata: None,
            },
        )
        .unwrap();
        let original_updated_at = t.updated_at.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));
        delete_account(&conn, &a.id, Some(&b.id)).unwrap();
        let after = get_transaction(&conn, &t.id).unwrap().unwrap();
        assert!(
            after.updated_at > original_updated_at,
            "reassigned transaction should have a newer updated_at; before={}, after={}",
            original_updated_at,
            after.updated_at
        );
    }

    // ── validate_date ─────────────────────────────────────────────────────────

    #[test]
    fn validate_date_accepts_iso() {
        assert!(validate_date("2026-05-19").is_ok());
        assert!(validate_date("2000-01-01").is_ok());
        assert!(validate_date("1999-12-31").is_ok());
    }

    #[test]
    fn validate_date_rejects_malformed() {
        assert!(validate_date("26-05-19").is_err(), "two-digit year");
        assert!(validate_date("2026-13-01").is_err(), "month 13");
        assert!(validate_date("").is_err(), "empty");
        assert!(validate_date(" 2026-05-19 ").is_err(), "whitespace");
        assert!(validate_date("2026/05/19").is_err(), "slashes");
    }

    // ── validate_amount ───────────────────────────────────────────────────────

    #[test]
    fn validate_amount_accepts_decimals() {
        assert!(validate_amount("42.50").is_ok());
        assert!(validate_amount("-42.50").is_ok());
        assert!(validate_amount("0").is_ok());
        assert!(validate_amount("0.00001").is_ok());
        assert!(validate_amount("1000000").is_ok());
    }

    #[test]
    fn validate_amount_rejects_nonnumeric() {
        assert!(validate_amount("abc").is_err());
        assert!(validate_amount("4,5").is_err(), "comma as decimal sep");
        assert!(validate_amount("1e5").is_err(), "scientific notation");
        assert!(validate_amount("").is_err(), "empty");
    }

    // ── validate_description ──────────────────────────────────────────────────

    #[test]
    fn validate_description_accepts_normal() {
        assert!(validate_description("Coffee").is_ok());
        assert!(validate_description("A").is_ok());
    }

    #[test]
    fn validate_description_rejects_empty() {
        assert!(validate_description("").is_err());
        assert!(validate_description("   ").is_err(), "all whitespace");
    }

    #[test]
    fn validate_description_rejects_oversized() {
        let big = "x".repeat(4097);
        assert!(validate_description(&big).is_err());
        // Exactly 4096 is still OK.
        let edge = "x".repeat(4096);
        assert!(validate_description(&edge).is_ok());
    }

    // ── validate_metadata ─────────────────────────────────────────────────────

    #[test]
    fn validate_metadata_accepts_json() {
        assert!(validate_metadata("{}").is_ok());
        assert!(validate_metadata(r#"{"tag":"travel"}"#).is_ok());
        assert!(validate_metadata("null").is_ok());
        assert!(validate_metadata("[]").is_ok());
    }

    #[test]
    fn validate_metadata_rejects_malformed() {
        assert!(validate_metadata("not json").is_err());
        assert!(validate_metadata("{").is_err());
    }

    #[test]
    fn validate_metadata_rejects_oversized() {
        // 65_537 bytes — just over the 64 KiB limit.
        let big = "x".repeat(65_537);
        assert!(validate_metadata(&big).is_err());
    }

    // ── create_transaction ────────────────────────────────────────────────────

    /// Helper: create a test account named `name` and return its id.
    fn make_account(conn: &Connection, name: &str) -> String {
        create_account(conn, name).unwrap().id
    }

    /// Helper: build a minimal valid NewTransaction for `account_id`.
    fn make_new_tx(account_id: &str) -> NewTransaction {
        NewTransaction {
            transaction_date: "2026-05-19".into(),
            amount: "-42.50".into(),
            currency: "USD".into(),
            account_id: account_id.to_string(),
            description: "Coffee".into(),
            metadata: None,
        }
    }

    #[test]
    fn create_transaction_happy_path() {
        let conn = fresh_db();
        let acc_id = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc_id)).unwrap();
        assert!(!tx.id.is_empty());
        assert_eq!(tx.transaction_date, "2026-05-19");
        assert_eq!(tx.amount, "-42.50");
        assert_eq!(tx.currency, "USD");
        assert_eq!(tx.account_id, acc_id);
        assert_eq!(tx.account_name, "Chase");
        assert_eq!(tx.description, "Coffee");
        assert!(tx.metadata.is_none());
    }

    #[test]
    fn create_transaction_rejects_missing_account() {
        let conn = fresh_db();
        let payload = NewTransaction {
            account_id: "00000000-0000-0000-0000-000000000000".into(),
            ..make_new_tx("00000000-0000-0000-0000-000000000000")
        };
        let err = create_transaction(&conn, payload).unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("account does not exist")),
            "got: {:?}",
            err
        );
    }

    #[test]
    fn create_transaction_rejects_invalid_date() {
        let conn = fresh_db();
        let acc_id = make_account(&conn, "Chase");
        let payload = NewTransaction {
            transaction_date: "bad-date".into(),
            ..make_new_tx(&acc_id)
        };
        assert!(matches!(
            create_transaction(&conn, payload),
            Err(MintError::Validation(_))
        ));
    }

    #[test]
    fn create_transaction_rejects_invalid_amount() {
        let conn = fresh_db();
        let acc_id = make_account(&conn, "Chase");
        let payload = NewTransaction {
            amount: "not-a-number".into(),
            ..make_new_tx(&acc_id)
        };
        assert!(matches!(
            create_transaction(&conn, payload),
            Err(MintError::Validation(_))
        ));
    }

    #[test]
    fn create_transaction_rejects_invalid_currency() {
        let conn = fresh_db();
        let acc_id = make_account(&conn, "Chase");
        let payload = NewTransaction {
            currency: "usd".into(), // lowercase — invalid
            ..make_new_tx(&acc_id)
        };
        assert!(matches!(
            create_transaction(&conn, payload),
            Err(MintError::Validation(_))
        ));
    }

    #[test]
    fn create_transaction_rejects_invalid_metadata() {
        let conn = fresh_db();
        let acc_id = make_account(&conn, "Chase");
        let payload = NewTransaction {
            metadata: Some("not valid json".into()),
            ..make_new_tx(&acc_id)
        };
        assert!(matches!(
            create_transaction(&conn, payload),
            Err(MintError::Validation(_))
        ));
    }

    // ── get_transaction ───────────────────────────────────────────────────────

    #[test]
    fn get_transaction_returns_none_for_missing() {
        let conn = fresh_db();
        let result = get_transaction(&conn, "00000000-0000-0000-0000-000000000000").unwrap();
        assert!(result.is_none());
    }

    // ── list_transactions ─────────────────────────────────────────────────────

    #[test]
    fn list_transactions_empty() {
        let conn = fresh_db();
        let result = list_transactions(&conn, &ListFilter::default()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn list_transactions_filter_date_from() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");

        let _t1 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-10".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        let t2 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-15".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        let t3 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-20".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();

        let results = list_transactions(
            &conn,
            &ListFilter {
                date_from: Some("2026-05-15".into()),
                ..ListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<_> = results.iter().map(|t| t.id.clone()).collect();
        assert!(ids.contains(&t2.id));
        assert!(ids.contains(&t3.id));
    }

    #[test]
    fn list_transactions_filter_date_to() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");

        let t1 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-10".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        let _t2 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-20".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();

        let results = list_transactions(
            &conn,
            &ListFilter {
                date_to: Some("2026-05-15".into()),
                ..ListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, t1.id);
    }

    #[test]
    fn list_transactions_filter_account() {
        let conn = fresh_db();
        let a = make_account(&conn, "A");
        let b = make_account(&conn, "B");

        let ta = create_transaction(&conn, make_new_tx(&a)).unwrap();
        let _tb = create_transaction(&conn, make_new_tx(&b)).unwrap();

        let results = list_transactions(
            &conn,
            &ListFilter {
                account_id: Some(a.clone()),
                ..ListFilter::default()
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, ta.id);
    }

    #[test]
    fn list_transactions_filter_all_three_combined() {
        let conn = fresh_db();
        let a = make_account(&conn, "A");
        let b = make_account(&conn, "B");

        // t1: account A, 2026-05-10 — outside date_from
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-10".into(),
                ..make_new_tx(&a)
            },
        )
        .unwrap();
        // t2: account A, 2026-05-16 — inside range
        let t2 = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-16".into(),
                ..make_new_tx(&a)
            },
        )
        .unwrap();
        // t3: account B, 2026-05-16 — wrong account
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-16".into(),
                ..make_new_tx(&b)
            },
        )
        .unwrap();
        // t4: account A, 2026-05-25 — outside date_to
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-25".into(),
                ..make_new_tx(&a)
            },
        )
        .unwrap();

        let results = list_transactions(
            &conn,
            &ListFilter {
                date_from: Some("2026-05-15".into()),
                date_to: Some("2026-05-20".into()),
                account_id: Some(a.clone()),
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, t2.id);
    }

    #[test]
    fn list_transactions_order_date_desc() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");

        // Two transactions on the same date — both should appear before the early one.
        let t_a = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-19".into(),
                description: "First".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        let t_b = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-19".into(),
                description: "Second".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        // And one with an earlier date.
        let t_early = create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                description: "Early".into(),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();

        let results = list_transactions(&conn, &ListFilter::default()).unwrap();
        assert_eq!(results.len(), 3);
        // Most recent date first — earliest date must be last.
        assert_eq!(results[2].id, t_early.id, "earliest date should be last");
        // Both same-date txns should appear in the top two slots.
        let pos_a = results.iter().position(|t| t.id == t_a.id).unwrap();
        let pos_b = results.iter().position(|t| t.id == t_b.id).unwrap();
        assert!(pos_a < 2, "same-date tx A should appear in top 2");
        assert!(pos_b < 2, "same-date tx B should appear in top 2");
    }

    #[test]
    fn list_transactions_order_secondary_id_desc() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let now = chrono::Utc::now().to_rfc3339();

        // Hand-crafted IDs: "bbbb..." sorts lexicographically after "aaaa...",
        // so with ORDER BY t.id DESC, "bbbb..." must appear first.
        let id_lo = "aaaaaaaa-0000-0000-0000-000000000000";
        let id_hi = "bbbbbbbb-0000-0000-0000-000000000000";

        for id in &[id_lo, id_hi] {
            conn.execute(
                "INSERT INTO transactions \
                 (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, "2026-05-19", "-1.00", "USD", acc, "det", now, now],
            )
            .unwrap();
        }

        let results = list_transactions(&conn, &ListFilter::default()).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, id_hi, "higher id should sort first (DESC)");
        assert_eq!(results[1].id, id_lo, "lower id should sort second (DESC)");
    }

    // ── update_transaction ────────────────────────────────────────────────────

    #[test]
    fn update_transaction_single_field() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc)).unwrap();

        let updated = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                amount: Some("-99.99".into()),
                ..UpdateTransaction::default()
            },
        )
        .unwrap();
        assert_eq!(updated.amount, "-99.99");
        // Other fields unchanged.
        assert_eq!(updated.description, tx.description);
        assert_eq!(updated.transaction_date, tx.transaction_date);
    }

    #[test]
    fn update_transaction_metadata_set() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc)).unwrap();

        let updated = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                metadata: Some(Some(r#"{"tag":"food"}"#.into())),
                ..UpdateTransaction::default()
            },
        )
        .unwrap();
        assert_eq!(updated.metadata.as_deref(), Some(r#"{"tag":"food"}"#));
    }

    #[test]
    fn update_transaction_metadata_clear() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(
            &conn,
            NewTransaction {
                metadata: Some(r#"{"tag":"food"}"#.into()),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();
        assert!(
            tx.metadata.is_some(),
            "metadata must be set before clearing"
        );

        let updated = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                metadata: Some(None), // Set to NULL.
                ..UpdateTransaction::default()
            },
        )
        .unwrap();
        assert!(
            updated.metadata.is_none(),
            "metadata should be cleared to NULL"
        );
    }

    #[test]
    fn update_transaction_metadata_untouched() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(
            &conn,
            NewTransaction {
                metadata: Some(r#"{"tag":"food"}"#.into()),
                ..make_new_tx(&acc)
            },
        )
        .unwrap();

        // Update with metadata = None (absent) — should leave value alone.
        let updated = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                amount: Some("-1.00".into()),
                metadata: None, // absent — leave alone
                ..UpdateTransaction::default()
            },
        )
        .unwrap();
        assert_eq!(
            updated.metadata.as_deref(),
            Some(r#"{"tag":"food"}"#),
            "metadata should be unchanged"
        );
    }

    #[test]
    fn update_transaction_rejects_invalid_account_id() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc)).unwrap();

        let err = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                account_id: Some("00000000-0000-0000-0000-000000000000".into()),
                ..UpdateTransaction::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, MintError::Validation(ref s) if s.contains("account does not exist")),
            "got: {:?}",
            err
        );
    }

    #[test]
    fn update_transaction_not_found() {
        let conn = fresh_db();
        let err = update_transaction(
            &conn,
            "00000000-0000-0000-0000-000000000000",
            UpdateTransaction {
                amount: Some("-5.00".into()),
                ..UpdateTransaction::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, MintError::NotFound(_)));
    }

    #[test]
    fn update_transaction_bumps_updated_at() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc)).unwrap();
        let original_updated_at = tx.updated_at.clone();

        // Sleep 10 ms so the RFC 3339 timestamp is guaranteed to advance.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let updated = update_transaction(
            &conn,
            &tx.id,
            UpdateTransaction {
                amount: Some("-1.00".into()),
                ..UpdateTransaction::default()
            },
        )
        .unwrap();
        assert!(
            updated.updated_at > original_updated_at,
            "updated_at should be bumped; was {} now {}",
            original_updated_at,
            updated.updated_at
        );
    }

    // ── delete_transaction ────────────────────────────────────────────────────

    #[test]
    fn delete_transaction_happy_path() {
        let conn = fresh_db();
        let acc = make_account(&conn, "Chase");
        let tx = create_transaction(&conn, make_new_tx(&acc)).unwrap();

        delete_transaction(&conn, &tx.id).unwrap();

        let result = get_transaction(&conn, &tx.id).unwrap();
        assert!(result.is_none(), "transaction should be deleted");
    }

    #[test]
    fn delete_transaction_not_found() {
        let conn = fresh_db();
        let err = delete_transaction(&conn, "00000000-0000-0000-0000-000000000000").unwrap_err();
        assert!(matches!(err, MintError::NotFound(_)));
    }
}
