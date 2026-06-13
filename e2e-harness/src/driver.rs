//! Semantic helpers over `NodeHandle::rpc` encoding the verified RPC contracts,
//! plus a generic `poll_until` convergence primitive.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::node::NodeHandle;

/// Poll `f` until it yields `Some(T)` or `timeout` elapses (250ms interval).
pub async fn poll_until<F, Fut, T>(timeout: Duration, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("poll_until timed out after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn as_str(v: &Value) -> anyhow::Result<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("expected string, got {v}"))
}

pub async fn mint(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("mint_owner_identity", json!({})).await.map(|_| ())
}

pub async fn create_community(
    node: &NodeHandle,
    name: &str,
    invite_only: bool,
) -> anyhow::Result<String> {
    as_str(
        &node
            .rpc(
                "create_community",
                json!({ "name": name, "isInviteOnly": invite_only }),
            )
            .await?,
    )
}

pub async fn generate_invite(node: &NodeHandle, community_id: &str) -> anyhow::Result<String> {
    as_str(
        &node
            .rpc("generate_invite", json!({ "communityId": community_id }))
            .await?,
    )
}

pub async fn redeem_invite(node: &NodeHandle, url: &str) -> anyhow::Result<Value> {
    node.rpc("redeem_invite", json!({ "url": url })).await
}

/// Iroh first-contact community join (the real cross-node path). Returns the
/// RedemptionOutcome value: { status, communityId }.
pub async fn redeem_invite_iroh(node: &NodeHandle, url: &str) -> anyhow::Result<Value> {
    node.rpc("connectivity_redeem_invite_iroh", json!({ "url": url }))
        .await
}

pub async fn list_community_members(
    node: &NodeHandle,
    community_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let v = node
        .rpc(
            "list_community_members",
            json!({ "communityId": community_id }),
        )
        .await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// True once `member_owner` appears in `community_id`'s roster with a joined
/// status. The headless `list_community_members` surface serializes the status
/// lowercase (`"joined"`); compare case-insensitively so the check is robust to
/// either casing.
pub async fn roster_has_joined(
    node: &NodeHandle,
    community_id: &str,
    member_owner: &str,
) -> anyhow::Result<bool> {
    let members = list_community_members(node, community_id).await?;
    Ok(members.iter().any(|m| {
        m.get("addr").and_then(Value::as_str) == Some(member_owner)
            && m.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("joined"))
    }))
}

pub async fn generate_friend_token(node: &NodeHandle) -> anyhow::Result<String> {
    as_str(&node.rpc("generate_friend_token", json!({})).await?)
}

pub async fn redeem_friend_token(node: &NodeHandle, url: &str) -> anyhow::Result<Value> {
    node.rpc("redeem_friend_token", json!({ "url": url })).await
}

pub async fn list_friends(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    Ok(node
        .rpc("list_friends", json!({}))
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

pub async fn friend_is_active(node: &NodeHandle, owner: &str) -> anyhow::Result<bool> {
    Ok(list_friends(node).await?.iter().any(|f| {
        f.get("ownerIdHex").and_then(Value::as_str) == Some(owner)
            && f.get("status").and_then(Value::as_str) == Some("active")
    }))
}

pub async fn accept_pending_from(node: &NodeHandle, owner: &str) -> anyhow::Result<bool> {
    let pending = node.rpc("list_pending_friend_requests", json!({})).await?;
    let has = pending
        .as_array()
        .map(|a| {
            a.iter()
                .any(|p| p.get("ownerIdHex").and_then(Value::as_str) == Some(owner))
        })
        .unwrap_or(false);
    if has {
        node.rpc("accept_friend_request", json!({ "ownerIdHex": owner }))
            .await?;
    }
    Ok(has)
}

pub async fn add_dm_space(
    node: &NodeHandle,
    name: &str,
    peer_owner: &str,
) -> anyhow::Result<String> {
    as_str(
        &node
            .rpc(
                "add_space",
                json!({ "kind": "dm", "name": name, "members": [peer_owner] }),
            )
            .await?,
    )
}

pub async fn send_dm(
    node: &NodeHandle,
    space_id: &str,
    content: &[u8],
    mime: &str,
) -> anyhow::Result<Value> {
    node.rpc(
        "send_dm",
        json!({ "spaceId": space_id, "content": content, "mimeType": mime }),
    )
    .await
}

/// Read the DM thread; returns decoded (from_owner, plaintext_bytes) pairs.
pub async fn read_dm_plaintext(
    node: &NodeHandle,
    space_id: &str,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let v = node
        .rpc(
            "read_dm_thread",
            json!({ "spaceId": space_id, "limit": 100 }),
        )
        .await?;
    let mut out = Vec::new();
    for m in v.as_array().cloned().unwrap_or_default() {
        let from = m
            .get("from")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let body_hex = m.get("body").and_then(Value::as_str).unwrap_or_default();
        let bytes = hex::decode(body_hex).unwrap_or_default();
        out.push((from, bytes));
    }
    Ok(out)
}

pub async fn create_channel(
    node: &NodeHandle,
    community_id: &str,
    name: &str,
    write_power: u8,
) -> anyhow::Result<String> {
    as_str(
        &node
            .rpc(
                "create_channel",
                json!({ "communityId": community_id, "name": name, "writePower": write_power }),
            )
            .await?,
    )
}

pub async fn list_channels(node: &NodeHandle, community_id: &str) -> anyhow::Result<Vec<Value>> {
    Ok(node
        .rpc("list_channels", json!({ "communityId": community_id }))
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

pub async fn channels_contains(
    node: &NodeHandle,
    community_id: &str,
    channel_id: &str,
) -> anyhow::Result<bool> {
    Ok(list_channels(node, community_id)
        .await?
        .iter()
        .any(|c| c.get("id").and_then(Value::as_str) == Some(channel_id)))
}

pub async fn post_channel_message(
    node: &NodeHandle,
    community_id: &str,
    channel_id: &str,
    body: &[u8],
) -> anyhow::Result<Value> {
    node.rpc(
        "post_channel_message",
        json!({ "communityId": community_id, "channelId": channel_id, "body": body }),
    )
    .await
}

pub async fn list_channel_messages(
    node: &NodeHandle,
    community_id: &str,
    channel_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let v = node
        .rpc(
            "list_channel_messages",
            json!({ "communityId": community_id, "channelId": channel_id, "limit": 100 }),
        )
        .await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}
