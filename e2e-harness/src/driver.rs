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
    // A non-array response is a broken RPC/DTO CONTRACT, not "no members":
    // surface it loudly so a schema regression can't masquerade as a roster
    // convergence timeout (the ZEB-462 masking trap). Matches list_channel_messages.
    v.as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("list_community_members expected a JSON array, got: {v}"))
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
    // A missing/non-array `held` is a broken response CONTRACT, not "no deposits":
    // surface it as an error so s6's characterize fallback can't mask a broken
    // get_relay_held by reading it as held=false (Qodo).
    let held = v
        .get("held")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("get_relay_held response missing 'held' array: {v}"))?;
    Ok(held.clone())
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
    // A non-array response is a broken RPC/DTO CONTRACT, not "no messages":
    // surface it loudly so a schema regression can't masquerade as a poll_until
    // timeout (the ZEB-462 masking trap). Matches `get_relay_held` / channels.
    v.as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("list_channel_messages expected a JSON array, got: {v}"))
}

// ── Vines (publish → feed → view → reshare) ──────────────────────────────────
//
// A node subscribes to `harmony/vines/*` at startup, so a peer's descriptor
// lands in the local feed over real Zenoh with no GUI. These wrap the headless
// vine verbs the two-node Vines scenario drives.

/// Publish a (non-reshare) vine. With no `videoCid` the impl ingests synthetic
/// bytes to mint a real CID. Returns `(vineId, videoCid)`.
pub async fn publish_vine(
    node: &NodeHandle,
    title: &str,
    creator_name: &str,
) -> anyhow::Result<(String, String)> {
    let v = node
        .rpc(
            "publish_vine",
            json!({ "title": title, "creatorName": creator_name }),
        )
        .await?;
    let vine_id = v
        .get("vineId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("publish_vine: missing vineId in {v}"))?
        .to_string();
    let video_cid = v
        .get("videoCid")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("publish_vine: missing videoCid in {v}"))?
        .to_string();
    Ok((vine_id, video_cid))
}

/// The local vine feed (flat, newest-first). A non-array response is a broken
/// DTO contract, not "empty feed" — surface it loudly (the ZEB-462 masking trap).
pub async fn list_vine_videos(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("list_vine_videos", json!({})).await?;
    v.as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("list_vine_videos expected a JSON array, got: {v}"))
}

/// Mark a vine viewed; `true` if newly marked, `false` if already viewed.
/// Fails loud on an unexpected response shape rather than masking a contract
/// regression as `false` (the ZEB-462 masking trap) — mirrors `list_vine_videos`.
pub async fn mark_vine_viewed(node: &NodeHandle, vine_id: &str) -> anyhow::Result<bool> {
    let v = node
        .rpc("mark_vine_viewed", json!({ "vineId": vine_id }))
        .await?;
    v.get("viewed")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("mark_vine_viewed expected {{ viewed: bool }}, got: {v}"))
}

/// Reshare a vine by id; returns the published reshare's `reshareOf`.
pub async fn reshare_vine(
    node: &NodeHandle,
    vine_id: &str,
    creator_name: &str,
) -> anyhow::Result<String> {
    let v = node
        .rpc(
            "reshare_vine",
            json!({ "vineId": vine_id, "creatorName": creator_name }),
        )
        .await?;
    v.get("reshareOf")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("reshare_vine: missing reshareOf in {v}"))
        .map(str::to_string)
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

// ── Pairing (ZEB-446) + butler rung (ZEB-489) helpers — ZEB-490 ──────────────

