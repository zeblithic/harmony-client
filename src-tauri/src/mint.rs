//! Mint — personal-finance transaction tracker backend.
//!
//! This module owns the SQLite layer for the Mint feature: schema
//! migration, database connection lifecycle, and settings management.
//! Account and transaction CRUD are added in subsequent tasks.
//!
//! Spec: `docs/specs/2026-05-19-mint-mvp-design.md`
//! Plan: `docs/plans/2026-05-19-mint-mvp-plan.md`

use rusqlite::{params, Connection};

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
/// can be called on every app start without error.  Task 2 will add the
/// `UNIQUE(name)` constraint on `accounts` via an `ALTER TABLE` migration.
fn apply_migrations(conn: &Connection) -> Result<(), MintError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS accounts (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
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

        INSERT OR IGNORE INTO settings (key, value) VALUES ('default_currency', 'USD');
        ",
    )?;
    Ok(())
}

// ── Settings ──────────────────────────────────────────────────────────────────

/// Read the current default currency from the settings table.
///
/// Returns `None` if the row has been deleted (unlikely in normal operation
/// because `apply_migrations` seeds it, but defensive is correct here).
pub fn get_default_currency(conn: &Connection) -> Result<Option<String>, MintError> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = 'default_currency'")?;
    let mut rows = stmt.query([])?;
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
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('default_currency', ?)",
        params![currency],
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
}
