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
    let conn = Connection::open_in_memory().expect("open in_memory");
    apply_migrations(&conn).expect("apply_migrations");
    conn
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