/// ZEB-490: assert two pairing nodes derived the SAME well-formed SAS — the real
/// SAS security property. The SAS is exactly 6 ASCII digits; a malformed value on
/// either side (wrong length / non-digit) is itself a bug, so validate shape
/// before comparing equality (CodeRabbit) — otherwise two equally-malformed
/// values would falsely pass. Returns a hard `Err` the scenario surfaces.
pub fn assert_sas_match(inviter_sas: &str, joiner_sas: &str) -> anyhow::Result<()> {
    for (who, sas) in [("inviter", inviter_sas), ("joiner", joiner_sas)] {
        if sas.len() != 6 || !sas.bytes().all(|b| b.is_ascii_digit()) {
            anyhow::bail!("malformed {who} SAS (expected 6 digits): {sas:?}");
        }
    }
    if inviter_sas != joiner_sas {
        anyhow::bail!("SAS mismatch: inviter={inviter_sas} joiner={joiner_sas}");
    }
    Ok(())
}

/// ZEB-490: inviter side — load owner_state + master_seed, enter Discovering.
pub async fn start_inviter_pairing(node: &NodeHandle, display_name: &str) -> anyhow::Result<()> {
    node.rpc(
        "start_inviter_pairing",
        json!({ "displayName": display_name }),
    )
    .await
    .map(|_| ())
}

/// ZEB-490: joiner side — generate a fresh ed25519 signing key, enter Discovering.
pub async fn start_joiner_pairing(node: &NodeHandle, display_name: &str) -> anyhow::Result<()> {
    node.rpc(
        "start_joiner_pairing",
        json!({ "displayName": display_name }),
    )
    .await
    .map(|_| ())
}

/// ZEB-490: select the discovered peer by its session id (mutual — BOTH sides
/// must select each other before either advances to Handshaking).
pub async fn select_pairing_peer(node: &NodeHandle, peer_session_id: &str) -> anyhow::Result<()> {
    node.rpc(
        "select_pairing_peer",
        json!({ "peerSessionId": peer_session_id }),
    )
    .await
    .map(|_| ())
}

/// ZEB-490: confirm the SAS — exchanges the encrypted Confirm; the inviter then
/// signs the EnrollmentCert.
pub async fn confirm_pairing_sas(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("confirm_pairing_sas", json!({})).await.map(|_| ())
}

/// ZEB-490: snapshot the current PairingState ({ kind, ...fields }, camelCase).
pub async fn get_pairing_state(node: &NodeHandle) -> anyhow::Result<Value> {
    node.rpc("get_pairing_state", json!({})).await
}

/// ZEB-490: abort an in-progress pairing.
pub async fn cancel_pairing(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("cancel_pairing", json!({})).await.map(|_| ())
}

/// ZEB-490: pin/clear a fleet device as butler. `device_id` is the 64-hex
/// ed25519 verify key (the enrolled-set id), NOT the 32-hex identity hash.
pub async fn set_butler_pin(node: &NodeHandle, device_id: Option<&str>) -> anyhow::Result<()> {
    node.rpc("set_butler_pin", json!({ "deviceId": device_id }))
        .await
        .map(|_| ())
}

/// ZEB-490: read this fleet's butler pin status ({ pinnedDeviceId, pinnedAtMs }).
pub async fn get_butler_pin(node: &NodeHandle) -> anyhow::Result<Value> {
    node.rpc("get_butler_pin", json!({})).await
}

/// ZEB-490: list the butler-held dm-inbox entries on this node. A missing/non-
/// array `held` is a broken response CONTRACT (surface it), mirroring
/// `get_relay_held`, so a characterize fallback can't read a broken response as
/// "nothing held".
pub async fn get_butler_held(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("get_butler_held", json!({})).await?;
    let held = v
        .get("held")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("get_butler_held response missing 'held' array: {v}"))?;
    Ok(held.clone())
}

/// ZEB-490: detect a terminal `Failed` pairing state so a poll fails fast with
/// the reason instead of silently timing out (Qodo). Returns the reason string
/// when `kind == "failed"`.
fn pairing_failed_reason(st: &Value) -> Option<String> {
    if st.get("kind").and_then(Value::as_str) == Some("failed") {
        Some(
            st.get("reason")
                .and_then(Value::as_str)
                .unwrap_or("(no reason)")
                .to_string(),
        )
    } else {
        None
    }
}

