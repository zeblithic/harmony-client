// src-tauri/src/api/rpc.rs — ZEB-445 uniform RPC: POST /v1/rpc/{command}.
//
// Same command names, same camelCase JSON args, same DTOs, same error
// strings as the Tauri IPC — one mental model across GUI and API. The
// curated v1 surface is intentionally small; adding a command later is one
// rpc!() line plus its _impl seam.

use crate::node_event_sink::NodeEventSink;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug)]
pub enum RpcError {
    UnknownCommand,
    BadArgs(String),
    Command(String),
}

pub type RpcFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, RpcError>> + Send>>;
pub type RpcHandler = Box<
    dyn Fn(Arc<dyn super::NodeStateAccess>, Arc<dyn NodeEventSink>, serde_json::Value) -> RpcFuture
        + Send
        + Sync,
>;

pub struct RpcRegistry {
    handlers: HashMap<&'static str, RpcHandler>,
}

impl RpcRegistry {
    pub async fn dispatch(
        &self,
        command: &str,
        state: Arc<dyn super::NodeStateAccess>,
        sink: Arc<dyn NodeEventSink>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let h = self.handlers.get(command).ok_or(RpcError::UnknownCommand)?;
        h(state, sink, args).await
    }

    pub fn command_names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.handlers.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

macro_rules! rpc {
    ($map:expr, $name:literal, $args_ty:ty, |$state:ident, $sink:ident, $args:ident| $call:expr) => {
        $map.insert(
            $name,
            Box::new(
                move |__access: Arc<dyn super::NodeStateAccess>,
                      $sink: Arc<dyn NodeEventSink>,
                      raw: serde_json::Value| {
                    Box::pin(async move {
                        let $state = __access.node_state();
                        let raw = if raw.is_null() {
                            serde_json::json!({})
                        } else {
                            raw
                        };
                        let $args: $args_ty = serde_json::from_value(raw)
                            .map_err(|e| RpcError::BadArgs(e.to_string()))?;
                        let out = $call.await.map_err(RpcError::Command)?;
                        serde_json::to_value(out)
                            .map_err(|e| RpcError::Command(format!("serialize: {e}")))
                    }) as RpcFuture
                },
            ) as RpcHandler,
        );
    };
}

// ── Arg structs ──────────────────────────────────────────────────────
//
// One struct per distinct argument shape; field names/types mirror the
// Tauri command wrappers exactly (snake_case params on the Rust side,
// camelCase keys on the wire — same conversion Tauri's IPC layer does).
// Deliberately NO deny_unknown_fields: Tauri tolerates extra fields, so
// the RPC surface stays permissive for parity.

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmptyArgs {}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetBuddyPledgeArgs {
    owner_address: String,
    bytes: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveStorageBuddyArgs {
    owner_address: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSharedBudgetArgs {
    bytes: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetBackupFlagArgs {
    sidecar_id: String,
    backup: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartNodeArgs {
    endpoint: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommunityIdArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VineIdArgs {
    vine_id: String,
}

/// ZEB-435: `remove_space` takes a generic space id (community / dm /
/// group-dm), not specifically a community id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpaceIdArgs {
    space_id: String,
}

/// ZEB-562: headless vine-follow verbs. `name` is the optional display label
/// recorded alongside the followed address (mirrors the GUI command).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowVineCreatorArgs {
    address: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnfollowVineCreatorArgs {
    address: String,
}

/// ZEB-527: `community_id` + capped `limit` — shared by the two recent-feed
/// moderation read verbs (`list_recent_counter_signs`,
/// `list_recent_moderation_events`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommunityLimitArgs {
    community_id: String,
    limit: u32,
}

/// ZEB-527: `community_id` + `target_addr` + optional `reason` — shared by
/// the two member-targeting moderation action verbs (`kick_from_community`,
/// `unban_from_community`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModerationTargetArgs {
    community_id: String,
    target_addr: String,
    reason: Option<String>,
}

/// ZEB-527: `community_id` + `proposal_event_id` for
/// `countersign_admin_proposal`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountersignArgs {
    community_id: String,
    proposal_event_id: String,
}

/// ZEB-714: `set_recovery_designates` (spec §3.1 config ceremony).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetRecoveryDesignatesArgs {
    community_id: String,
    designate_addrs: Vec<String>,
    threshold: u8,
    veto_window_ms: u64,
}

/// ZEB-714: `initiate_admin_recovery` (spec §3.2).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitiateRecoveryArgs {
    community_id: String,
    lost_admin_addr: String,
    new_admin_addr: String,
}

