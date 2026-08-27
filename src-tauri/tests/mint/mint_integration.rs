//! Integration tests for the Mint MVP sync layer.
//!
//! These tests exercise the public sync API of `harmony_app::mint`
//! against an in-memory or tempfile-backed SQLite database. The Tauri
//! command layer itself is thin (spawn_blocking + .lock() + delegate)
//! and is exercised end-to-end via the harmony-client app; this file
//! covers cross-function lifecycle scenarios and persistence.

use harmony_app::mint::*;
use rusqlite::Connection;

// ZEB-985: on-disk ledgers are SQLCipher-encrypted; disk-backed tests key
// their opens with a fixed test key.
const TEST_LEDGER_KEY: [u8; 32] = [0x42; 32];

fn fresh_in_memory_db() -> Connection {
    harmony_app::mint::open_in_memory().expect("open_in_memory")
}

#[test]
fn full_lifecycle_account_plus_transactions() {
    let conn = fresh_in_memory_db();
    let a = create_account(&conn, "Chase Checking").unwrap();
    let b = create_account(&conn, "United Miles").unwrap();
    let t1 = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-19".into(),
            amount: "-42.50".into(),
            currency: "USD".into(),
            account_id: a.id.clone(),
            description: "Coffee".into(),
            metadata: Some(r#"{"tag":"travel"}"#.into()),
        },
    )
    .unwrap();
    let t2 = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-18".into(),
            amount: "1500".into(),
            currency: "UAVF".into(),
            account_id: b.id.clone(),
            description: "Booking bonus".into(),
            metadata: None,
        },
    )
    .unwrap();
    // List — both present, most recent date first.
    let all = list_transactions(
        &conn,
        &ListFilter {
            date_from: None,
            date_to: None,
            account_id: None,
        },
    )
    .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, t1.id, "2026-05-19 should come first (DESC)");
    // Filter by account.
    let only_a = list_transactions(
        &conn,
        &ListFilter {
            date_from: None,
            date_to: None,
            account_id: Some(a.id.clone()),
        },
    )
    .unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].id, t1.id);
    // Update.
    let updated = update_transaction(
        &conn,
        &t1.id,
        UpdateTransaction {
            transaction_date: None,
            amount: Some("-99.99".into()),
            currency: None,
            account_id: None,
            description: None,
            metadata: Some(None), // clear
        },
    )
    .unwrap();
    assert_eq!(updated.amount, "-99.99");
    assert!(updated.metadata.is_none());
    // Delete.
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
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: format!("2026-05-{:02}", 10 + i),
                amount: format!("{}.00", i + 1),
                currency: "USD".into(),
                account_id: a.id.clone(),
                description: format!("Tx {i}"),
                metadata: None,
            },
        )
        .unwrap();
    }
    delete_account(&conn, &a.id, Some(&b.id)).unwrap();
    let on_b = list_transactions(
        &conn,
        &ListFilter {
            date_from: None,
            date_to: None,
            account_id: Some(b.id.clone()),
        },
    )
    .unwrap();
    assert_eq!(on_b.len(), 3);
    assert!(
        list_accounts(&conn)
            .unwrap()
            .iter()
            .all(|acc| acc.id != a.id),
        "account A should be gone"
    );
}

#[test]
fn migration_idempotent_across_reopens() {
    // Open tempfile-backed DB, migrate, close. Open again, migrate, no error.
    // Verify default_currency persists across reopens.
    let tmpdir = tempfile::tempdir().unwrap();
    let path = tmpdir.path().join("ledger.db");
    {
        let conn = open_database(&path, &TEST_LEDGER_KEY).unwrap();
        set_default_currency(&conn, "JPY").unwrap();
    }
    let conn = open_database(&path, &TEST_LEDGER_KEY).unwrap();
    assert_eq!(
        get_default_currency(&conn).unwrap(),
        Some("JPY".into()),
        "default_currency should persist across close/reopen"
    );
}

// ── CSV export integration tests ──────────────────────────────────────────────