/// ZEB-490: drive the real ZEB-446 SAS pairing handshake between two local
/// nodes until the joiner is enrolled into the inviter's fleet. Returns the
/// joiner's 64-hex ed25519 verify key — the device id `set_butler_pin` expects.
///
/// Mutual-selection contract (state_machine `maybe_advance_to_handshake`
/// requires `sent_select && received_select`): both sides SelectPeer each other.
/// `deadline` is an OVERALL budget: each phase polls only the time remaining
/// until `Instant::now() + deadline`, so worst-case runtime is `deadline`, not a
/// multiple of it (Qodo). Each poll also fails fast on a `Failed` state (with the
/// reason), and returns `Err` on timeout so the caller's boundary-1 characterize
/// fallback can fire instead of panicking.
pub async fn pair_into_fleet(
    inviter: &NodeHandle,
    joiner: &NodeHandle,
    display: &str,
    deadline: Duration,
) -> anyhow::Result<String> {
    let end = Instant::now() + deadline;
    start_inviter_pairing(inviter, &format!("{display}-P")).await?;
    start_joiner_pairing(joiner, &format!("{display}-B2")).await?;

    // Inviter discovers the joiner; capture the joiner's session id + its
    // ed25519 verify key (the pin device id). Both sides keep broadcasting
    // DISCOVER until they SelectPeer, so polling them sequentially here (before
    // any select) still lets both discover each other.
    let (joiner_session, joiner_vk) =
        poll_until(end.saturating_duration_since(Instant::now()), || async {
            let st = get_pairing_state(inviter).await?;
            if let Some(reason) = pairing_failed_reason(&st) {
                anyhow::bail!("inviter pairing failed: {reason}");
            }
            if st.get("kind").and_then(Value::as_str) != Some("discovered") {
                return Ok(None);
            }
            // Exactly one joiner is expected in the isolated 2-node scenario; >1
            // means a contaminated LAN (another concurrent pairing), where a
            // role-only pick would be nondeterministic (CodeAnt) — bail instead.
            let joiners: Vec<&Value> = st
                .get("peers")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter(|p| p.get("role").and_then(Value::as_str) == Some("joiner"))
                        .collect()
                })
                .unwrap_or_default();
            if joiners.len() > 1 {
                anyhow::bail!(
                    "ambiguous pairing discovery: {} joiner peers",
                    joiners.len()
                );
            }
            let Some(peer) = joiners.first() else {
                return Ok(None);
            };
            match (
                peer.get("sessionId").and_then(Value::as_str),
                peer.get("joinerEd25519VerifyHex").and_then(Value::as_str),
            ) {
                (Some(sid), Some(vk)) => Ok(Some((sid.to_string(), vk.to_string()))),
                _ => Ok(None),
            }
        })
        .await?;

    // Joiner discovers the inviter; capture the inviter's session id.
    let inviter_session = poll_until(end.saturating_duration_since(Instant::now()), || async {
        let st = get_pairing_state(joiner).await?;
        if let Some(reason) = pairing_failed_reason(&st) {
            anyhow::bail!("joiner pairing failed: {reason}");
        }
        if st.get("kind").and_then(Value::as_str) != Some("discovered") {
            return Ok(None);
        }
        // Exactly one inviter is expected (see the joiner-side note above);
        // >1 means a contaminated LAN — bail rather than pick nondeterministically.
        let inviters: Vec<&Value> = st
            .get("peers")
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.get("role").and_then(Value::as_str) == Some("inviter"))
                    .collect()
            })
            .unwrap_or_default();
        if inviters.len() > 1 {
            anyhow::bail!(
                "ambiguous pairing discovery: {} inviter peers",
                inviters.len()
            );
        }
        Ok(inviters
            .first()
            .and_then(|p| p.get("sessionId").and_then(Value::as_str))
            .map(str::to_string))
    })
    .await?;

    // Mutual select.
    select_pairing_peer(inviter, &joiner_session).await?;
    select_pairing_peer(joiner, &inviter_session).await?;

    // Both derive SAS; assert the codes match (the real security property).
    let inviter_sas =
        poll_pairing_sas(inviter, end.saturating_duration_since(Instant::now())).await?;
    let joiner_sas =
        poll_pairing_sas(joiner, end.saturating_duration_since(Instant::now())).await?;
    assert_sas_match(&inviter_sas, &joiner_sas)?;

    // Both confirm; the inviter signs the EnrollmentCert.
    confirm_pairing_sas(inviter).await?;
    confirm_pairing_sas(joiner).await?;

    // Both reach Complete (joiner now enrolled in the inviter's fleet).
    poll_pairing_complete(inviter, end.saturating_duration_since(Instant::now())).await?;
    poll_pairing_complete(joiner, end.saturating_duration_since(Instant::now())).await?;

    Ok(joiner_vk)
}

