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

/// Open (or create) the Mint SQLite database at `path`.
///
/// Applies WAL journaling and foreign-key enforcement pragmas, then runs
/// `apply_migrations` to ensure the schema is up to date.  The caller is
/// responsible for placing the file inside the app-data directory; this
/// function only requires that the *parent directory* already exists.
pub fn open_database(path: &std::path::Path) -> Result<Connection, MintError> {
    // Ensure parent directory exists (create_dir_all is a no-op if already
    // present, and propagates Io errors via the #[from] impl above).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
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
fn apply_migrations(conn: &Connection) -> Result<(), MintError> {
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
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
        params![DEFAULT_CURRENCY_KEY, currency],
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
    let created_at = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO accounts (id, name, created_at) VALUES (?, ?, ?)",
        params![id, trimmed_name, created_at],
    )
    .map_err(map_account_name_constraint)?;
    Ok(Account {
        id,
        name: trimmed_name,
        created_at,
        transaction_count: 0,
    })
}

/// Return all accounts ordered case-insensitively by name, each annotated with
/// the number of transactions posted to it.
pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>, MintError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, COUNT(t.id) AS tx_count
         FROM accounts a
         LEFT JOIN transactions t ON t.account_id = a.id
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
    let affected = conn
        .execute(
            "UPDATE accounts SET name = ? WHERE id = ?",
            params![trimmed_name, id],
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
            "SELECT COUNT(*) FROM transactions WHERE account_id = ?",
            params![id],
            |r| r.get(0),
        )?;
        if count > 0 {
            return Err(MintError::Validation(
                "account has transactions; pass reassign_to".into(),
            ));
        }
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    if let Some(target) = reassign_to {
        tx.execute(
            "UPDATE transactions SET account_id = ? WHERE account_id = ?",
            params![target, id],
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

/// Return the account with the given `id`, annotated with its transaction
/// count.  Returns `Ok(None)` if no such account exists.
fn get_account_by_id(conn: &Connection, id: &str) -> Result<Option<Account>, MintError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, COUNT(t.id)
         FROM accounts a
         LEFT JOIN transactions t ON t.account_id = a.id
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
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        conn
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
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
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
}
