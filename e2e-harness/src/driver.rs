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
    let deadline = std::time::Instant::now() + timeout;
    // Capture the last error/outcome so a hard failure surfaces its real message
    // on timeout instead of a bare generic timeout (Cursor: poll retries permanent
    // RPC errors). A terminal non-join status still fails fast immediately.
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("poll_join_iroh timed out after {timeout:?}; last error: {last_err}");
        }
        match redeem_invite_iroh(node, url).await {
            Ok(outcome) => match outcome.get("status").and_then(|v| v.as_str()) {
                Some("joined") => return Ok(outcome),
                // pkarr/iroh not yet converged — keep polling.
                Some("inviter_unreachable") => {
                    last_err = format!("inviter_unreachable ({outcome})")
                }
                // A terminal non-join status (e.g. join_failed) is NOT retryable.
                other => {
                    anyhow::bail!("iroh redeem terminal non-join status: {other:?} ({outcome})")
                }
            },
            // RPC error (pkarr relay cooldown, transport hiccup, etc.): retry, but
            // remember it so a persistent/permanent failure is reported on timeout.
            Err(e) => {
                eprintln!("poll_join_iroh transient error (retrying): {e}");
                last_err = e.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

// ── Relay rung (ZEB-487) — community sealed-relay deposit→recover ────────────

/// ZEB-487: opt this node in/out of relaying for a community.
pub async fn set_relay_opt_in(
    node: &NodeHandle,
    community_id: &str,
    opted_in: bool,
) -> anyhow::Result<()> {
    node.rpc(
        "set_community_relay_opt_in",
        json!({ "communityIdHex": community_id, "optedIn": opted_in }),
    )
    .await?;
    Ok(())
}

/// ZEB-487: read whether this node is opted in to relaying for a community.
pub async fn get_relay_opt_in(node: &NodeHandle, community_id: &str) -> anyhow::Result<bool> {
    let v = node
        .rpc(
            "get_community_relay_status",
            json!({ "communityIdHex": community_id }),
        )
        .await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// ZEB-487: list the relay-held deposit entries on this node (routing metadata
/// only; the held blobs are sealed). `community_id = None` returns all.
pub async fn get_relay_held(
    node: &NodeHandle,
    community_id: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    let args = match community_id {
        Some(c) => json!({ "communityIdHex": c }),
        None => json!({}),
    };
    let v = node.rpc("get_relay_held", args).await?;
    Ok(v.get("held")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
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
    // `ChannelInfoDto` is `#[serde(rename_all = "camelCase")]`, so the id field
    // serializes as `channelId` (NOT `id`). Checking the wrong key makes this
    // silently always-false → poll_until always times out, masquerading as a
    // sync failure (exactly the ZEB-462 misdiagnosis). Guard against a future
    // DTO rename by surfacing a missing key as a loud schema error rather than a
    // silent miss: an empty list is "not converged yet" (keep polling), but a
    // non-empty list whose objects lack `channelId` is a contract mismatch.
    for c in list_channels(node, community_id).await? {
        let id = c.get("channelId").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "channel object missing string `channelId` key (DTO/schema mismatch?): {c}"
            )
        })?;
        if id == channel_id {
            return Ok(true);
        }
    }
    Ok(false)
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

// ── Profile cards (ZEB-341) + peer-profile broadcast (ZEB-281) — ZEB-464 ──────
//
// Card propagation rides a Zenoh broadcast topic keyed by the owner's 16-byte
// owner_id; a subscription needs only the peer's ownerIdHex. These wrap the
// headless card verbs the two-agent suite needs to drive S5.

/// Re-publish the local owner's signed profile card onto its owner_id-keyed
/// Zenoh topic. Avatar / profile-page CIDs are omitted — card propagation is
/// proven by the signed name/status; avatar bytes are the separate CAS layer.
pub async fn republish_owner_card(
    node: &NodeHandle,
    display_name: &str,
    status_text: &str,
) -> anyhow::Result<()> {
    node.rpc(
        "republish_owner_card",
        json!({ "displayName": display_name, "statusText": status_text }),
    )
    .await
    .map(|_| ())
}

/// Poll `republish_owner_card` until it is accepted. The card publisher runtime
/// is wired post-connect (in `start_node_inner`), so right after mint the publish
/// can fail with `owner card runtime not ready` until the Zenoh session is up —
/// retry until Ok or `timeout`, surfacing the last error on timeout.
pub async fn publish_card_until_ok(
    node: &NodeHandle,
    display_name: &str,
    status_text: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err = String::from("(no publish attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "publish_card_until_ok timed out after {timeout:?}; last error: {last_err}"
            );
        }
        match republish_owner_card(node, display_name, status_text).await {
            Ok(()) => return Ok(()),
            // Only the pre-connect "owner card runtime not ready" is transient
            // (the card publisher is wired post-connect). Any other error —
            // malformed avatar/profile-page hex, a poisoned lock, a publish
            // failure — is NOT retryable; fail fast so it surfaces immediately
            // instead of being masked by a timeout (Qodo, PR #263).
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("owner card runtime not ready") {
                    return Err(e);
                }
                last_err = msg;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Subscribe to a peer's profile-card topic (keyed by their owner_id hex).
/// Returns the u64 subscription id for `cached_card_display_name` / unsubscribe.
pub async fn subscribe_member_card(node: &NodeHandle, owner_id_hex: &str) -> anyhow::Result<u64> {
    node.rpc(
        "subscribe_member_card",
        json!({ "ownerIdHex": owner_id_hex }),
    )
    .await?
    .as_u64()
    .ok_or_else(|| anyhow::anyhow!("subscribe_member_card did not return a u64 subscription id"))
}

/// Snapshot the latest verified card for a subscription, returning the signed
/// `displayName` if a card has arrived (None = loading / not yet received).
/// `DiscoveredCardInfo` is camelCase — a present-but-keyless card object is
/// surfaced as a loud schema error, never a silent miss (the ZEB-462 wrong-key
/// trap that masqueraded as a sync failure).
pub async fn cached_card_display_name(
    node: &NodeHandle,
    subscription_id: u64,
) -> anyhow::Result<Option<String>> {
    let v = node
        .rpc(
            "get_cached_member_card",
            json!({ "subscriptionId": subscription_id }),
        )
        .await?;
    if v.is_null() {
        return Ok(None);
    }
    let name = v
        .get("displayName")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cached card missing string `displayName` key (DTO/schema mismatch?): {v}"
            )
        })?;
    Ok(Some(name.to_string()))
}

/// Tear down a card subscription.
pub async fn unsubscribe_member_card(
    node: &NodeHandle,
    subscription_id: u64,
) -> anyhow::Result<()> {
    node.rpc(
        "unsubscribe_member_card",
        json!({ "subscriptionId": subscription_id }),
    )
    .await
    .map(|_| ())
}