/// Poll a node until it is `Handshaking`, returning its 6-digit `sasDigits`.
/// Fails fast on a `Failed` state. `timeout` is the remaining overall budget.
async fn poll_pairing_sas(node: &NodeHandle, timeout: Duration) -> anyhow::Result<String> {
    poll_until(timeout, || async {
        let st = get_pairing_state(node).await?;
        if let Some(reason) = pairing_failed_reason(&st) {
            anyhow::bail!("pairing failed: {reason}");
        }
        if st.get("kind").and_then(Value::as_str) != Some("handshaking") {
            return Ok(None);
        }
        // In `handshaking` the SAS MUST be present; a missing/non-string
        // sasDigits is a schema regression — fail fast rather than spin to a
        // timeout (CodeRabbit).
        let sas = st.get("sasDigits").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!("handshaking state missing/non-string sasDigits (schema regression)")
        })?;
        Ok(Some(sas.to_string()))
    })
    .await
}

/// Poll a node until it reaches `Complete` (enrollment finished). Fails fast on
/// a `Failed` state. `timeout` is the remaining overall budget.
async fn poll_pairing_complete(node: &NodeHandle, timeout: Duration) -> anyhow::Result<()> {
    poll_until(timeout, || async {
        let st = get_pairing_state(node).await?;
        if let Some(reason) = pairing_failed_reason(&st) {
            anyhow::bail!("pairing failed: {reason}");
        }
        Ok((st.get("kind").and_then(Value::as_str) == Some("complete")).then_some(()))
    })
    .await
}

/// Leave a community (ZEB-427 Half B: the engine commits the Leave, then the
/// owner-state Space row is marked `left_at` and fenced to disk).
pub async fn leave_community(node: &NodeHandle, community_id: &str) -> anyhow::Result<Value> {
    node.rpc("leave_community", json!({ "communityId": community_id }))
        .await
}

/// Owner-visible communities (`list_owner_communities` — already filters out
/// rows whose `Space.left_at` is set, both live and after a restart-rehydrate).
pub async fn list_owner_communities(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("list_owner_communities", json!({})).await?;
    // The backend command returns `Vec<CommunityNavDto>`, so a successful response is
    // always a JSON array. A non-array is a broken response CONTRACT, not "no
    // communities": surface it as an error so `community_listed` can't mask an RPC/DTO
    // regression by reading it as an empty list (Qodo; cf. get_relay_held / PR #277).
    v.as_array().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "list_owner_communities response is not a JSON array (Vec<CommunityNavDto>): {v}"
        )
    })
}

/// Whether `space_id` appears in `list_owner_communities`. The id field on
/// `CommunityNavDto` is camelCase `spaceId` (NOT `id`) — checking the wrong key
/// is an always-false trap that masquerades as a sync/durability failure (the
/// bug that long mis-`#[ignore]`'d S3/S4). A community object present but missing
/// a string `spaceId` is a DTO/schema mismatch, surfaced loudly rather than read
/// as absence.
pub async fn community_listed(node: &NodeHandle, space_id: &str) -> anyhow::Result<bool> {
    for c in list_owner_communities(node).await? {
        let sid = c.get("spaceId").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "community object missing string `spaceId` key (DTO/schema mismatch?): {c}"
            )
        })?;
        if sid == space_id {
            return Ok(true);
        }
    }
    Ok(false)
}

