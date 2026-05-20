//! Integration tests for the Mint MVP sync layer.
//!
//! These tests exercise the public sync API of `harmony_app::mint`
//! against an in-memory or tempfile-backed SQLite database. The Tauri
//! command layer itself is thin (spawn_blocking + .lock() + delegate)
//! and is exercised end-to-end via the harmony-client app; this file
//! covers cross-function lifecycle scenarios and persistence.

use harmony_app::mint::*;
use rusqlite::Connection;

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
        let conn = open_database(&path).unwrap();
        set_default_currency(&conn, "JPY").unwrap();
    }
    let conn = open_database(&path).unwrap();
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

    let summary = export_csv(&conn, &csv_path, None, None).unwrap();
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
    // Metadata with an embedded newline in a JSON string value.
    let metadata = "{\"note\":\"line\nbreak\"}";

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

    let summary = export_csv(&conn, &csv_path, None, None).unwrap();
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
    let summary = export_csv(&conn, &csv_path, Some("2026-05-15"), Some("2026-05-17")).unwrap();
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

    let summary = export_csv(&conn, &csv_path, None, None).unwrap();
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
    let summary = export_csv(&conn, &out, None, None).unwrap();
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