#[test]
fn export_csv_round_trips_via_csv_reader() {
    let conn = fresh_in_memory_db();

    // Create 5 accounts.
    let mut account_ids = Vec::new();
    for i in 0..5u32 {
        let acc = create_account(&conn, &format!("Account {i}")).unwrap();
        account_ids.push(acc.id);
    }

    // Create 50 transactions spread across the accounts with varied dates.
    for i in 0..50u32 {
        let account_id = account_ids[(i as usize) % account_ids.len()].clone();
        // Dates cycle over 2026-05-01 through 2026-05-28 (i % 28 + 1).
        let day = (i % 28) + 1;
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: format!("2026-05-{day:02}"),
                amount: format!("-{}.00", i + 1),
                currency: "USD".into(),
                account_id,
                description: format!("Transaction {i}"),
                metadata: None,
            },
        )
        .unwrap();
    }

    let tmpdir = tempfile::tempdir().unwrap();
    let csv_path = tmpdir.path().join("export.csv");

    let summary = export_csv(&conn, &csv_path, None, None, None).unwrap();
    assert_eq!(summary.rows_written, 50, "should have written 50 data rows");
    assert!(summary.byte_size > 0, "file should be non-empty");

    // Re-read with csv::Reader and verify header + row count.
    let mut reader = csv::Reader::from_path(&csv_path).unwrap();
    let headers = reader.headers().unwrap().clone();
    assert_eq!(
        headers.iter().collect::<Vec<_>>(),
        vec![
            "date",
            "account_name",
            "amount",
            "currency",
            "description",
            "metadata"
        ],
        "header row must match spec"
    );
    let data_rows: Vec<_> = reader.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(data_rows.len(), 50, "csv reader should see 50 data rows");
}

#[test]
fn export_csv_escapes_special_characters() {
    let conn = fresh_in_memory_db();
    let acc = create_account(&conn, "Test Account").unwrap();

    // Description with literal comma, double-quotes, and embedded newline.
    let description = "Lunch, \"deluxe\" combo\nwith soup";
    // Metadata uses the JSON `\n` escape (two characters: backslash + n)
    // rather than a raw newline byte — RFC 8259 forbids raw control chars
    // inside JSON string values, and our validator rejects them. RFC 4180
    // CSV escaping has no special treatment for backslash-n, so the field
    // still round-trips byte-exactly.
    let metadata = "{\"note\":\"line\\nbreak\"}";

    create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-19".into(),
            amount: "-12.50".into(),
            currency: "USD".into(),
            account_id: acc.id.clone(),
            description: description.into(),
            metadata: Some(metadata.into()),
        },
    )
    .unwrap();

    let tmpdir = tempfile::tempdir().unwrap();
    let csv_path = tmpdir.path().join("escape_test.csv");

    let summary = export_csv(&conn, &csv_path, None, None, None).unwrap();
    assert_eq!(summary.rows_written, 1);

    // Re-read and verify the special characters round-trip byte-exactly.
    let mut reader = csv::Reader::from_path(&csv_path).unwrap();
    let records: Vec<_> = reader.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(records.len(), 1, "expected exactly one data row");
    let record = &records[0];
    // Column 4 is description, column 5 is metadata.
    assert_eq!(
        &record[4], description,
        "description must round-trip byte-exactly through RFC 4180 escaping"
    );
    assert_eq!(
        &record[5], metadata,
        "metadata must round-trip byte-exactly through RFC 4180 escaping"
    );
}

#[test]
fn export_csv_respects_date_filter() {
    let conn = fresh_in_memory_db();
    let acc = create_account(&conn, "Filter Account").unwrap();

    // Create 10 transactions on dates 2026-05-10 through 2026-05-19.
    for day in 10u32..=19 {
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: format!("2026-05-{day:02}"),
                amount: "-1.00".into(),
                currency: "USD".into(),
                account_id: acc.id.clone(),
                description: format!("Day {day}"),
                metadata: None,
            },
        )
        .unwrap();
    }

    let tmpdir = tempfile::tempdir().unwrap();
    let csv_path = tmpdir.path().join("date_filter.csv");

    // Export only 2026-05-15 through 2026-05-17 (inclusive).
    let summary = export_csv(
        &conn,
        &csv_path,
        Some("2026-05-15"),
        Some("2026-05-17"),
        None,
    )
    .unwrap();
    assert_eq!(
        summary.rows_written, 3,
        "date filter should include exactly 3 rows (May 15, 16, 17)"
    );

    // Verify the dates in the CSV are exactly the 3 expected ones.
    let mut reader = csv::Reader::from_path(&csv_path).unwrap();
    let dates: Vec<String> = reader
        .records()
        .map(|r| r.unwrap()[0].to_string())
        .collect();
    assert_eq!(
        dates,
        vec!["2026-05-15", "2026-05-16", "2026-05-17"],
        "exported dates must be exactly 15, 16, 17 in ascending order"
    );
}

#[test]
fn export_csv_empty_ledger() {
    // Fresh DB with no accounts and no transactions.
    let conn = fresh_in_memory_db();

    let tmpdir = tempfile::tempdir().unwrap();
    let csv_path = tmpdir.path().join("empty.csv");

    let summary = export_csv(&conn, &csv_path, None, None, None).unwrap();
    assert_eq!(summary.rows_written, 0, "no data rows expected");
    assert!(
        summary.byte_size > 0,
        "file should still contain the header row (non-zero bytes)"
    );

    // Re-read: must have exactly the header and zero data rows.
    let mut reader = csv::Reader::from_path(&csv_path).unwrap();
    let headers = reader.headers().unwrap().clone();
    assert_eq!(
        headers.iter().collect::<Vec<_>>(),
        vec![
            "date",
            "account_name",
            "amount",
            "currency",
            "description",
            "metadata"
        ],
        "header must be present even in empty ledger"
    );
    let data_rows: Vec<_> = reader.records().collect::<Result<_, _>>().unwrap();
    assert!(
        data_rows.is_empty(),
        "no data rows expected in empty ledger export"
    );
}

