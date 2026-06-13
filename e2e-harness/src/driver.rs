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

/// Poll the iroh first-contact community join until the recipient reports
/// `"joined"`, returning the joined RedemptionOutcome (with `communityId`).
///
/// Retries on BOTH (a) transient RPC errors — most importantly
/// `pkarr resolve: no relays available (all on cooldown or unreachable)`, which
/// happens when the external pkarr relays rate-limit a node under repeated runs
/// — and (b) the `inviter_unreachable` outcome (pkarr/iroh not yet converged).
/// Fails fast only on a terminal non-join status (e.g. `join_failed`), and times
/// out after `timeout`. This is what makes the suite robust to the inherently
/// racy + relay-dependent first-contact path; see ZEB-447 / ZEB-461.
pub async fn poll_join_iroh(
    node: &NodeHandle,
    url: &str,
    timeout: Duration,
) -> anyhow::Result<Value> {
    poll_until(timeout, || async {
        match redeem_invite_iroh(node, url).await {
            Ok(outcome) => match outcome.get("status").and_then(|v| v.as_str()) {
                Some("joined") => Ok(Some(outcome)),
                // pkarr/iroh not yet converged — keep polling.
                Some("inviter_unreachable") => Ok(None),
                other => {
                    anyhow::bail!("iroh redeem terminal non-join status: {other:?} ({outcome})")
                }
            },
            // Transient RPC error (pkarr relay cooldown, transport hiccup): the
            // relay pool recovers and pkarr propagates — retry, don't fail.
            Err(e) => {
                eprintln!("poll_join_iroh transient error (retrying): {e}");
                Ok(None)
            }
        }
    })
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

/// Read a DM thread tolerant of which `SpaceId` the conversation settled under.
///
/// DM `SpaceId`s are minted randomly per node (`SpaceId(rand::random())`), with a
/// dedupe key = sorted member set. When two independently-created same-member-set
/// spaces meet (via the cross-node space-invite each side dispatches in
/// `add_space`), `apply_space` canonicalizes them to the lexicographically-smaller
/// `SpaceId` and REMOVES the loser. So the id a node returned from `add_space` may
/// no longer be live after the merge — the live thread is under whichever candidate
/// id won. This reads every candidate (the local id + any peer/canonical id) and
/// unions the decoded messages, ignoring per-id `UnknownSpace` errors so a not-yet-
/// existing or already-merged-away id doesn't mask a delivered message under
/// another id. Returns the first candidate that yields a non-empty read, else the
/// empty union.
pub async fn read_dm_plaintext_any(
    node: &NodeHandle,
    candidate_space_ids: &[&str],
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    for sid in candidate_space_ids {
        match read_dm_plaintext(node, sid).await {
            Ok(msgs) if !msgs.is_empty() => return Ok(msgs),
            Ok(_) => {}  // empty thread under this id — keep trying others
            Err(_) => {} // UnknownSpace / merged-away id — not fatal, try next
        }
    }
    Ok(Vec::new())
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