// ── Reachability-sync barrier (durable seal-targets) ─────────────────────────
//
// The deposit rungs (relay + butler) resolve a recipient's SEAL TARGETS on the
// SENDER, from the sender's `ReachabilityResolver` (in-memory CRDT cache, pkarr
// fallback) carrying the recipient's `ReachabilityAnnounce` payload — which now
// carries the recipient's DURABLE butler-set (ZEB-488). So a deposit for a
// fully-offline recipient resolves ONLY once that recipient's DURABLE announce
// has replicated to the sender BEFORE the recipient is killed. These helpers give
// a deterministic barrier for that replication: poll until the observing node's
// `connectivity_list_peer_reachability` snapshot contains the target owner with
// `source == "durableCrdt"` (a transient pkarr cache-back does not count).

/// Snapshot of the peer-reachability entries this node's LWW resolver knows.
/// `PeerReachabilityDto` is `#[serde(rename_all = "camelCase")]` — the owner key
/// is `ownerAddress` (the 32-hex `OwnerAddr`, same value `owner_id`/`ownerId`
/// returns and what `add_dm_space`/friend RPCs key on). A successful response is
/// always a JSON array; a non-array is a broken response CONTRACT, surfaced as an
/// error rather than read as "no peers" (cf. get_relay_held / list_owner_communities).
pub async fn list_peer_reachability(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    let v = node
        .rpc("connectivity_list_peer_reachability", json!({}))
        .await?;
    v.as_array().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "connectivity_list_peer_reachability response is not a JSON array \
             (Vec<PeerReachabilityDto>): {v}"
        )
    })
}

/// Whether `node` has OBSERVED `target_owner`'s DURABLE-CRDT `ReachabilityAnnounce`
/// — i.e. an entry whose `ownerAddress` matches the target's 32-hex owner id AND
/// whose `source` is `"durableCrdt"` is present in `node`'s resolver.
///
/// A pkarr cache-back (`source == "pkarrLive"`) does NOT satisfy this barrier: it
/// proves only a transient live lookup, not durable community-CRDT replication of
/// the recipient's seal-targets — and the whole point of the deposit→recover
/// asserts is to prove the DURABLE path replicated before the recipient is killed
/// (Qodo "Barrier ignores entry source"; ZEB-488). An entry present but missing a
/// string `ownerAddress`/`source` is a DTO/schema mismatch, surfaced loudly rather
/// than read as absence (the ZEB-462 wrong-key trap).
pub async fn has_observed_reachability(
    node: &NodeHandle,
    target_owner: &str,
) -> anyhow::Result<bool> {
    for e in list_peer_reachability(node).await? {
        let owner = e
            .get("ownerAddress")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "peer-reachability entry missing string `ownerAddress` key \
                 (DTO/schema mismatch?): {e}"
                )
            })?;
        if owner != target_owner {
            continue;
        }
        let source = e.get("source").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "peer-reachability entry missing string `source` key \
                 (DTO/schema mismatch?): {e}"
            )
        })?;
        if source == "durableCrdt" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reachability-sync barrier: poll until `observer` has observed `target_owner`'s