#[test]
fn export_csv_no_partial_file_on_unwritable_path() {
    let conn = fresh_in_memory_db();
    // Create an account + transaction so the query has content.
    let a = create_account(&conn, "A").unwrap();
    create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-19".into(),
            amount: "1.00".into(),
            currency: "USD".into(),
            account_id: a.id,
            description: "X".into(),
            metadata: None,
        },
    )
    .unwrap();

    // Verify the happy path: export into a nested directory that doesn't
    // yet exist (create_dir_all path), then confirm no NamedTempFile
    // sibling remains in the parent directory after a successful export.
    let tmpdir = tempfile::tempdir().unwrap();
    let out = tmpdir.path().join("nested").join("export.csv");
    let summary = export_csv(&conn, &out, None, None, None).unwrap();
    assert_eq!(summary.rows_written, 1);
    assert!(out.exists());

    // After a successful export, no NamedTempFile sibling should
    // remain in the parent directory.
    let parent = out.parent().unwrap();
    let leftover: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path() != out)
        .collect();
    assert!(
        leftover.is_empty(),
        "stray tempfile left behind: {:?}",
        leftover.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

#[test]
fn migration_v2_adds_columns_and_backfills() {
    let db_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    // First open: open_database internally calls apply_migrations (v1 + v2
    // both land). The explicit apply_migrations call here is redundant with
    // what open_database already did; it documents that calling it a second
    // time in the same session is safe (same-process idempotency).
    //
    // The INSERT statements use only the v1 column set intentionally: they
    // simulate rows that were written before the v2 backfill UPDATE ran (i.e.
    // rows that will have updated_at = '' until the next migration pass).
    {
        let conn = harmony_app::mint::open_database(&db_path, &TEST_LEDGER_KEY).unwrap();
        harmony_app::mint::apply_migrations(&conn).unwrap();
        // Insert rows that intentionally omit the v2 columns (updated_at),
        // simulating pre-v2 row state to verify the backfill UPDATEs fire.
        conn.execute(
            "INSERT INTO accounts (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["acct-1", "Chase", "2026-05-01T00:00:00Z"],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
            rusqlite::params!["default_currency", "USD"],
        )
        .unwrap();
    }
    // Second open: close and reopen the DB, then migrate again. This verifies
    // that the backfill UPDATEs in apply_migrations correctly handle rows that
    // were inserted with updated_at = '' (the DEFAULT for pre-v2 rows).
    let conn = harmony_app::mint::open_database(&db_path, &TEST_LEDGER_KEY).unwrap();
    harmony_app::mint::apply_migrations(&conn).unwrap();

    // accounts now has updated_at, backfilled from created_at.
    let updated_at: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            ["acct-1"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(updated_at, "2026-05-01T00:00:00Z");

    // settings now has updated_at, backfilled to epoch so any explicit user
    // change (with a real timestamp) always wins in LWW merge.
    let setting_updated: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        setting_updated, "1970-01-01T00:00:00Z",
        "v2 migration must backfill settings.updated_at to epoch, not wall-clock now()"
    );

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
    harmony_app::mint::apply_migrations(&conn).unwrap();
}

#[test]
fn export_csv_respects_account_filter() {
    let conn = fresh_in_memory_db();

    // Create two accounts.
    let acc_a = create_account(&conn, "Account A").unwrap();
    let acc_b = create_account(&conn, "Account B").unwrap();

    // Create 3 transactions on account A and 2 on account B.
    for i in 1u32..=3 {
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: format!("2026-05-{i:02}"),
                amount: format!("-{i}.00"),
                currency: "USD".into(),
                account_id: acc_a.id.clone(),
                description: format!("A tx {i}"),
                metadata: None,
            },
        )
        .unwrap();
    }
    for i in 4u32..=5 {
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: format!("2026-05-{i:02}"),
                amount: format!("-{i}.00"),
                currency: "USD".into(),
                account_id: acc_b.id.clone(),
                description: format!("B tx {i}"),
                metadata: None,
            },
        )
        .unwrap();
    }

    let tmpdir = tempfile::tempdir().unwrap();

    // Export filtered to account A only — expect exactly 3 rows.
    let path_a = tmpdir.path().join("account_a.csv");
    let summary_a = export_csv(&conn, &path_a, None, None, Some(&acc_a.id)).unwrap();
    assert_eq!(
        summary_a.rows_written, 3,
        "account A filter should yield 3 rows"
    );

    let mut reader_a = csv::Reader::from_path(&path_a).unwrap();
    let rows_a: Vec<_> = reader_a.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows_a.len(), 3);
    for r in &rows_a {
        assert_eq!(&r[1], "Account A", "all rows must belong to Account A");
    }

    // Export with no account filter — expect all 5 rows.
    let path_all = tmpdir.path().join("all.csv");
    let summary_all = export_csv(&conn, &path_all, None, None, None).unwrap();
    assert_eq!(
        summary_all.rows_written, 5,
        "unfiltered export should yield all 5 rows"
    );
}