/// ZEB-714: `cosign_admin_recovery` / `veto_admin_recovery` — both take
/// the target proposal's event id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryProposalTargetArgs {
    community_id: String,
    proposal_event_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCommunityArgs {
    name: String,
    is_invite_only: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateInviteArgs {
    community_id: String,
    invitee_hint: Option<String>,
    /// ZEB-564: TTL *duration* in milliseconds (not an absolute epoch). The
    /// server computes `expiry = now_ms + ttlMs`, defaulting to 7 days when
    /// omitted/`null`.
    ttl_ms: Option<u64>,
}

/// Shared by `redeem_invite` and `redeem_friend_token` — both take one
/// `url` parameter.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UrlArgs {
    url: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChannelArgs {
    community_id: String,
    name: String,
    write_power: u8,
    kind: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListChannelMessagesArgs {
    community_id: String,
    channel_id: String,
    since: Option<crate::community_channel_log_engine::HlcDto>,
    limit: u32,
    /// ZEB-602: `"asc"` (default) or `"desc"` for newest-first.
    order: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostChannelMessageArgs {
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
    mentions: Option<Vec<String>>,
    attachments: Option<Vec<crate::community_channel_log_engine::ChannelAttachmentDto>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnsubscribeCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadChannelArtifactArgs {
    community_id: String,
    channel_id: String,
    cid: String,
    dest_path: String,
    max_bytes: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestChannelArtifactArgs {
    community_id: String,
    source_path: String,
    name: Option<String>,
    mime: Option<String>,
    encrypt: Option<bool>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMessageReactionArgs {
    community_id: String,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
    /// ZEB-541: optional custom (CAS-backed) emoji descriptor.
    #[serde(default)]
    custom_emoji: Option<crate::ReactionEmojiInput>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateFriendTokenArgs {
    /// ZEB-507: TTL *duration* in milliseconds (not an absolute epoch). The
    /// server computes `expiresAt = now_ms + ttlMs`. Omitted/`null` → no expiry.
    ttl_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddFriendByKeyArgs {
    identity_pub_hex: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnerIdHexArgs {
    owner_id_hex: String,
}

/// ZEB-236: accept/decline a staged DM invite, keyed by hex `SpaceId`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpaceIdHexArgs {
    space_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddSpaceArgs {
    kind: String,
    name: String,
    members: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendDmArgs {
    space_id: String,
    content: Vec<u8>,
    mime_type: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadDmThreadArgs {
    space_id: String,
    limit: usize,
    before_hlc: Option<u64>,
}

/// ZEB-487: relay opt-in control. Note the existing `CommunityIdArgs` uses
/// `community_id`; the relay-rung RPCs key on `community_id_hex`, so they take
/// their own `…HexArgs` shapes.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetCommunityRelayOptInArgs {
    community_id_hex: String,
    opted_in: bool,
}

/// ZEB-487: `community_id_hex`-keyed arg shape for the relay-status read.
/// Distinct from the existing `CommunityIdArgs` (which keys on `community_id`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommunityIdHexArgs {
    community_id_hex: String,
}

/// ZEB-487: optional community filter for the relay-held observability read.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetRelayHeldArgs {
    #[serde(default)]
    community_id_hex: Option<String>,
}

/// ZEB-489: butler-pin control arg shape. `deviceId` omitted/null → clear.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetButlerPinArgs {
    #[serde(default)]
    device_id: Option<String>,
}

/// ZEB-668 S2: device revocation. `reason` ∈ decommissioned|lost|compromised.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevokeDeviceArgs {
    device_vk_hex: String,
    reason: String,
}

/// ZEB-668 S4: fleet-synced device petname. Empty `petname` clears.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDevicePetnameArgs {
    device_vk_hex: String,
    petname: String,
}

/// ZEB-677 S3: quorum ceremony verbs addressed by request id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuorumRequestIdArgs {
    request_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayNameArgs {
    display_name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerSessionIdArgs {
    peer_session_id: String,
}

/// ZEB-464: shared by the card/profile `get_cached_*` + `unsubscribe_*`
/// verbs — a single `subscriptionId` returned from a prior subscribe.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionIdArgs {
    subscription_id: u64,
}

/// ZEB-464: `subscribe_peer_profile` keys on the peer's owner address hex
/// (`peerAddr`), distinct from the card verbs' `ownerIdHex`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerAddrArgs {
    peer_addr: String,
}

/// ZEB-464: `republish_owner_card` args (avatar/profile-page CIDs optional).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepublishOwnerCardArgs {
    display_name: String,
    status_text: String,
    avatar_cid: Option<String>,
    profile_page_root: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetIdentityDiscoverableArgs {
    enabled: bool,
}

// ── Registry ─────────────────────────────────────────────────────────

/// Build the curated v1 RPC surface (74 commands). Every handler calls
/// the same `*_impl` seam its Tauri wrapper calls, so the GUI and the
/// headless API observe identical behavior and error strings.
pub fn build_registry() -> RpcRegistry {
    let mut m: HashMap<&'static str, RpcHandler> = HashMap::new();

    // Node lifecycle.
    rpc!(
        m,
        "start_node",
        StartNodeArgs,
        |state, sink, a| async move { crate::start_node_inner(a.endpoint, sink, None, state).await }
    );
    rpc!(m, "stop_node", EmptyArgs, |state, sink, _a| async move {
        crate::stop_node_impl(state, sink)
    });

    // Owner state.
    rpc!(m, "get_owner_state", EmptyArgs, |state, _sink, _a| {
        async move { crate::owner_commands::get_owner_state_impl(state).await }
    });
    // ZEB-445 DoD: explicit headless identity bootstrap — first boot is
    // pre-mint; the GUI mints via WelcomeModal. `None` wry_handle: no Tauri
    // runtime headless.
    rpc!(m, "mint_owner_identity", EmptyArgs, |state, sink, _a| {
        async move { crate::owner_commands::mint_owner_identity_impl(state, sink, None).await }
    });

    // Communities.
    rpc!(
        m,
        "list_owner_communities",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_owner_communities_impl(state).await }
    );
    rpc!(
        m,
        "create_community",
        CreateCommunityArgs,
        |state, sink, a| async move {
            crate::create_community_impl(state, sink, a.name, a.is_invite_only).await
        }
    );
    rpc!(
        m,
        "list_community_members",
        CommunityIdArgs,
        |state, _sink, a| async move { crate::list_community_members_impl(state, a.community_id).await }
    );
    rpc!(
        m,
        "generate_invite",
        GenerateInviteArgs,
        |state, _sink, a| async move {
            crate::generate_invite_impl(state, a.community_id, a.invitee_hint, a.ttl_ms).await
        }
    );
    rpc!(m, "redeem_invite", UrlArgs, |state, sink, a| async move {
        crate::redeem_invite_impl(state, sink, a.url).await
    });
    // ZEB-447: the REAL first-contact community-join verb (pkarr-resolve +
    // iroh handshake + allow_no_reticulum_destinations=true). The
    // reticulum-only `redeem_invite` above cold-fails between two
    // never-met nodes, so the two-agent E2E harness drives this instead.
    rpc!(
        m,
        "connectivity_redeem_invite_iroh",
        UrlArgs,
        |state, sink, a| async move {
            crate::connectivity_redeem_invite_iroh_impl(state, sink, a.url).await
        }
    );
    // open-community-cross-wan: open/tokenless first-contact join verb. The
    // Task 12 two-agent E2E harness drives this for never-met open communities.
    rpc!(
        m,
        "connectivity_open_join_iroh",
        UrlArgs,
        |state, sink, a| async move {
            crate::connectivity_open_join_iroh_impl(state, sink, a.url).await
        }
    );
    rpc!(
        m,
        "join_open_community",
        CommunityIdArgs,
        |state, sink, a| async move {
            crate::join_open_community_impl(state, sink, a.community_id).await
        }
    );
    rpc!(
        m,
        "leave_community",
        CommunityIdArgs,
        |state, sink, a| async move { crate::leave_community_impl(state, sink, a.community_id).await }
    );
    // ZEB-435: left-communities management verbs. `list_left_communities`
    // enumerates left-but-not-deleted communities (they're hidden from nav);
    // `remove_space` is the irreversible delete-forever (tombstone + local
    // data cleanup; refuses communities that haven't been left). Headless
    // parity also serves fleet test-community cleanup.
    rpc!(
        m,
        "list_left_communities",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_left_communities_impl(state).await }
    );
    rpc!(
        m,
        "remove_space",
        SpaceIdArgs,
        |state, _sink, a| async move { crate::remove_space_impl(state, a.space_id).await }
    );

    // community moderation (ZEB-527)
    rpc!(
        m,
        "list_pending_joins",
        CommunityIdArgs,
        |state, _sink, a| {
            async move { crate::list_pending_joins_impl(state, a.community_id).await }
        }
    );
    rpc!(
        m,
        "list_recent_counter_signs",
        CommunityLimitArgs,
        |state, _sink, a| async move {
            crate::list_recent_counter_signs_impl(state, a.community_id, a.limit).await
        }
    );
    rpc!(
        m,
        "list_recent_moderation_events",
        CommunityLimitArgs,
        |state, _sink, a| async move {
            crate::list_recent_moderation_events_impl(state, a.community_id, a.limit).await
        }
    );
    rpc!(
        m,
        "countersign_admin_proposal",
        CountersignArgs,
        |state, _sink, a| {
            async move {
                crate::countersign_admin_proposal_impl(state, a.community_id, a.proposal_event_id)
                    .await
            }
        }
    );
    // ZEB-714: admin-recovery verbs (D2) — full parity with the Tauri
    // surface so the D3 e2e can drive both sides headlessly.
    rpc!(
        m,
        "get_recovery_state",
        CommunityIdArgs,
        |state, _sink, a| async move { crate::get_recovery_state_impl(state, a.community_id).await }
    );
    rpc!(
        m,
        "set_recovery_designates",
        SetRecoveryDesignatesArgs,
        |state, _sink, a| {
            async move {
                crate::set_recovery_designates_impl(
                    state,
                    a.community_id,
                    a.designate_addrs,
                    a.threshold,
                    a.veto_window_ms,
                )
                .await
            }
        }
    );
    rpc!(
        m,
        "initiate_admin_recovery",
        InitiateRecoveryArgs,
        |state, _sink, a| {
            async move {
                crate::initiate_admin_recovery_impl(
                    state,
                    a.community_id,
                    a.lost_admin_addr,
                    a.new_admin_addr,
                )
                .await
            }
        }
    );
    rpc!(
        m,
        "cosign_admin_recovery",
        RecoveryProposalTargetArgs,
        |state, _sink, a| {
            async move {
                crate::cosign_admin_recovery_impl(state, a.community_id, a.proposal_event_id).await
            }
        }
    );
    rpc!(
        m,
        "veto_admin_recovery",
        RecoveryProposalTargetArgs,
        |state, _sink, a| {
            async move {
                crate::veto_admin_recovery_impl(state, a.community_id, a.proposal_event_id).await
            }
        }
    );
    rpc!(
        m,
        "kick_from_community",
        ModerationTargetArgs,
        |state, _sink, a| {
            async move {
                crate::kick_from_community_impl(state, a.community_id, a.target_addr, a.reason)
                    .await
            }
        }
    );
    rpc!(
        m,
        "unban_from_community",
        ModerationTargetArgs,
        |state, _sink, a| {
            async move {
                crate::unban_from_community_impl(state, a.community_id, a.target_addr, a.reason)
                    .await
            }
        }
    );

    // Channels.
    rpc!(
        m,
        "create_channel",
        CreateChannelArgs,
        |state, _sink, a| async move {
            crate::create_channel_impl(state, a.community_id, a.name, a.write_power, a.kind).await
        }
    );
    rpc!(
        m,
        "list_channels",
        CommunityIdArgs,
        |state, _sink, a| async move { crate::list_channels_impl(state, a.community_id).await }
    );
    rpc!(
        m,
        "list_channel_messages",
        ListChannelMessagesArgs,
        |state, _sink, a| async move {
            crate::list_channel_messages_impl(
                state,
                a.community_id,
                a.channel_id,
                a.since,
                a.limit,
                a.order,
            )
            .await
        }
    );
    rpc!(
        m,
        "post_channel_message",
        PostChannelMessageArgs,
        |state, _sink, a| async move {
            crate::post_channel_message_impl(
                state,
                a.community_id,
                a.channel_id,
                a.body,
                a.reply_to,
                a.mentions,
                a.attachments,
            )
            .await
        }
    );
    rpc!(
        m,
        "download_channel_artifact",
        DownloadChannelArtifactArgs,
        |state, _sink, a| async move {
            crate::download_channel_artifact_impl(
                state,
                a.community_id,
                a.channel_id,
                a.cid,
                a.dest_path,
                a.max_bytes,
            )
            .await
        }
    );
    rpc!(
        m,
        "ingest_channel_artifact",
        IngestChannelArtifactArgs,
        |state, _sink, a| async move {
            crate::ingest_channel_artifact_impl(
                state,
                a.community_id,
                a.source_path,
                a.name,
                a.mime,
                a.encrypt.unwrap_or(true),
            )
            .await
        }
    );
    rpc!(
        m,
        "set_message_reaction",
        SetMessageReactionArgs,
        |state, _sink, a| async move {
            crate::set_message_reaction_impl(
                state,
                a.community_id,
                a.channel_id,
                a.message_id,
                a.emoji,
                a.add,
                a.custom_emoji,
            )
            .await
        }
    );

    // Community presence (ZEB-537).
    rpc!(
        m,
        "subscribe_community_presence",
        SubscribeCommunityPresenceArgs,
        |state, _sink, a| async move {
            crate::subscribe_community_presence_impl(state, a.community_id).await
        }
    );
    rpc!(
        m,
        "unsubscribe_community_presence",
        UnsubscribeCommunityPresenceArgs,
        |state, _sink, a| async move {
            crate::unsubscribe_community_presence_impl(state, a.community_id).await
        }
    );
    rpc!(
        m,
        "get_community_presence",
        GetCommunityPresenceArgs,
        |state, _sink, a| async move { crate::get_community_presence_impl(state, a.community_id).await }
    );

    // Vines (publish → feed → view → reshare; drives the two-node e2e harness).
    rpc!(
        m,
        "publish_vine",
        crate::PublishVineArgs,
        |state, _sink, a| async move { crate::publish_vine_impl(state, a).await }
    );
    rpc!(
        m,
        "list_vine_videos",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_vine_videos_impl(state) }
    );
    rpc!(
        m,
        "list_vine_reactions",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_vine_reactions_impl(state) }
    );
    rpc!(
        m,
        "mark_vine_viewed",
        VineIdArgs,
        |state, _sink, a| async move {
            // Return an object `{ viewed }` (not a bare bool) to match the
            // documented contract and the publish/reshare object shapes. (Qodo)
            crate::mark_vine_viewed_impl(state, a.vine_id)
                .map(|viewed| serde_json::json!({ "viewed": viewed }))
        }
    );
    rpc!(
        m,
        "reshare_vine",
        crate::ReshareVineArgs,
        |state, _sink, a| async move { crate::reshare_vine_impl(state, a).await }
    );
    // ZEB-670: creator-signed delete verb (tombstone publish).
    rpc!(m, "delete_vine", VineIdArgs, |state, _sink, a| async move {
        crate::delete_vine_impl(state, a.vine_id).await
    });
    // ZEB-562: vine-follow verbs for the agent-driven e2e harness — parity with
    // the GUI follow surface, mirroring the ZEB-552 vine RPC pattern.
    rpc!(
        m,
        "follow_vine_creator",
        FollowVineCreatorArgs,
        |state, _sink, a| async move {
            crate::follow_vine_creator_impl(state, a.address, a.name)
                .map(|followed| serde_json::json!({ "followed": followed }))
        }
    );
    rpc!(
        m,
        "unfollow_vine_creator",
        UnfollowVineCreatorArgs,
        |state, _sink, a| async move {
            crate::unfollow_vine_creator_impl(state, a.address)
                .map(|unfollowed| serde_json::json!({ "unfollowed": unfollowed }))
        }
    );
    rpc!(
        m,
        "list_followed",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_followed_impl(state) }
    );

    // ZEB-669 S2: storage-buddy verbs — headless parity for the pact
    // surface (invite/accept/remove/budget/meter).
    rpc!(
        m,
        "get_storage_buddies",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_storage_buddies_impl(state) }
    );
    rpc!(
        m,
        "set_buddy_pledge",
        SetBuddyPledgeArgs,
        |state, sink, a| async move {
            crate::set_buddy_pledge_impl(state, sink.as_ref(), a.owner_address, a.bytes)
        }
    );
    rpc!(
        m,
        "remove_storage_buddy",
        RemoveStorageBuddyArgs,
        |state, sink, a| async move {
            crate::remove_storage_buddy_impl(state, sink.as_ref(), a.owner_address)
        }
    );
    rpc!(
        m,
        "set_shared_budget",
        SetSharedBudgetArgs,
        |state, sink, a| async move { crate::set_shared_budget_impl(state, sink.as_ref(), a.bytes) }
    );
    rpc!(
        m,
        "get_contribution_summary",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_contribution_summary_impl(state) }
    );
    rpc!(
        m,
        "set_backup_flag",
        SetBackupFlagArgs,
        |state, sink, a| async move {
            crate::set_backup_flag_impl(state, sink.as_ref(), a.sidecar_id, a.backup)
        }
    );

    // Friends.
    rpc!(m, "list_friends", EmptyArgs, |state, _sink, _a| {
        async move { crate::list_friends_impl(state).await }
    });
    rpc!(
        m,
        "generate_friend_token",
        GenerateFriendTokenArgs,
        |state, _sink, a| async move { crate::generate_friend_token_impl(state, a.ttl_ms).await }
    );
    rpc!(m, "redeem_friend_token", UrlArgs, |state, sink, a| {
        async move { crate::redeem_friend_token_impl(state, sink, a.url).await }
    });
    rpc!(
        m,
        "add_friend_by_key",
        AddFriendByKeyArgs,
        |state, sink, a| async move {
            crate::add_friend_by_key_impl(state, sink, a.identity_pub_hex).await
        }
    );
    rpc!(
        m,
        "list_pending_friend_requests",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_pending_friend_requests_impl(state).await }
    );
    rpc!(
        m,
        "accept_friend_request",
        OwnerIdHexArgs,
        |state, sink, a| async move {
            crate::accept_friend_request_impl(state, sink, a.owner_id_hex).await
        }
    );
    rpc!(
        m,
        "decline_friend_request",
        OwnerIdHexArgs,
        |state, sink, a| async move {
            crate::decline_friend_request_impl(state, sink, a.owner_id_hex).await
        }
    );

    // DM invites (ZEB-236): the staged non-friend invite consent trio.
    rpc!(
        m,
        "list_pending_dm_invites",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_pending_dm_invites_impl(state).await }
    );
    rpc!(
        m,
        "accept_dm_invite",
        SpaceIdHexArgs,
        |state, sink, a| async move { crate::accept_dm_invite_impl(state, sink, a.space_id).await }
    );
    rpc!(
        m,
        "decline_dm_invite",
        SpaceIdHexArgs,
        |state, sink, a| async move { crate::decline_dm_invite_impl(state, sink, a.space_id).await }
    );

    // Spaces / DMs.
    rpc!(m, "add_space", AddSpaceArgs, |state, _sink, a| async move {
        crate::add_space_impl(state, a.kind, a.name, a.members).await
    });
    rpc!(m, "send_dm", SendDmArgs, |state, _sink, a| async move {
        crate::send_dm_impl(state, a.space_id, a.content, a.mime_type).await
    });
    rpc!(
        m,
        "read_dm_thread",
        ReadDmThreadArgs,
        |state, _sink, a| async move {
            crate::read_dm_thread_impl(state, a.space_id, a.limit, a.before_hlc).await
        }
    );
    // DM nav rehydration (ZEB-666).
    rpc!(
        m,
        "list_owner_dm_spaces",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_owner_dm_spaces_impl(state).await }
    );

    // Relay rung (ZEB-487).
    rpc!(
        m,
        "set_community_relay_opt_in",
        SetCommunityRelayOptInArgs,
        |state, _sink, a| async move {
            crate::set_community_relay_opt_in_impl(state, a.community_id_hex, a.opted_in).await
        }
    );
    rpc!(
        m,
        "get_community_relay_status",
        CommunityIdHexArgs,
        |state, _sink, a| async move {
            crate::get_community_relay_status_impl(state, a.community_id_hex).await
        }
    );
    rpc!(
        m,
        "get_relay_held",
        GetRelayHeldArgs,
        |state, _sink, a| async move { crate::get_relay_held_impl(state, a.community_id_hex).await }
    );

    // Butler rung (ZEB-489).
    rpc!(
        m,
        "set_butler_pin",
        SetButlerPinArgs,
        |state, _sink, a| async move { crate::set_butler_pin_impl(state, a.device_id).await }
    );
    rpc!(
        m,
        "get_butler_pin",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_butler_pin_impl(state).await }
    );
    rpc!(
        m,
        "get_butler_held",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_butler_held_impl(state).await }
    );

    // Device management (ZEB-668 S2).
    rpc!(
        m,
        "revoke_device",
        RevokeDeviceArgs,
        |state, sink, a| async move {
            crate::owner_commands::revoke_device_impl(state, sink, a.device_vk_hex, a.reason).await
        }
    );
    rpc!(
        m,
        "set_device_petname",
        SetDevicePetnameArgs,
        |state, sink, a| async move {
            crate::set_device_petname_impl(state, sink, a.device_vk_hex, a.petname).await
        }
    );
    // ZEB-677 S3: quorum revocation ceremony.
    rpc!(
        m,
        "request_quorum_revocation",
        RevokeDeviceArgs,
        |state, sink, a| async move {
            crate::owner_quorum_commands::request_quorum_revocation_impl(
                state,
                sink,
                a.device_vk_hex,
                a.reason,
            )
            .await
        }
    );
    rpc!(
        m,
        "cosign_quorum_request",
        QuorumRequestIdArgs,
        |state, sink, a| async move {
            crate::owner_quorum_commands::cosign_quorum_request_impl(state, sink, a.request_id)
                .await
        }
    );
    rpc!(
        m,
        "decline_quorum_request",
        QuorumRequestIdArgs,
        |state, sink, a| async move {
            crate::owner_quorum_commands::decline_quorum_request_impl(state, sink, a.request_id)
                .await
        }
    );
    // ZEB-677 S4: pre-armed enrollment co-sign window.
    rpc!(
        m,
        "arm_quorum_enrollment",
        EmptyArgs,
        |state, sink, _a| async move {
            crate::owner_quorum_commands::arm_quorum_enrollment_impl(state, sink).await
        }
    );
    rpc!(
        m,
        "disarm_quorum_enrollment",
        EmptyArgs,
        |state, sink, _a| async move {
            crate::owner_quorum_commands::disarm_quorum_enrollment_impl(state, sink).await
        }
    );
    // ZEB-677 S5: standalone quorum fleet-epoch rotation.
    rpc!(
        m,
        "request_quorum_epoch_bump",
        EmptyArgs,
        |state, sink, _a| async move {
            crate::owner_quorum_commands::request_quorum_epoch_bump_impl(state, sink).await
        }
    );
    rpc!(
        m,
        "bump_fleet_epoch",
        EmptyArgs,
        |state, sink, _a| async move { crate::bump_fleet_epoch_impl(state, sink).await }
    );

    // Connectivity.
    rpc!(
        m,
        "connectivity_get_my_reachability_record",
        EmptyArgs,
        |state, _sink, _a| async move {
            crate::connectivity_get_my_reachability_record_impl(state).await
        }
    );
    rpc!(
        m,
        "connectivity_get_my_identity_pub_hex",
        EmptyArgs,
        |state, _sink, _a| async move { crate::connectivity_get_my_identity_pub_hex_impl(state).await }
    );
    rpc!(
        m,
        "connectivity_list_peer_reachability",
        EmptyArgs,
        |state, _sink, _a| async move { crate::connectivity_list_peer_reachability_impl(state).await }
    );
    rpc!(
        m,
        "connectivity_set_identity_discoverable",
        SetIdentityDiscoverableArgs,
        |state, _sink, a| {
            async move { crate::connectivity_set_identity_discoverable_impl(state, a.enabled).await }
        }
    );
    rpc!(
        m,
        "connectivity_get_identity_discoverable",
        EmptyArgs,
        |state, _sink, _a| {
            async move { crate::connectivity_get_identity_discoverable_impl(state).await }
        }
    );

    // Network health.
    rpc!(
        m,
        "network_health_snapshot",
        EmptyArgs,
        |state, _sink, _a| async move { crate::network_health_snapshot_impl(state).await }
    );
    rpc!(
        m,
        "network_health_run_self_test",
        EmptyArgs,
        |state, _sink, _a| async move { crate::network_health_run_self_test_impl(state).await }
    );

    // Pairing (ZEB-446): enroll a second local instance — e.g. the pinned
    // headless coordination node — into the owner's fleet without a GUI.
    // SAS verification flows through get_pairing_state polling on both
    // sides; the joiner side runs on the headless instance.
    rpc!(
        m,
        "start_inviter_pairing",
        DisplayNameArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::start_inviter_pairing_inner(state, a.display_name).await
        }
    );
    rpc!(
        m,
        "start_joiner_pairing",
        DisplayNameArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::start_joiner_pairing_inner(state, a.display_name).await
        }
    );
    rpc!(
        m,
        "select_pairing_peer",
        PeerSessionIdArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::select_pairing_peer_inner(state, a.peer_session_id).await
        }
    );
    rpc!(
        m,
        "confirm_pairing_sas",
        EmptyArgs,
        |state, _sink, _a| async move {
            crate::pairing_commands::confirm_pairing_sas_inner(state).await
        }
    );
    rpc!(m, "cancel_pairing", EmptyArgs, |state, _sink, _a| {
        async move { crate::pairing_commands::cancel_pairing_inner(state).await }
    });
    rpc!(
        m,
        "get_pairing_state",
        EmptyArgs,
        |state, _sink, _a| async move { crate::pairing_commands::get_pairing_state_inner(state).await }
    );

    // Profile cards (ZEB-341) + peer-profile broadcast (ZEB-281) — ZEB-464.
    // The card/profile pub-sub runtime is wired in `start_node_inner` (shared
    // by `serve`), so these expose the GUI's exact behavior headless — the
    // last cross-peer surface the two-agent E2E suite couldn't drive. Cards
    // ride a Zenoh broadcast topic keyed by owner_id, so propagation is
    // exercisable both co-located and cross-WAN.
    rpc!(
        m,
        "republish_owner_card",
        RepublishOwnerCardArgs,
        |state, _sink, a| async move {
            crate::republish_owner_card_impl(
                state,
                a.display_name,
                a.status_text,
                a.avatar_cid,
                a.profile_page_root,
            )
            .await
        }
    );
    rpc!(
        m,
        "subscribe_member_card",
        OwnerIdHexArgs,
        |state, _sink, a| async move { crate::subscribe_member_card_impl(state, a.owner_id_hex).await }
    );
    rpc!(
        m,
        "get_cached_member_card",
        SubscriptionIdArgs,
        |state, _sink, a| async move {
            crate::get_cached_member_card_impl(state, a.subscription_id).await
        }
    );
    rpc!(
        m,
        "unsubscribe_member_card",
        SubscriptionIdArgs,
        |state, _sink, a| async move {
            crate::unsubscribe_member_card_impl(state, a.subscription_id).await
        }
    );
    rpc!(
        m,
        "subscribe_peer_profile",
        PeerAddrArgs,
        |state, _sink, a| async move { crate::subscribe_peer_profile_impl(state, a.peer_addr).await }
    );
    rpc!(
        m,
        "get_cached_peer_profile",
        SubscriptionIdArgs,
        |state, _sink, a| async move {
            crate::get_cached_peer_profile_impl(state, a.subscription_id).await
        }
    );
    rpc!(
        m,
        "unsubscribe_peer_profile",
        SubscriptionIdArgs,
        |state, _sink, a| async move {
            crate::unsubscribe_peer_profile_impl(state, a.subscription_id).await
        }
    );

    RpcRegistry { handlers: m }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::FanoutSink;
    use crate::NodeState;
    use std::sync::Mutex;

    const DUMMY_COMMUNITY_HEX: &str = "00000000000000000000000000000000"; // 16 bytes

    fn test_sink() -> Arc<dyn NodeEventSink> {
        Arc::new(FanoutSink(vec![]))
    }

    fn test_state() -> Arc<Mutex<NodeState>> {
        Arc::new(Mutex::new(NodeState::default()))
    }

    #[tokio::test]
    async fn unknown_command_is_distinct_from_command_error() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "no_such_cmd",
                test_state(),
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::UnknownCommand),
            "expected UnknownCommand, got {err:?}"
        );
    }

    #[tokio::test]
    async fn bad_args_reports_serde_message() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_channels",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": 42 }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::BadArgs(msg) => {
                assert!(!msg.is_empty(), "serde message should be non-empty")
            }
            other => panic!("expected BadArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_error_passes_through_ipc_error_string() {
        let reg = build_registry();
        // Valid-shape hex id, but the default NodeState has no owner loaded:
        // the _impl seam fails with the same error string the GUI would see.
        let err = reg
            .dispatch(
                "list_channels",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": "00112233445566778899aabbccddeeff" }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert!(!msg.is_empty(), "IPC error string should be non-empty")
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vine_follow_rpcs_are_registered_and_wired() {
        // ZEB-562: dispatching the three follow verbs with valid args on a
        // default (not-connected) NodeState must reach the `_impl` seam and
        // surface its `Command("not connected")` error — NOT `UnknownCommand`
        // (would mean unregistered) and NOT `BadArgs` (would mean the arg
        // struct rejected the shape). Proves registration + arg parse + wiring.
        let reg = build_registry();
        let cases = [
            (
                "follow_vine_creator",
                serde_json::json!({ "address": "abcd1234", "name": "Alice" }),
            ),
            (
                "unfollow_vine_creator",
                serde_json::json!({ "address": "abcd1234" }),
            ),
            ("list_followed", serde_json::json!({})),
        ];
        for (method, args) in cases {
            let err = reg
                .dispatch(method, test_state(), test_sink(), args)
                .await
                .unwrap_err();
            assert!(
                matches!(err, RpcError::Command(_)),
                "{method}: expected Command (not connected), got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn storage_buddy_rpcs_are_registered_and_wired() {
        // ZEB-669 S2 (PR #449 review): same proof shape as the vine-follow
        // parity test — valid-shaped args on a default NodeState must reach
        // the `_impl` seam (Ok or a Command error from the seam itself),
        // never UnknownCommand (unregistered) or BadArgs (arg-struct
        // mismatch with the Tauri wrapper's camelCase shape).
        let reg = build_registry();
        let owner = "ab".repeat(16); // valid 32-hex owner address
        let cases = [
            ("get_storage_buddies", serde_json::json!({})),
            (
                "set_buddy_pledge",
                serde_json::json!({ "ownerAddress": owner, "bytes": 1024 }),
            ),
            (
                "remove_storage_buddy",
                serde_json::json!({ "ownerAddress": owner }),
            ),
            (
                "set_shared_budget",
                serde_json::json!({ "bytes": 5_000_000 }),
            ),
            ("get_contribution_summary", serde_json::json!({})),
            (
                "set_backup_flag",
                serde_json::json!({
                    "sidecarId": "00000000-0000-4000-8000-000000000000",
                    "backup": true
                }),
            ),
        ];
        for (method, args) in cases {
            match reg.dispatch(method, test_state(), test_sink(), args).await {
                Err(RpcError::UnknownCommand) => panic!("{method} must be registered"),
                Err(RpcError::BadArgs(msg)) => {
                    panic!("{method}: arg struct rejected the wrapper shape: {msg}")
                }
                Ok(_) | Err(RpcError::Command(_)) => {}
            }
        }
    }

    /// ZEB-668 S2 (PR #452 review): same proof shape as the storage-buddy
    /// parity test — camelCase args must parse and reach the `_impl` seam.
    /// `identity_dir` pins to an empty tempdir so the seam deterministically
    /// answers `noOwner:` without ever touching the developer's real
    /// identity directory.
    #[tokio::test]
    async fn revoke_device_rpc_is_registered_and_wired() {
        let reg = build_registry();
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..NodeState::default()
        }));
        let args = serde_json::json!({
            "deviceVkHex": "ab".repeat(32),
            "reason": "decommissioned",
        });
        match reg
            .dispatch("revoke_device", state, test_sink(), args)
            .await
        {
            Err(RpcError::UnknownCommand) => panic!("revoke_device must be registered"),
            Err(RpcError::BadArgs(msg)) => {
                panic!("revoke_device: arg struct rejected the wrapper shape: {msg}")
            }
            Err(RpcError::Command(msg)) => {
                assert!(
                    msg.starts_with("noOwner:"),
                    "expected the no-identity seam error, got: {msg}"
                );
            }
            Ok(v) => panic!("expected noOwner error on an empty identity dir, got {v:?}"),
        }
    }

    /// ZEB-668 S5: dispatch proof for `bump_fleet_epoch` (no args). A
    /// default NodeState has no fleet-keys carrier, so the seam
    /// deterministically answers "carrier not running" without any
    /// identity/keychain access.
    #[tokio::test]
    async fn bump_fleet_epoch_rpc_is_registered_and_wired() {
        let reg = build_registry();
        let state = Arc::new(Mutex::new(NodeState::default()));
        match reg
            .dispatch(
                "bump_fleet_epoch",
                state,
                test_sink(),
                serde_json::json!({}),
            )
            .await
        {
            Err(RpcError::UnknownCommand) => panic!("bump_fleet_epoch must be registered"),
            Err(RpcError::BadArgs(msg)) => {
                panic!("bump_fleet_epoch: EmptyArgs rejected the wrapper shape: {msg}")
            }
            Err(RpcError::Command(msg)) => {
                assert!(
                    msg.contains("fleet-keys carrier not running"),
                    "expected the node-not-started seam error, got: {msg}"
                );
            }
            Ok(v) => panic!("expected node-not-started error on a default NodeState, got {v:?}"),
        }
    }

    /// ZEB-668 S4 (PR #454 review): dispatch proof for `set_device_petname`,
    /// mirroring `revoke_device_rpc_is_registered_and_wired` — camelCase args
    /// must parse and reach the `_impl` seam. A default NodeState has no
    /// fleet-net doc, so the seam deterministically answers "fleet-net not
    /// running" without any identity/keychain access.
    #[tokio::test]
    async fn set_device_petname_rpc_is_registered_and_wired() {
        let reg = build_registry();
        let state = Arc::new(Mutex::new(NodeState::default()));
        let args = serde_json::json!({
            "deviceVkHex": "ab".repeat(32),
            "petname": "KRILE",
        });
        match reg
            .dispatch("set_device_petname", state, test_sink(), args)
            .await
        {
            Err(RpcError::UnknownCommand) => panic!("set_device_petname must be registered"),
            Err(RpcError::BadArgs(msg)) => {
                panic!("set_device_petname: arg struct rejected the wrapper shape: {msg}")
            }
            Err(RpcError::Command(msg)) => {
                assert!(
                    msg.contains("fleet-net not running"),
                    "expected the node-not-started seam error, got: {msg}"
                );
            }
            Ok(v) => panic!("expected node-not-started error on a default NodeState, got {v:?}"),
        }
    }

    /// ZEB-714: dispatch proof for the five admin-recovery verbs — each
    /// must be registered and its camelCase arg struct must accept the
    /// wire shape. On a default NodeState every impl deterministically
    /// fails at the owner-not-loaded seam (a `Command` error), which is
    /// exactly the proof that registration + arg parsing succeeded.
    #[tokio::test]
    async fn recovery_rpcs_are_registered_and_wired() {
        let reg = build_registry();
        let community_id = "c0".repeat(16);
        let proposal_target = serde_json::json!({
            "communityId": community_id,
            "proposalEventId": "b0".repeat(16),
        });
        let cases: Vec<(&str, serde_json::Value)> = vec![
            (
                "get_recovery_state",
                serde_json::json!({ "communityId": community_id }),
            ),
            (
                "set_recovery_designates",
                serde_json::json!({
                    "communityId": community_id,
                    "designateAddrs": ["11".repeat(16), "12".repeat(16)],
                    "threshold": 2,
                    "vetoWindowMs": 30u64 * 86_400_000,
                }),
            ),
            (
                "initiate_admin_recovery",
                serde_json::json!({
                    "communityId": community_id,
                    "lostAdminAddr": "01".repeat(16),
                    "newAdminAddr": "21".repeat(16),
                }),
            ),
            ("cosign_admin_recovery", proposal_target.clone()),
            ("veto_admin_recovery", proposal_target),
        ];
        for (verb, args) in cases {
            let state = Arc::new(Mutex::new(NodeState::default()));
            match reg.dispatch(verb, state, test_sink(), args).await {
                Err(RpcError::UnknownCommand) => panic!("{verb} must be registered"),
                Err(RpcError::BadArgs(msg)) => {
                    panic!("{verb}: arg struct rejected the wire shape: {msg}")
                }
                // Owner-not-loaded (or a friendly pre-check) — args
                // parsed, impl reached.
                Err(RpcError::Command(_)) => {}
                Ok(v) => {
                    panic!("{verb}: expected owner-not-loaded error on default state, got {v:?}")
                }
            }
        }
    }

    #[tokio::test]
    async fn null_args_treated_as_empty() {
        let reg = build_registry();
        // `null` body must be treated as `{}` — a no-arg command must not
        // reject it as BadArgs (POST with empty body deserializes to Null).
        let res = reg
            .dispatch(
                "list_friends",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await;
        match res {
            Err(RpcError::BadArgs(msg)) => panic!("null args must not be BadArgs: {msg}"),
            Err(RpcError::UnknownCommand) => panic!("list_friends must be registered"),
            // Ok or a Command error (owner not loaded on default state) are
            // both acceptable — the point is args parsing succeeded.
            Ok(_) | Err(RpcError::Command(_)) => {}
        }
    }

    #[tokio::test]
    async fn pairing_commands_dispatch_with_ipc_parity_pre_node() {
        let reg = build_registry();
        // Pre-node, every pairing command must fail with the SAME error string
        // the Tauri IPC layer produces — proving the seam is shared, not forked.
        let err = reg
            .dispatch(
                "get_pairing_state",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert_eq!(msg, "pairing not initialized — start node first")
            }
            other => panic!("expected Command, got {other:?}"),
        }
        // Args parse: camelCase displayName reaches the seam (the seam then
        // fails pre-node, which is fine — BadArgs would mean parsing broke).
        let err = reg
            .dispatch(
                "start_joiner_pairing",
                test_state(),
                test_sink(),
                serde_json::json!({ "displayName": "coord-device" }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "displayName must parse (got {err:?})"
        );
    }

    #[tokio::test]
    async fn card_and_profile_commands_dispatch_with_ipc_parity_pre_node() {
        // ZEB-464: the profile-card (ZEB-341) + peer-profile broadcast
        // (ZEB-281) verbs must dispatch through the SAME `*_impl` seam the
        // Tauri IPC layer uses, observing identical pre-node error strings.
        let reg = build_registry();

        // The owner-not-loaded read/subscribe verbs all surface the shared
        // OWNER_NOT_LOADED_MSG (proves the seam is shared, args parsed).
        for (cmd, args) in [
            (
                "subscribe_member_card",
                serde_json::json!({ "ownerIdHex": "00112233445566778899aabbccddeeff" }),
            ),
            (
                "get_cached_member_card",
                serde_json::json!({ "subscriptionId": 1 }),
            ),
            (
                "unsubscribe_member_card",
                serde_json::json!({ "subscriptionId": 1 }),
            ),
            (
                "subscribe_peer_profile",
                serde_json::json!({ "peerAddr": "00112233445566778899aabbccddeeff" }),
            ),
            (
                "get_cached_peer_profile",
                serde_json::json!({ "subscriptionId": 1 }),
            ),
            (
                "unsubscribe_peer_profile",
                serde_json::json!({ "subscriptionId": 1 }),
            ),
        ] {
            let err = reg
                .dispatch(cmd, test_state(), test_sink(), args)
                .await
                .unwrap_err();
            match err {
                RpcError::Command(msg) => assert_eq!(
                    msg,
                    crate::OWNER_NOT_LOADED_MSG,
                    "{cmd} must share the IPC owner-not-loaded error string"
                ),
                other => panic!("{cmd}: expected Command, got {other:?}"),
            }
        }

        // republish_owner_card parses its camelCase args, then fails pre-node
        // with the publish-side runtime error (proves args parsed + seam ran).
        let err = reg
            .dispatch(
                "republish_owner_card",
                test_state(),
                test_sink(),
                serde_json::json!({ "displayName": "ZEBbot", "statusText": "gm" }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert_eq!(msg, "owner card runtime not ready")
            }
            other => panic!("republish_owner_card: expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dm_invite_rpcs_dispatch_with_ipc_parity_pre_node() {
        // ZEB-236: the DM-invite consent trio must dispatch through the SAME
        // `*_impl` seams the Tauri IPC layer uses, observing identical pre-node
        // behavior (list → empty, accept/decline → owner-not-loaded).
        let reg = build_registry();

        // Pre-node the staged-invite store isn't wired → empty list, not error
        // (mirrors `list_pending_friend_requests`).
        let out = reg
            .dispatch(
                "list_pending_dm_invites",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .expect("verb registered + dispatches");
        assert_eq!(out, serde_json::json!([]));

        // Accept/decline parse the camelCase `spaceId` (BadArgs would mean
        // parsing broke), then fail pre-node with the shared IPC error string.
        for cmd in ["accept_dm_invite", "decline_dm_invite"] {
            let err = reg
                .dispatch(
                    cmd,
                    test_state(),
                    test_sink(),
                    serde_json::json!({ "spaceId": "00112233445566778899aabbccddeeff" }),
                )
                .await
                .unwrap_err();
            match err {
                RpcError::Command(msg) => assert_eq!(
                    msg,
                    crate::OWNER_NOT_LOADED_MSG,
                    "{cmd} must share the IPC owner-not-loaded error string"
                ),
                other => panic!("{cmd}: expected Command, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn list_owner_dm_spaces_dispatches_with_ipc_parity_pre_node() {
        // ZEB-666: must dispatch through the SAME `*_impl` seam the Tauri
        // IPC layer uses, observing the shared pre-node error string.
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_owner_dm_spaces",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => assert_eq!(
                msg,
                crate::OWNER_NOT_LOADED_MSG,
                "must share the IPC owner-not-loaded error string"
            ),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connectivity_get_my_identity_pub_hex_returns_null_pre_owner() {
        let reg = build_registry();
        let out = reg
            .dispatch(
                "connectivity_get_my_identity_pub_hex",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .expect("verb registered + dispatches");
        assert_eq!(out, serde_json::Value::Null); // Option<String>::None → JSON null
    }

    #[tokio::test]
    async fn connectivity_get_identity_discoverable_defaults_false() {
        let reg = build_registry();
        let out = reg
            .dispatch(
                "connectivity_get_identity_discoverable",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .expect("verb registered");
        assert_eq!(out, serde_json::Value::Bool(false));
    }

    #[tokio::test]
    async fn connectivity_set_identity_discoverable_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "connectivity_set_identity_discoverable",
                test_state(),
                test_sink(),
                serde_json::json!({ "enabled": true }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "expected Command, got {err:?}"
        );
    }

    #[tokio::test]
    async fn connectivity_set_identity_discoverable_rejects_missing_enabled() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "connectivity_set_identity_discoverable",
                test_state(),
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::BadArgs(_)),
            "expected BadArgs, got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_pending_joins_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_pending_joins",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn list_pending_joins_rejects_missing_community_id() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_pending_joins",
                test_state(),
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::BadArgs(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn list_recent_counter_signs_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_recent_counter_signs",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "limit": 20 }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn list_recent_moderation_events_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_recent_moderation_events",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "limit": 20 }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn countersign_admin_proposal_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "countersign_admin_proposal",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "proposalEventId": DUMMY_COMMUNITY_HEX }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn kick_from_community_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "kick_from_community",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "targetAddr": DUMMY_COMMUNITY_HEX }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn kick_from_community_rejects_missing_target() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "kick_from_community",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::BadArgs(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn unban_from_community_errs_pre_owner() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "unban_from_community",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "targetAddr": DUMMY_COMMUNITY_HEX }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn list_vine_reactions_errs_not_connected_pre_node() {
        // ZEB-672: pins the registration + the "not connected" contract
        // shared with list_vine_videos (frontend hydrate calls it only
        // after connectAdapter succeeds).
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_vine_reactions",
                test_state(),
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert!(msg.contains("not connected"), "got {msg:?}")
            }
            other => panic!("expected Command(\"not connected\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_vine_errs_not_connected_pre_node() {
        // ZEB-670: pins the registration + the "not connected" contract.
        // (The signer / ownership checks are covered by delete_vine_impl's
        // own tests in lib.rs and the cache's tombstone tests.)
        let reg = build_registry();
        let err = reg
            .dispatch(
                "delete_vine",
                test_state(),
                test_sink(),
                serde_json::json!({ "vineId": "vine-1" }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert!(msg.contains("not connected"), "got {msg:?}")
            }
            other => panic!("expected Command(\"not connected\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_has_exactly_the_curated_v1_surface() {
        let reg = build_registry();
        let mut names = reg.command_names();
        names.sort_unstable();
        // The complete curated v1 surface — any addition, rename, or
        // removal must update this list deliberately (PR #245 round 3,
        // CodeRabbit: a count + spot checks let a swap inside the set
        // slip through).
        let mut expected = vec![
            // node lifecycle
            "start_node",
            "stop_node",
            // identity
            "get_owner_state",
            "mint_owner_identity",
            // communities
            "list_owner_communities",
            "create_community",
            "list_community_members",
            "generate_invite",
            "redeem_invite",
            "join_open_community",
            "leave_community",
            "list_left_communities",
            "remove_space",
            // community moderation (ZEB-527)
            "list_pending_joins",
            "list_recent_counter_signs",
            "list_recent_moderation_events",
            "countersign_admin_proposal",
            "kick_from_community",
            "unban_from_community",
            // admin recovery (ZEB-714)
            "get_recovery_state",
            "set_recovery_designates",
            "initiate_admin_recovery",
            "cosign_admin_recovery",
            "veto_admin_recovery",
            // channels
            "create_channel",
            "list_channels",
            "list_channel_messages",
            "post_channel_message",
            "set_message_reaction",
            // channel artifacts (CAS)
            "ingest_channel_artifact",
            "download_channel_artifact",
            // community presence (ZEB-537)
            "subscribe_community_presence",
            "unsubscribe_community_presence",
            "get_community_presence",
            // vines
            "publish_vine",
            "list_vine_videos",
            "list_vine_reactions",
            "mark_vine_viewed",
            "reshare_vine",
            "delete_vine",
            // vine follows (ZEB-562)
            "follow_vine_creator",
            "unfollow_vine_creator",
            "list_followed",
            // storage buddies (ZEB-669 S2)
            "get_storage_buddies",
            "set_buddy_pledge",
            "remove_storage_buddy",
            "set_shared_budget",
            "get_contribution_summary",
            "set_backup_flag",
            // friends
            "list_friends",
            "generate_friend_token",
            "redeem_friend_token",
            "add_friend_by_key",
            "list_pending_friend_requests",
            "accept_friend_request",
            "decline_friend_request",
            // DM invites (ZEB-236)
            "list_pending_dm_invites",
            "accept_dm_invite",
            "decline_dm_invite",
            // spaces / DMs
            "add_space",
            "send_dm",
            "read_dm_thread",
            // DM nav rehydration (ZEB-666)
            "list_owner_dm_spaces",
            // relay rung (ZEB-487)
            "set_community_relay_opt_in",
            "get_community_relay_status",
            "get_relay_held",
            // butler rung (ZEB-489)
            "set_butler_pin",
            "get_butler_pin",
            "get_butler_held",
            // device management (ZEB-668 S2/S4/S5)
            "revoke_device",
            "set_device_petname",
            "bump_fleet_epoch",
            // quorum revocation ceremony (ZEB-677 S3)
            "request_quorum_revocation",
            "cosign_quorum_request",
            "decline_quorum_request",
            // quorum enrollment arm window (ZEB-677 S4)
            "arm_quorum_enrollment",
            "disarm_quorum_enrollment",
            // quorum fleet-epoch rotation (ZEB-677 S5)
            "request_quorum_epoch_bump",
            // connectivity
            "connectivity_get_my_reachability_record",
            "connectivity_get_my_identity_pub_hex",
            "connectivity_list_peer_reachability",
            "connectivity_set_identity_discoverable",
            "connectivity_get_identity_discoverable",
            "connectivity_redeem_invite_iroh",
            "connectivity_open_join_iroh",
            // network health
            "network_health_snapshot",
            "network_health_run_self_test",
            // pairing (ZEB-446)
            "start_inviter_pairing",
            "start_joiner_pairing",
            "select_pairing_peer",
            "confirm_pairing_sas",
            "cancel_pairing",
            "get_pairing_state",
            // profile cards (ZEB-341) + peer-profile broadcast (ZEB-281) — ZEB-464
            "republish_owner_card",
            "subscribe_member_card",
            "get_cached_member_card",
            "unsubscribe_member_card",
            "subscribe_peer_profile",
            "get_cached_peer_profile",
            "unsubscribe_peer_profile",
        ];
        expected.sort_unstable();
        assert_eq!(names, expected, "curated v1 surface drifted");
    }

    #[tokio::test]
    async fn set_message_reaction_rejects_bad_hex() {
        // Dispatch "set_message_reaction" with a messageId that is not valid
        // 32-char hex — must surface RpcError::Command about hex/length.
        let reg = build_registry();
        let err = reg
            .dispatch(
                "set_message_reaction",
                test_state(),
                test_sink(),
                serde_json::json!({
                    "communityId": "00112233445566778899aabbccddeeff",
                    "channelId":   "00112233445566778899aabbccddeeff",
                    "messageId":   "zz",
                    "emoji":       "👍",
                    "add":         true
                }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => assert!(
                msg.contains("message_id") || msg.contains("hex") || msg.contains("32"),
                "expected hex/length error for message_id, got: {msg}"
            ),
            other => panic!("set_message_reaction bad hex: expected Command, got {other:?}"),
        }
    }
}