/// DURABLE-CRDT `ReachabilityAnnounce` (carrying the durable butler-set, ZEB-488)
/// in its resolver, or `timeout` elapses.
///
/// This is the load-bearing precondition for the deposit→recover hard asserts:
/// once the observer (the SENDER) sees the target's DURABLE announce, its resolver
/// holds the target's payload INCLUDING the durable butler-set, so a deposit for
/// the then-offline target resolves seal targets instead of skipping the rung. The
/// announce payload and its embedded durable butler-set replicate together via
/// the SAME community-CRDT path, so observing the announce is observing the set.
///
/// NB: the headless `connectivity_list_peer_reachability` DTO
/// (`PeerReachabilityDto`) does NOT expose the butler-set field, so this asserts
/// the announce-observed PROXY rather than reading the set directly. The set
/// cannot arrive separately from its carrying announce, so the proxy is exact for
/// the replication-before-kill guarantee this barrier exists to provide. The
/// `source == "durableCrdt"` gate (see [`has_observed_reachability`]) ensures the
/// proxy is satisfied only by durable replication, never by a transient pkarr
/// cache-back.
pub async fn poll_reachability_observed(
    observer: &NodeHandle,
    target_owner: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    poll_until(timeout, || async {
        Ok(has_observed_reachability(observer, target_owner)
            .await?
            .then_some(()))
    })
    .await
}

// ── ZEB-715: admin-recovery verbs (D3 e2e over the ZEB-714 surface) ──────────
//
// Request/response shapes are the camelCase DTOs pinned by
// `recovery_rpcs_are_registered_and_wired` (src-tauri/src/api/rpc.rs) and
// `RecoveryStateDto` (src-tauri/src/lib.rs). Per the ZEB-462 rule, an object
// present but missing an expected camelCase key is surfaced as a loud contract
// error, never read as absence.

/// `set_recovery_designates` — admin-only config ceremony (spec §3.1). Returns
/// the `AdminActionResult` value; in these scenarios the founder self-satisfies
/// `admin_quorum == 1`, so a successful call is `{"kind": "Completed"}`.
pub async fn set_recovery_designates(
    node: &NodeHandle,
    community_id: &str,
    designate_addrs: &[&str],
    threshold: u8,
    veto_window_ms: u64,
) -> anyhow::Result<Value> {
    node.rpc(
        "set_recovery_designates",
        json!({
            "communityId": community_id,
            "designateAddrs": designate_addrs,
            "threshold": threshold,
            "vetoWindowMs": veto_window_ms,
        }),
    )
    .await
}

/// `initiate_admin_recovery` — a designate proposes replacing `lost_admin`
/// with `new_admin`. Returns the proposal event id (the handle every
/// cosign/veto and banner assertion keys on).
pub async fn initiate_admin_recovery(
    node: &NodeHandle,
    community_id: &str,
    lost_admin_addr: &str,
    new_admin_addr: &str,
) -> anyhow::Result<String> {
    let v = node
        .rpc(
            "initiate_admin_recovery",
            json!({
                "communityId": community_id,
                "lostAdminAddr": lost_admin_addr,
                "newAdminAddr": new_admin_addr,
            }),
        )
        .await?;
    v.get("proposalEventId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "InitiateRecoveryResult missing string `proposalEventId` (DTO/schema mismatch?): {v}"
            )
        })
}

/// `cosign_admin_recovery` — returns the `RecoveryCosignResult` value
/// (`signersAfter` / `threshold` / `reachedThreshold`).
pub async fn cosign_admin_recovery(
    node: &NodeHandle,
    community_id: &str,
    proposal_event_id: &str,
) -> anyhow::Result<Value> {
    node.rpc(
        "cosign_admin_recovery",
        json!({
            "communityId": community_id,
            "proposalEventId": proposal_event_id,
        }),
    )
    .await
}

/// `veto_admin_recovery` — power-100 admin kills the proposal (unit result).
pub async fn veto_admin_recovery(
    node: &NodeHandle,
    community_id: &str,
    proposal_event_id: &str,
) -> anyhow::Result<()> {
    node.rpc(
        "veto_admin_recovery",
        json!({
            "communityId": community_id,
            "proposalEventId": proposal_event_id,
        }),
    )
    .await
    .map(|_| ())
}