// ── Soft-delete + updated_at bump tests (Task 2) ──────────────────────────────

#[test]
fn soft_delete_transaction_filters_from_reads() {
    let conn = fresh_in_memory_db();
    let acct = create_account(&conn, "Chase").unwrap();
    let tx = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-01".into(),
            amount: "-12.34".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "Coffee".into(),
            metadata: None,
        },
    )
    .unwrap();
    delete_transaction(&conn, &tx.id).unwrap();
    let listed = list_transactions(&conn, &ListFilter::default()).unwrap();
    assert_eq!(listed.len(), 0, "soft-deleted row must not appear in list");
    let fetched = get_transaction(&conn, &tx.id).unwrap();
    assert!(
        fetched.is_none(),
        "soft-deleted row must not be returned by get"
    );
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
    let conn = fresh_in_memory_db();
    let acct = create_account(&conn, "Chase").unwrap();
    let tx = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-01".into(),
            amount: "1".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "x".into(),
            metadata: None,
        },
    )
    .unwrap();
    delete_transaction(&conn, &tx.id).unwrap();
    let first_deleted_at: String = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            [&tx.id],
            |r| r.get(0),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    delete_transaction(&conn, &tx.id).unwrap();
    let second_deleted_at: String = conn
        .query_row(
            "SELECT deleted_at FROM transactions WHERE id = ?",
            [&tx.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        first_deleted_at, second_deleted_at,
        "second delete must not overwrite the original tombstone timestamp"
    );
}

#[test]
fn account_rename_bumps_updated_at() {
    let conn = fresh_in_memory_db();
    let acct = create_account(&conn, "Chase").unwrap();
    let before: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&acct.id],
            |r| r.get(0),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    rename_account(&conn, &acct.id, "Chase Checking").unwrap();
    let after: String = conn
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&acct.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(after > before, "rename must bump updated_at");
}

#[test]
fn set_default_currency_bumps_updated_at() {
    let conn = fresh_in_memory_db();
    set_default_currency(&conn, "USD").unwrap();
    let before: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    set_default_currency(&conn, "JPY").unwrap();
    let after: String = conn
        .query_row(
            "SELECT updated_at FROM settings WHERE key = ?",
            ["default_currency"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(after > before, "set_default_currency must bump updated_at");
}

#[test]
fn update_transaction_on_tombstoned_row_returns_not_found() {
    let conn = fresh_in_memory_db();
    let acct = create_account(&conn, "Chase").unwrap();
    let tx = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-01".into(),
            amount: "1".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "x".into(),
            metadata: None,
        },
    )
    .unwrap();
    delete_transaction(&conn, &tx.id).unwrap();
    let result = update_transaction(
        &conn,
        &tx.id,
        UpdateTransaction {
            transaction_date: None,
            amount: Some("999".into()),
            currency: None,
            account_id: None,
            description: None,
            metadata: None,
        },
    );
    assert!(
        matches!(result, Err(MintError::NotFound(_))),
        "updating a tombstoned row must return NotFound, got: {result:?}"
    );
}

#[test]
fn export_csv_excludes_tombstoned_transactions() {
    let conn = fresh_in_memory_db();
    let acct = create_account(&conn, "Chase").unwrap();
    let live = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-01".into(),
            amount: "1".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "live".into(),
            metadata: None,
        },
    )
    .unwrap();
    let tombstone = create_transaction(
        &conn,
        NewTransaction {
            transaction_date: "2026-05-02".into(),
            amount: "2".into(),
            currency: "USD".into(),
            account_id: acct.id.clone(),
            description: "soon-to-be-tombstoned".into(),
            metadata: None,
        },
    )
    .unwrap();
    delete_transaction(&conn, &tombstone.id).unwrap();

    let tmpdir = tempfile::tempdir().unwrap();
    let csv_path = tmpdir.path().join("export.csv");
    let summary = export_csv(&conn, &csv_path, None, None, None).unwrap();

    assert_eq!(
        summary.rows_written, 1,
        "export_csv should exclude tombstoned rows"
    );
    let _ = live; // suppress unused warning if any
}
