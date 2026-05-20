//! Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.

// These functions are the Task 4 public API consumed by Tasks 7+ (engine
// scaffold). They are intentionally not called yet; suppress dead_code until
// the engine wires them in.
#![allow(dead_code)]

use crate::mint_sync_types::{AccountRow, MintSnapshot, MintSyncError, SettingRow, TransactionRow};
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
/// **Caller contract:** the caller must verify
/// `remote.schema_version <= LOCAL_MAX_SCHEMA_VERSION` BEFORE calling this
/// function. The subscriber (Task 9) does this check; direct callers
/// (tests, future code paths) must replicate it. This function does NOT
/// re-check schema_version — silent application of an unknown version
/// risks silent data corruption.
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

fn upsert_setting_lww(tx: &rusqlite::Transaction, r: &SettingRow) -> Result<(), MintSyncError> {
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
    use crate::mint::open_in_memory;

    fn fresh_db() -> Connection {
        // open_in_memory() already runs apply_migrations internally.
        open_in_memory().unwrap()
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
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| {
                r.get(0)
            })
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
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| {
                r.get(0)
            })
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