/// `get_recovery_state` on the real clock — the view every member's banner
/// polls (`RecoveryStateDto`: `config` / `proposals` / `selfIsDesignate` /
/// `selfPower`).
pub async fn recovery_state(node: &NodeHandle, community_id: &str) -> anyhow::Result<Value> {
    node.rpc("get_recovery_state", json!({ "communityId": community_id }))
        .await
}

/// `get_recovery_state` with the ZEB-715 read-side as-of override. Recovery
/// phases advance with wall clock and the veto window has a 7-day floor (RD4),
/// so TimeLocked→Executed is only observable by supplying `nowMs`. The
/// materialization is deterministic in (event set, nowMs) — once both nodes
/// hold the same events, no polling is needed around this read.
pub async fn recovery_state_as_of(
    node: &NodeHandle,
    community_id: &str,
    now_ms: u64,
) -> anyhow::Result<Value> {
    node.rpc(
        "get_recovery_state",
        json!({ "communityId": community_id, "nowMs": now_ms }),
    )
    .await
}

/// Poll predicate for "the designate config has replicated here and this node
/// is one of the designates". `Ok(None)` = not converged yet — which includes
/// BOTH `config: null` (the SetRecoveryDesignates proposal hasn't landed) and
/// the freshly-joined window where the community engine hasn't spawned (the
/// RPC refuses with "no engine for community"). A MISSING `config` or
/// `selfIsDesignate` key, or a mis-typed value, is a broken DTO contract,
/// surfaced loudly per the ZEB-462 rule (Qodo, PR #499 R1) — `config` is an
/// `Option` serialized as an always-present nullable key, and
/// `selfIsDesignate` is an always-present bool.
pub async fn recovery_configured_as_designate(
    node: &NodeHandle,
    community_id: &str,
) -> anyhow::Result<Option<()>> {
    let st = match recovery_state(node, community_id).await {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no engine for community") => return Ok(None),
        Err(e) => return Err(e),
    };
    let config_present = !st
        .get("config")
        .ok_or_else(|| {
            anyhow::anyhow!("RecoveryStateDto missing `config` (DTO/schema mismatch?): {st}")
        })?
        .is_null();
    let is_designate = st
        .get("selfIsDesignate")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "RecoveryStateDto missing bool `selfIsDesignate` (DTO/schema mismatch?): {st}"
            )
        })?;
    Ok((config_present && is_designate).then_some(()))
}

/// The proposal object with `proposalEventId == proposal_id` from a
/// `RecoveryStateDto` value. `Ok(None)` means "not replicated here yet" (poll
/// semantics); a missing/non-array `proposals` key or a proposal object without
/// a string `proposalEventId` is a broken DTO contract, surfaced loudly.
pub fn recovery_proposal<'a>(
    state: &'a Value,
    proposal_id: &str,
) -> anyhow::Result<Option<&'a Value>> {
    let proposals = state
        .get("proposals")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "RecoveryStateDto missing `proposals` array (DTO/schema mismatch?): {state}"
            )
        })?;
    for p in proposals {
        let pid = p
            .get("proposalEventId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                "recovery proposal missing string `proposalEventId` (DTO/schema mismatch?): {p}"
            )
            })?;
        if pid == proposal_id {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::assert_sas_match;

    #[test]
    fn assert_sas_match_ok_when_equal() {
        assert!(assert_sas_match("012845", "012845").is_ok());
    }

    #[test]
    fn assert_sas_match_err_when_differ() {
        let e = assert_sas_match("012845", "999999").unwrap_err();
        assert!(
            e.to_string().contains("SAS mismatch"),
            "error should name the mismatch, got: {e}"
        );
    }

    #[test]
    fn assert_sas_match_err_when_malformed() {
        // Equal but malformed (wrong length / non-digit) must NOT pass.
        for (a, b) in [("12", "12"), ("01284a", "01284a"), ("", "")] {
            let e = assert_sas_match(a, b).unwrap_err();
            assert!(
                e.to_string().contains("malformed"),
                "malformed SAS should be rejected, got: {e}"
            );
        }
    }
}
