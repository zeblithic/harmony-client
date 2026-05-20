//! Two-engine convergence tests for Mint Phase 2 sync.

use harmony_app::content_store::InMemoryStub;
use harmony_app::mint::{
    create_account, create_transaction, delete_transaction, open_in_memory, set_default_currency,
    update_transaction, NewTransaction, UpdateTransaction,
};
use harmony_app::mint_sync::MintSyncEngine;
use harmony_app::mint_sync_types::MintSyncState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

struct Harness {
    conn_a: Arc<std::sync::Mutex<rusqlite::Connection>>,
    conn_b: Arc<std::sync::Mutex<rusqlite::Connection>>,
    cs: Arc<InMemoryStub>,
    engine_a: MintSyncEngine,
    engine_b: MintSyncEngine,
    handle_a: harmony_app::mint_sync::MintSyncEngineHandle,
    handle_b: harmony_app::mint_sync::MintSyncEngineHandle,
}

async fn setup() -> Harness {
    // open_in_memory already runs apply_migrations.
    let a = open_in_memory().unwrap();
    let b = open_in_memory().unwrap();
    let conn_a = Arc::new(std::sync::Mutex::new(a));
    let conn_b = Arc::new(std::sync::Mutex::new(b));
    let cs = Arc::new(InMemoryStub::default());
    let cs_erased: Arc<dyn harmony_app::content_store::ContentStore> = cs.clone();
    let state_a = Arc::new(TokioMutex::new(MintSyncState::default()));
    let state_b = Arc::new(TokioMutex::new(MintSyncState::default()));

    let (engine_a, handle_a) =
        MintSyncEngine::new_for_test(conn_a.clone(), cs_erased.clone(), state_a).await;
    let (engine_b, handle_b) =
        MintSyncEngine::new_for_test(conn_b.clone(), cs_erased.clone(), state_b).await;
    Harness {
        conn_a,
        conn_b,
        cs,
        engine_a,
        engine_b,
        handle_a,
        handle_b,
    }
}

/// Wait until the content store has at least `min_count` blobs, with a
/// bounded poll to avoid flaky fixed-sleep timeouts on slow CI (MAJOR 9).
async fn wait_for_cs_count(cs: &Arc<InMemoryStub>, min_count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if cs.debug_all_cids().await.len() >= min_count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for content store to reach {min_count} blob(s); \
                 got {} after 5s",
                cs.debug_all_cids().await.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Drive engine_a → publish, then deliver to engine_b.
async fn sync_a_to_b(h: &Harness) {
    let before_count = h.cs.debug_all_cids().await.len();
    h.engine_a.flush_now().await.unwrap();
    // Bounded poll instead of fixed sleep (MAJOR 9: avoids flaky CI failures).
    wait_for_cs_count(&h.cs, before_count + 1).await;
    let cids = h.cs.debug_all_cids().await;
    for cid in cids {
        h.engine_b
            .handle_incoming_envelope_for_test(cid)
            .await
            .unwrap_or_else(|e| panic!("envelope {cid:?} failed on engine_b: {e}"));
    }
}

async fn shutdown(h: Harness) {
    h.engine_a.shutdown().await.unwrap();
    h.engine_b.shutdown().await.unwrap();
    h.handle_a.await.unwrap();
    h.handle_b.await.unwrap();
}

#[tokio::test]
async fn two_engines_converge_on_inserts() {
    let h = setup().await;
    {
        let conn = h.conn_a.lock().unwrap();
        let acct = create_account(&conn, "Chase").unwrap();
        for i in 0..5 {
            create_transaction(
                &conn,
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
    sync_a_to_b(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 5);
    }
    shutdown(h).await;
}

#[tokio::test]
async fn two_engines_converge_on_updates() {
    let h = setup().await;
    let tx_id = {
        let conn = h.conn_a.lock().unwrap();
        let acct = create_account(&conn, "Chase").unwrap();
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-10".into(),
                currency: "USD".into(),
                account_id: acct.id.clone(),
                description: "original".into(),
                metadata: None,
            },
        )
        .unwrap()
        .id
    };
    sync_a_to_b(&h).await;
    {
        let conn = h.conn_a.lock().unwrap();
        update_transaction(
            &conn,
            &tx_id,
            UpdateTransaction {
                transaction_date: None,
                amount: None,
                currency: None,
                account_id: None,
                description: Some("edited".into()),
                metadata: None,
            },
        )
        .unwrap();
    }
    sync_a_to_b(&h).await;
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
    shutdown(h).await;
}

#[tokio::test]
async fn two_engines_converge_on_delete() {
    let h = setup().await;
    let tx_id = {
        let conn = h.conn_a.lock().unwrap();
        let acct = create_account(&conn, "Chase").unwrap();
        create_transaction(
            &conn,
            NewTransaction {
                transaction_date: "2026-05-01".into(),
                amount: "-10".into(),
                currency: "USD".into(),
                account_id: acct.id.clone(),
                description: "x".into(),
                metadata: None,
            },
        )
        .unwrap()
        .id
    };
    sync_a_to_b(&h).await;
    {
        let conn = h.conn_a.lock().unwrap();
        delete_transaction(&conn, &tx_id).unwrap();
    }
    sync_a_to_b(&h).await;
    {
        let conn = h.conn_b.lock().unwrap();
        let deleted: Option<String> = conn
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                [&tx_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted.is_some(), "tombstone should have propagated");
        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 0);
    }
    shutdown(h).await;
}

#[tokio::test]
async fn two_engines_converge_on_setting_change() {
    let h = setup().await;
    {
        let conn = h.conn_a.lock().unwrap();
        // Create an account so the snapshot is non-empty and publish_root_now
        // doesn't short-circuit on the empty-ledger guard.
        create_account(&conn, "Seed").unwrap();
        set_default_currency(&conn, "JPY").unwrap();
    }
    sync_a_to_b(&h).await;
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
    shutdown(h).await;
}

#[tokio::test]
async fn concurrent_writes_to_distinct_rows_both_land() {
    let h = setup().await;
    // Seed an account on A and sync to B.
    let acct_id = {
        let conn = h.conn_a.lock().unwrap();
        create_account(&conn, "Chase").unwrap().id
    };
    sync_a_to_b(&h).await;

    // Concurrent inserts (different UUIDs).
    let tx_a = {
        let conn = h.conn_a.lock().unwrap();
        create_transaction(
            &conn,
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
    let tx_b = {
        let conn = h.conn_b.lock().unwrap();
        create_transaction(
            &conn,
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

    // A → B
    sync_a_to_b(&h).await;
    // B → A: publish from B and apply on A.
    let before_b = h.cs.debug_all_cids().await.len();
    h.engine_b.flush_now().await.unwrap();
    wait_for_cs_count(&h.cs, before_b + 1).await;
    let cids = h.cs.debug_all_cids().await;
    for cid in cids {
        h.engine_a
            .handle_incoming_envelope_for_test(cid)
            .await
            .unwrap_or_else(|e| panic!("envelope {cid:?} failed on engine_a: {e}"));
    }

    // Both DBs should have both rows.
    for conn_arc in [&h.conn_a, &h.conn_b] {
        let conn = conn_arc.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE id IN (?1, ?2)",
                rusqlite::params![&tx_a, &tx_b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }
    shutdown(h).await;
}
