//! ZEB-480 `harmony-app watch` subprocess smoke. Gated behind `--features e2e`
//! (spawns the real binary + transport; NEVER runs in CI). Build the binary
//! first: `cd src-tauri && cargo build --bin harmony-app`.
//!
//! Proves the subprocess/stdout contract the channel-watch playbook depends on:
//! spawn `serve`, create a community + channel, post a message, then spawn
//! `harmony-app watch` against the SAME node (matching env → same data-dir +
//! discovery files) and assert it prints the message as one NDJSON line.
//!
//! The CI-durable coverage of the resume/backfill contract is the in-process
//! `tests/api/watch.rs::watch_backfill_emits_then_dedupes` test; this adds
//! real-subprocess realism only.

#![cfg(feature = "e2e")]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use e2e_harness::{resolve_harmony_app_bin, NodeConfig, NodeHandle};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

fn fresh_home(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("harmony-e2e-{tag}-"))
        .tempdir()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_subprocess_prints_channel_message() {
    let home = fresh_home("watch");
    let cfg = NodeConfig::new(PathBuf::from(home.path()), "watcher");
    let node = NodeHandle::spawn(cfg.clone()).await.expect("spawn node");

    // Mint owner, create a community + channel, post a message.
    node.rpc("mint_owner_identity", json!({}))
        .await
        .expect("mint");
    let community_id = node
        .rpc("create_community", json!({"name": "watch-e2e", "isInviteOnly": false}))
        .await
        .expect("create_community")
        .as_str()
        .expect("community id string")
        .to_string();
    let channel_id = node
        .rpc(
            "create_channel",
            json!({"communityId": community_id, "name": "team-chat", "writePower": 0, "kind": "text"}),
        )
        .await
        .expect("create_channel")
        .as_str()
        .expect("channel id string")
        .to_string();
    node.rpc(
        "post_channel_message",
        json!({"communityId": community_id, "channelId": channel_id, "body": b"hello subprocess".to_vec(), "replyTo": null}),
    )
    .await
    .expect("post_channel_message");

    // Spawn `harmony-app watch` against the same node. `--since 0:0:0` backfills
    // from origin so the already-posted message is caught regardless of connect
    // timing; `--no-retry` so it doesn't loop forever if the socket drops.
    let bin = resolve_harmony_app_bin().expect("harmony-app bin");
    let mut child = tokio::process::Command::new(bin)
        .arg("--profile")
        .arg(&cfg.profile)
        .arg("watch")
        .arg("--community")
        .arg(&community_id)
        .arg("--channel")
        .arg(&channel_id)
        .arg("--since")
        .arg("0:0:0")
        .arg("--no-retry")
        .env("HOME", &cfg.home)
        .env("USERPROFILE", &cfg.home)
        .env("HARMONY_DATA_DIR", cfg.home.join("data"))
        .env("XDG_DATA_HOME", cfg.home.join("xdg-data"))
        .env("HARMONY_PASSPHRASE", &cfg.passphrase)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn watch subprocess");

    let stdout = child.stdout.take().expect("watch stdout");
    let mut lines = BufReader::new(stdout).lines();

    // Read stdout until the posted message appears (bounded).
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("channelId").and_then(|c| c.as_str()) == Some(channel_id.as_str())
                    && v.get("body").and_then(|b| b.as_str()) == Some("hello subprocess")
                {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    let _ = child.start_kill();
    assert!(found, "watch subprocess must print the posted channel message");
}
