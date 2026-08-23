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
//
// ZEB-797: every struct carries `deny_unknown_fields`. This deliberately
// breaks parity with the Tauri IPC surface, which tolerates extra fields
// — the two surfaces differ on the axis that matters here:
//
//   Tauri IPC is called by the bundled frontend, compiled and shipped
//   from this same tree. Caller and callee cannot be at different
//   revisions, so a dropped field hides nothing.
//
//   This surface exists to be driven by out-of-band, independently
//   versioned clients (fleet agent nodes, the `api` CLI, e2e harnesses).
//   Skew is the normal condition, not an edge case.
//
// Permissiveness was therefore free on one surface and load-bearing on
// the other. Measured cost of the old behaviour: `order` did not exist
// before ZEB-602, so `{limit: 50, order: "desc"}` against an older node
// silently returned the OLDEST 50 — the caller applied the documented
// ZEB-789 workaround, got a full array of real messages, and came away
// with more confidence and identical blindness. An unknown key is now a
// named error instead.
//
// Cost accepted in exchange: a new optional argument fails loudly against
// older servers rather than silently, so cross-version clients must
// feature-detect rather than optimistically pass the new field.

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmptyArgs {}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetBuddyPledgeArgs {
    owner_address: String,
    bytes: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoveStorageBuddyArgs {
    owner_address: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetSharedBudgetArgs {
    bytes: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetBackupFlagArgs {
    sidecar_id: String,
    backup: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartNodeArgs {
    endpoint: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommunityIdArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VineIdArgs {
    vine_id: String,
}

/// ZEB-435: `remove_space` takes a generic space id (community / dm /
/// group-dm), not specifically a community id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpaceIdArgs {
    space_id: String,
}

/// ZEB-562: headless vine-follow verbs. `name` is the optional display label
/// recorded alongside the followed address (mirrors the GUI command).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FollowVineCreatorArgs {
    address: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnfollowVineCreatorArgs {
    address: String,
}

/// ZEB-811 Task 9: `fetch_vine_video` — mesh-first content fetch with a
/// vine-relay fallback for a followed creator's video.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FetchVineVideoArgs {
    cid: String,
    creator_address: String,
}

/// ZEB-811: `set_vine_settings` — both gates are required on every call
/// (mirrors the Tauri command signature; there is no partial-update form).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetVineSettingsArgs {
    share_follows: bool,
    share_vines_publicly: bool,
}

/// ZEB-527: `community_id` + capped `limit` — shared by the two recent-feed
/// moderation read verbs (`list_recent_counter_signs`,
/// `list_recent_moderation_events`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommunityLimitArgs {
    community_id: String,
    limit: u32,
}

/// ZEB-527: `community_id` + `target_addr` + optional `reason` — shared by
/// the two member-targeting moderation action verbs (`kick_from_community`,
/// `unban_from_community`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModerationTargetArgs {
    community_id: String,
    target_addr: String,
    reason: Option<String>,
}

/// ZEB-527: `community_id` + `proposal_event_id` for
/// `countersign_admin_proposal`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CountersignArgs {
    community_id: String,
    proposal_event_id: String,
}

/// ZEB-714/715: `get_recovery_state`. `now_ms` is the D3 e2e's
/// read-side as-of override (recovery phases advance with wall clock;
/// the 7-day RD4 floor makes execution unobservable on the real clock).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetRecoveryStateArgs {
    community_id: String,
    #[serde(default)]
    now_ms: Option<u64>,
}

/// ZEB-714: `set_recovery_designates` (spec §3.1 config ceremony).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetRecoveryDesignatesArgs {
    community_id: String,
    designate_addrs: Vec<String>,
    threshold: u8,
    veto_window_ms: u64,
}

/// ZEB-714: `initiate_admin_recovery` (spec §3.2).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitiateRecoveryArgs {
    community_id: String,
    lost_admin_addr: String,
    new_admin_addr: String,
}

/// ZEB-714: `cosign_admin_recovery` / `veto_admin_recovery` — both take
/// the target proposal's event id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryProposalTargetArgs {
    community_id: String,
    proposal_event_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateCommunityArgs {
    name: String,
    is_invite_only: bool,
}

// ── Tier-2 conviction voting (ZEB-720) ───────────────────────────────
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VotingCreateTier2Args {
    community_id: String,
    channel_id: String,
    proposal_text: String,
    #[serde(default)]
    half_life_seconds: Option<u32>,
    #[serde(default)]
    threshold_min: Option<i64>,
    #[serde(default)]
    threshold_max: Option<i64>,
    #[serde(default)]
    beta: Option<u8>,
    #[serde(default)]
    delegation_allowed: Option<bool>,
    #[serde(default)]
    min_power: Option<u32>,
    /// Hex-encoded 16-byte OwnerAddr of the SetPower target. When present
    /// (with `set_power_new_power`), the handler builds an
    /// `AutoExecAction::SetPower`; otherwise auto_exec is None. Hex avoids
    /// pushing the OwnerAddr bstr encoding across the JSON boundary.
    #[serde(default)]
    set_power_target: Option<String>,
    #[serde(default)]
    set_power_new_power: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VotingSignalTier2Args {
    proposal_id: String,
    support: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VotingGetTier2Args {
    proposal_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UrlArgs {
    url: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateChannelArgs {
    community_id: String,
    name: String,
    write_power: u8,
    kind: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListChannelMessagesArgs {
    community_id: String,
    channel_id: String,
    since: Option<crate::community_channel_log_engine::HlcDto>,
    limit: u32,
    /// ZEB-602 / ZEB-789: `"desc"` (default) for newest-first, or
    /// `"asc"` for oldest-first. Also selects which end `limit` cuts
    /// from — `"asc"` with a `limit` is the *earliest* N, not the
    /// latest.
    order: Option<String>,
}

/// ZEB-780: `communityId` optional — omit to scan every joined community.
/// `since` is the caller's own cursor, which is what makes this resumable
/// (see `list_mentions_impl` for why it is not server-held read state).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListMentionsArgs {
    #[serde(default)]
    community_id: Option<String>,
    #[serde(default)]
    since: Option<crate::community_channel_log_engine::HlcDto>,
    limit: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PostChannelMessageArgs {
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
    mentions: Option<Vec<String>>,
    attachments: Option<Vec<crate::community_channel_log_engine::ChannelAttachmentDto>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscribeCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnsubscribeCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetCommunityPresenceArgs {
    community_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DownloadChannelArtifactArgs {
    community_id: String,
    channel_id: String,
    cid: String,
    dest_path: String,
    max_bytes: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IngestChannelArtifactArgs {
    community_id: String,
    source_path: String,
    name: Option<String>,
    mime: Option<String>,
    encrypt: Option<bool>,
}

/// ZEB-781: `list_grants` / `dismiss_received_grant` — addressed by content id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CidArgs {
    cid: String,
}

/// ZEB-781: `grant_read` / `revoke_read`. `granteeAddress` is an OwnerAddr hex.
///
/// `grant_read` DOES gate on the friend graph: grants deliver over the friend
/// transport, so the grantee must be an **Active** friend or the share is
/// rejected with `INELIGIBLE_NON_FRIEND` (`file_sharing.rs`, "Gate 2"). Sharing
/// with a community member you have not friended will fail — being co-resident
/// in a community is not sufficient. `revoke_read` has no such gate; it stamps a
/// local tombstone and converges, so a grantee can always be revoked.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantReadArgs {
    cid: String,
    grantee_address: String,
}

/// ZEB-781: `burn_content` addresses the *sidecar*, not the CID — a CID is only
/// fully burned once its last sidecar reference is removed.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BurnContentArgs {
    sidecar_id: String,
}

/// ZEB-781: headless encrypted ingest takes an explicit path. The GUI command
/// owns the native file picker and delegates here with the chosen path, so the
/// dialog stays out of the shared seam (same split as `ingest_channel_artifact`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IngestContentEncryptedArgs {
    source_path: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GenerateFriendTokenArgs {
    /// ZEB-507: TTL *duration* in milliseconds (not an absolute epoch). The
    /// server computes `expiresAt = now_ms + ttlMs`. Omitted/`null` → no expiry.
    ttl_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddFriendByKeyArgs {
    identity_pub_hex: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnerIdHexArgs {
    owner_id_hex: String,
}

/// ZEB-977: set/clear the local petname for any identity.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetContactPetnameArgs {
    owner_id_hex: String,
    #[serde(default)]
    petname: Option<String>,
}

/// ZEB-977: set/clear the local private notes for any identity.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetContactNotesArgs {
    owner_id_hex: String,
    #[serde(default)]
    notes: Option<String>,
}

/// ZEB-236: accept/decline a staged DM invite, keyed by hex `SpaceId`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpaceIdHexArgs {
    space_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddSpaceArgs {
    kind: String,
    name: String,
    members: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendDmArgs {
    space_id: String,
    content: Vec<u8>,
    mime_type: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadDmThreadArgs {
    space_id: String,
    limit: usize,
    /// ZEB-244: opaque full-HLC pagination cursor (an `encode_dm_cursor`
    /// token), was a bare `wall_ms`. Echo back the oldest entry's `cursor`.
    before_hlc: Option<String>,
}

// ZEB-883: ZEB-214 read-receipt controls on the headless surface. `get` reuses
// `SpaceIdHexArgs`. `space_id` is 32-hex; `up_to_ms` is the newest-rendered
// message's ms timestamp (the watermark the Seen advances to).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetSpaceReadReceiptPrefArgs {
    space_id: String,
    enabled: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MarkDmReadArgs {
    space_id: String,
    up_to_ms: u64,
}

// ZEB-775: the three relay-rung RPCs below used to key on `communityIdHex`
// while every sibling verb — `list_community_members`, `list_channels`,
// `list_channel_messages`, `generate_invite` — keys on `communityId`, for
// the same concept holding the same value. The rejection named the key it
// wanted without hinting that the caller had almost certainly used the
// spelling from the adjacent line, so it read as "do you have the id?"
// (yes) rather than "did you spell it our way?" (no).
//
// They now take `communityId`, with `communityIdHex` kept as a serde
// `alias`. An alias rather than a rename because ZEB-797 turns unknown
// fields into hard errors in this same change: a straight rename would
// promote every existing `communityIdHex` caller from silently-wrong to
// broken in one commit. Strictness is meant to catch typos and version
// skew, not to punish the callers who followed the old docs.

/// ZEB-487 / ZEB-775: relay opt-in control.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetCommunityRelayOptInArgs {
    #[serde(alias = "communityIdHex")]
    community_id: String,
    opted_in: bool,
}

/// ZEB-487 / ZEB-775: arg shape for the relay-status read.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommunityIdHexArgs {
    #[serde(alias = "communityIdHex")]
    community_id: String,
}

/// ZEB-487 / ZEB-775: optional community filter for the relay-held
/// observability read.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetRelayHeldArgs {
    #[serde(default, alias = "communityIdHex")]
    community_id: Option<String>,
}

/// ZEB-489: butler-pin control arg shape. `deviceId` omitted/null → clear.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetButlerPinArgs {
    #[serde(default)]
    device_id: Option<String>,
}

/// ZEB-668 S2: device revocation. `reason` ∈ decommissioned|lost|compromised.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevokeDeviceArgs {
    device_vk_hex: String,
    reason: String,
}

/// ZEB-668 S4: fleet-synced device petname. Empty `petname` clears.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetDevicePetnameArgs {
    device_vk_hex: String,
    petname: String,
}

/// ZEB-677 S3: quorum ceremony verbs addressed by request id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuorumRequestIdArgs {
    request_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DisplayNameArgs {
    display_name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerSessionIdArgs {
    peer_session_id: String,
}

/// ZEB-464: shared by the card/profile `get_cached_*` + `unsubscribe_*`
/// verbs — a single `subscriptionId` returned from a prior subscribe.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscriptionIdArgs {
    subscription_id: u64,
}

/// ZEB-464: `subscribe_peer_profile` keys on the peer's owner address hex
/// (`peerAddr`), distinct from the card verbs' `ownerIdHex`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerAddrArgs {
    peer_addr: String,
}

/// ZEB-464: `republish_owner_card` args (avatar/profile-page CIDs optional).
/// ZEB-898: `status_text` optional too (default "") — headless agents set
/// only the display name; an empty status is the natural "no status" card.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepublishOwnerCardArgs {
    display_name: String,
    #[serde(default)]
    status_text: String,
    avatar_cid: Option<String>,
    profile_page_root: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetIdentityDiscoverableArgs {
    enabled: bool,
}

// ── Registry ─────────────────────────────────────────────────────────

/// Build the curated v1 RPC surface. Every handler calls
/// the same `*_impl` seam its Tauri wrapper calls, so the GUI and the
/// headless API observe identical behavior and error strings.
pub fn build_registry() -> RpcRegistry {
    let mut m: HashMap<&'static str, RpcHandler> = HashMap::new();

    // Node lifecycle. ZEB-719: `start_node` is hand-written (not the generic `rpc!`
    // macro) so it can pass the owned `Arc<Mutex<NodeState>>` for headless Tier-2
    // auto-exec — the macro only exposes the borrowed `node_state()`. The owned Arc
    // (same allocation the serve node runs on) lets the `'static` voting-tick closure
    // dispatch a finalized SetPower instead of the `SkippedNotAdmin` stub.
    m.insert(
        "start_node",
        Box::new(
            move |access: Arc<dyn super::NodeStateAccess>,
                  sink: Arc<dyn NodeEventSink>,
                  raw: serde_json::Value| {
                Box::pin(async move {
                    let owned = access.clone().node_state_arc();
                    let state = access.node_state();
                    let raw = if raw.is_null() {
                        serde_json::json!({})
                    } else {
                        raw
                    };
                    let a: StartNodeArgs = serde_json::from_value(raw)
                        .map_err(|e| RpcError::BadArgs(e.to_string()))?;
                    let out = crate::start_node_inner(a.endpoint, sink, None, state, owned)
                        .await
                        .map_err(RpcError::Command)?;
                    serde_json::to_value(out)
                        .map_err(|e| RpcError::Command(format!("serialize: {e}")))
                }) as RpcFuture
            },
        ) as RpcHandler,
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
    // runtime headless. ZEB-719: hand-written (not `rpc!`) so the mint's node
    // RESTART receives the owned `Arc<Mutex<NodeState>>` — every agent-testing node
    // mints on first run, so without this the post-mint tick would re-stub auto-exec.
    m.insert(
        "mint_owner_identity",
        Box::new(
            move |access: Arc<dyn super::NodeStateAccess>,
                  sink: Arc<dyn NodeEventSink>,
                  raw: serde_json::Value| {
                Box::pin(async move {
                    let owned = access.clone().node_state_arc();
                    let state = access.node_state();
                    // Preserve the `rpc!` macro's arg-shape contract (Qodo): normalize
                    // Null→{} and reject malformed payloads with BadArgs, so the bespoke
                    // handler keeps the file's IPC/RPC parity promise.
                    let raw = if raw.is_null() {
                        serde_json::json!({})
                    } else {
                        raw
                    };
                    let _args: EmptyArgs = serde_json::from_value(raw)
                        .map_err(|e| RpcError::BadArgs(e.to_string()))?;
                    let out =
                        crate::owner_commands::mint_owner_identity_impl(state, sink, None, owned)
                            .await
                            .map_err(RpcError::Command)?;
                    serde_json::to_value(out)
                        .map_err(|e| RpcError::Command(format!("serialize: {e}")))
                }) as RpcFuture
            },
        ) as RpcHandler,
    );

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
    // tier-2 conviction voting (ZEB-720)
    rpc!(
        m,
        "voting_create_tier2_proposal",
        VotingCreateTier2Args,
        |state, sink, a| async move {
            let auto_exec = match (a.set_power_target, a.set_power_new_power) {
                (Some(hex_target), Some(np)) => {
                    let bytes: [u8; 16] = hex::decode(&hex_target)
                        .map_err(|e| format!("invalid setPowerTarget hex: {e}"))?
                        .as_slice()
                        .try_into()
                        .map_err(|_| {
                            "setPowerTarget must be 16 bytes (32 hex chars)".to_string()
                        })?;
                    Some(
                        crate::community_voting_conviction::AutoExecAction::SetPower {
                            target_pubkey: crate::owner_state_types::OwnerAddr(bytes),
                            new_power: np,
                        },
                    )
                }
                (None, None) => None,
                // Reject a PARTIAL SetPower: supplying exactly one of the pair
                // would otherwise mint a proposal that silently never changes
                // power on finalize — a caller footgun.
                _ => {
                    return Err(
                        "voting_create_tier2_proposal: setPowerTarget and setPowerNewPower \
                         must both be provided or both omitted"
                            .to_string(),
                    )
                }
            };
            crate::voting_create_tier2_proposal_impl(
                state,
                sink,
                a.community_id,
                a.channel_id,
                a.proposal_text,
                a.half_life_seconds,
                a.threshold_min,
                a.threshold_max,
                a.beta,
                a.delegation_allowed,
                auto_exec,
                a.min_power,
            )
            .await
        }
    );
    rpc!(
        m,
        "voting_signal_tier2",
        VotingSignalTier2Args,
        |state, sink, a| async move {
            crate::voting_signal_tier2_impl(state, sink, a.proposal_id, a.support).await
        }
    );
    rpc!(
        m,
        "voting_get_tier2_proposal",
        VotingGetTier2Args,
        |state, _sink, a| async move {
            crate::voting_get_tier2_proposal_impl(state, a.proposal_id).await
        }
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
        // ZEB-885: flatten the structured RedeemInviteError to its message —
        // the serve/api RPC wire (HTTP POST /v1/rpc/{command}) stays a string
        // error (the fleet CLI doesn't switch on codes). The structured form is
        // GUI-only (Tauri IPC).
        crate::redeem_invite_impl(state, sink, a.url)
            .await
            .map_err(|e| e.to_string())
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
            // ZEB-885: flatten the structured error to its message — the
            // serve/api RPC wire stays a string (structured form is GUI-only).
            crate::connectivity_redeem_invite_iroh_impl(state, sink, a.url)
                .await
                .map_err(|e| e.to_string())
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
    // ZEB-581: clear a LEFT community's on-disk cache WITHOUT tombstoning
    // (keeps it rejoinable). Community-only; refuses not-yet-left communities.
    rpc!(
        m,
        "clear_space_local_cache",
        CommunityIdArgs,
        |state, _sink, a| async move {
            crate::clear_space_local_cache_impl(state, a.community_id).await
        }
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
        GetRecoveryStateArgs,
        |state, _sink, a| async move {
            crate::get_recovery_state_impl(state, a.community_id, a.now_ms).await
        }
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
    // ZEB-780: the receive half of mentions. Without this a headless agent
    // could send a correct mention and never learn it had received one.
    rpc!(
        m,
        "list_mentions",
        ListMentionsArgs,
        |state, _sink, a| async move {
            crate::list_mentions_impl(state, a.community_id, a.since, a.limit).await
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

    // File sharing (ZEB-781). These are registrations, not a refactor: the
    // `_impl` seams already take `&dyn NodeEventSink`, and the GUI commands
    // in lib.rs reach them by passing `&app` (AppHandle implements the same
    // trait). Without them a headless node can join, chat and vote but cannot
    // answer "has a file been shared with me?" — which made ZEB-770's
    // third-party exclusion assertion unavailable rather than merely weak.
    rpc!(m, "list_received_grants", EmptyArgs, |state, _sink, _a| {
        async move { crate::list_received_grants_impl(state).await }
    });
    rpc!(m, "list_grants", CidArgs, |state, _sink, a| async move {
        crate::list_grants_impl(state, a.cid).await
    });
    rpc!(
        m,
        "grant_read",
        GrantReadArgs,
        |state, sink, a| async move {
            crate::grant_read_impl(state, sink.as_ref(), a.cid, a.grantee_address).await
        }
    );
    rpc!(
        m,
        "revoke_read",
        GrantReadArgs,
        |state, sink, a| async move {
            crate::revoke_read_impl(state, sink.as_ref(), a.cid, a.grantee_address).await
        }
    );
    rpc!(
        m,
        "dismiss_received_grant",
        CidArgs,
        |state, sink, a| async move {
            crate::dismiss_received_grant_impl(state, sink.as_ref(), a.cid).await
        }
    );
    rpc!(m, "burn_content", BurnContentArgs, |state, _sink, a| {
        async move { crate::burn_content_impl(a.sidecar_id, state).await }
    });
    rpc!(
        m,
        "ingest_content_encrypted",
        IngestContentEncryptedArgs,
        |state, _sink, a| async move {
            crate::ingest_content_encrypted_impl(state, std::path::Path::new(&a.source_path)).await
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
    // ZEB-811 Task 9: mesh-first video fetch with a vine-relay fallback for
    // followed creators. Returns the raw byte `Vec<u8>` — same JSON-array
    // shape the GUI IPC's `fetch_content`/`fetch_avatar` already return, so
    // this is one mental model across GUI and API (file header comment).
    rpc!(
        m,
        "fetch_vine_video",
        FetchVineVideoArgs,
        |state, _sink, a| async move {
            crate::fetch_vine_video_impl(state, a.cid, a.creator_address).await
        }
    );
    // ZEB-811: vine-settings verbs — headless parity for the Tune-sheet
    // toggles (`share_follows`, `share_vines_publicly`).
    rpc!(
        m,
        "get_vine_settings",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_vine_settings_impl(state) }
    );
    rpc!(
        m,
        "set_vine_settings",
        SetVineSettingsArgs,
        |state, _sink, a| async move {
            crate::set_vine_settings_impl(state, a.share_follows, a.share_vines_publicly)
        }
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
    // ZEB-783: this user's own unanswered outbound requests + a cancel.
    rpc!(
        m,
        "list_outbound_friend_requests",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_outbound_friend_requests_impl(state).await }
    );
    rpc!(
        m,
        "cancel_outbound_friend_request",
        AddFriendByKeyArgs,
        |state, _sink, a| async move {
            crate::cancel_outbound_friend_request_impl(state, a.identity_pub_hex).await
        }
    );

    // Contacts (ZEB-977): owner-private petname + notes for ANY identity.
    rpc!(m, "contacts_list", EmptyArgs, |state, _sink, _a| {
        async move { crate::contacts_commands::contacts_list_impl(state).await }
    });
    rpc!(
        m,
        "set_contact_petname",
        SetContactPetnameArgs,
        |state, sink, a| async move {
            crate::contacts_commands::set_contact_petname_impl(
                state,
                sink.as_ref(),
                a.owner_id_hex,
                a.petname,
            )
            .await
        }
    );
    rpc!(
        m,
        "set_contact_notes",
        SetContactNotesArgs,
        |state, sink, a| async move {
            crate::contacts_commands::set_contact_notes_impl(
                state,
                sink.as_ref(),
                a.owner_id_hex,
                a.notes,
            )
            .await
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
    // ZEB-214 read receipts, headless parity (ZEB-883): opt in/out, read the
    // pref, and advance the read watermark (the last fires the Seen).
    rpc!(
        m,
        "set_space_read_receipt_pref",
        SetSpaceReadReceiptPrefArgs,
        |state, _sink, a| async move {
            crate::set_space_read_receipt_pref_impl(state, a.space_id, a.enabled).await
        }
    );
    rpc!(
        m,
        "get_space_read_receipt_pref",
        SpaceIdHexArgs,
        |state, _sink, a| async move {
            crate::get_space_read_receipt_pref_impl(state, a.space_id).await
        }
    );
    rpc!(
        m,
        "mark_dm_read",
        MarkDmReadArgs,
        |state, _sink, a| async move { crate::mark_dm_read_impl(state, a.space_id, a.up_to_ms).await }
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
            crate::set_community_relay_opt_in_impl(state, a.community_id, a.opted_in).await
        }
    );
    rpc!(
        m,
        "get_community_relay_status",
        CommunityIdHexArgs,
        |state, _sink, a| async move {
            crate::get_community_relay_status_impl(state, a.community_id).await
        }
    );
    rpc!(
        m,
        "get_relay_held",
        GetRelayHeldArgs,
        |state, _sink, a| async move { crate::get_relay_held_impl(state, a.community_id).await }
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
    // ZEB-794: was Tauri-only. `identityActive` here is the direct answer to
    // "why does add_friend_by_key say `unreachable` against my node?" — and
    // it was reachable only from a surface a `serve` node does not have.
    rpc!(
        m,
        "connectivity_pkarr_publication_status",
        EmptyArgs,
        |state, _sink, _a| {
            async move { crate::connectivity_pkarr_publication_status_impl(state).await }
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

    /// ZEB-797: an unknown key must be a named error, not silently dropped.
    ///
    /// The oracle is a real dispatch of a real payload, not the presence of
    /// the attribute — `deny_unknown_fields` is easy to delete and a test
    /// that greps for it would keep passing on a struct that no longer
    /// enforces anything.
    ///
    /// The field name must appear in the message. The whole value of failing
    /// loudly is that an operator reads *which* key was not understood and
    /// concludes "my node predates this argument" in one step; a bare
    /// "invalid arguments" would restore the guessing this replaces.
    #[tokio::test]
    async fn unknown_arg_key_is_rejected_and_named() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_channel_messages",
                test_state(),
                test_sink(),
                serde_json::json!({
                    "communityId": "00".repeat(16),
                    "channelId": "00".repeat(16),
                    "limit": 5,
                    "nonsenseArg": "xyz",
                }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::BadArgs(msg) => assert!(
                msg.contains("nonsenseArg"),
                "the rejection must name the offending key so version skew is \
                 self-diagnosing; got: {msg}"
            ),
            other => panic!("expected BadArgs for an unknown key, got {other:?}"),
        }
    }

    /// ZEB-797, the case that motivated it: `order` did not exist before
    /// ZEB-602, so against an older node `{limit, order}` deserialized to
    /// `{limit}` and returned a window the caller had explicitly asked not
    /// to get. Nothing can make an *old* binary report that — but this pins
    /// the property that makes the failure impossible going forward, by
    /// proving an argument this surface does not know is never ignored.
    #[tokio::test]
    async fn a_future_argument_is_never_silently_dropped() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_channel_messages",
                test_state(),
                test_sink(),
                serde_json::json!({
                    "communityId": "00".repeat(16),
                    "channelId": "00".repeat(16),
                    "limit": 5,
                    // Stand-in for whatever `order` was in July 2026: an
                    // argument a later revision adds and this one has never
                    // heard of.
                    "cursorDirection": "desc",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::BadArgs(_)),
            "an unrecognised argument must fail the call rather than change \
             its meaning; got {err:?}"
        );
    }

    /// ZEB-775: the relay verbs accept `communityId` like every sibling.
    ///
    /// Reaching the impl seam is the assertion — the default `NodeState`
    /// has no owner, so a `Command` error proves deserialization succeeded
    /// and the value arrived. `BadArgs` would mean the key was not accepted.
    #[tokio::test]
    async fn relay_status_accepts_community_id() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "get_community_relay_status",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityId": "00".repeat(16) }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "communityId must deserialize and reach the impl; got {err:?}"
        );
    }

    /// ZEB-775 + ZEB-797: the old spelling still works.
    ///
    /// This is the half that makes the two tickets compatible. `alias` keeps
    /// `communityIdHex` a *known* field, so strictness does not convert the
    /// callers who followed the old docs from silently-wrong into broken.
    /// Delete the alias and this fails while the test above still passes —
    /// which is the point of asserting both spellings separately.
    #[tokio::test]
    async fn relay_status_still_accepts_the_deprecated_community_id_hex() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "get_community_relay_status",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityIdHex": "00".repeat(16) }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "the deprecated communityIdHex alias must still deserialize; \
             got {err:?}"
        );
    }

    /// ZEB-775: the other two relay-rung verbs carry the same alias contract.
    ///
    /// Only `get_community_relay_status` had coverage in the first cut
    /// (CodeRabbit, PR #554), so an accidental removal of the `alias` on
    /// `SetCommunityRelayOptInArgs` or `GetRelayHeldArgs` would have gone
    /// unnoticed — and under ZEB-797's strictness that removal is no longer
    /// a silent degradation, it is a hard failure for those callers.
    ///
    /// `BadArgs` is the failure this discriminates: it means the key was not
    /// accepted. Reaching `Command` means it deserialized and hit the impl.
    #[tokio::test]
    async fn all_relay_verbs_accept_both_community_id_spellings() {
        let reg = build_registry();
        let hex = "00".repeat(16);
        let cases = [
            (
                "set_community_relay_opt_in",
                serde_json::json!({ "communityId": hex, "optedIn": true }),
            ),
            (
                "set_community_relay_opt_in",
                serde_json::json!({ "communityIdHex": hex, "optedIn": true }),
            ),
            ("get_relay_held", serde_json::json!({ "communityId": hex })),
            (
                "get_relay_held",
                serde_json::json!({ "communityIdHex": hex }),
            ),
        ];
        for (cmd, args) in cases {
            let outcome = reg
                .dispatch(cmd, test_state(), test_sink(), args.clone())
                .await;
            match outcome {
                // Either shape is fine: what must NOT happen is BadArgs,
                // which would mean the key was rejected.
                Ok(_) => {}
                Err(RpcError::Command(_)) => {}
                Err(other) => panic!("{cmd} rejected {args}: {other:?}"),
            }
        }
    }

    /// ZEB-797 + ZEB-775 together: strictness must not swallow the alias.
    ///
    /// A misspelling adjacent to both accepted spellings has to fail — the
    /// alias widens the accepted set by exactly one name, not to anything
    /// that looks close enough.
    #[tokio::test]
    async fn a_near_miss_community_id_spelling_is_still_rejected() {
        let reg = build_registry();
        let err = reg
            .dispatch(
                "get_community_relay_status",
                test_state(),
                test_sink(),
                serde_json::json!({ "communityIDHex": "00".repeat(16) }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::BadArgs(_)),
            "`communityIDHex` is neither the field nor the alias and must be \
             rejected; got {err:?}"
        );
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
    async fn outbound_friend_request_rpcs_are_registered_and_wired() {
        // ZEB-783 (PR #552 review): `registry_has_exactly_the_curated_v1_surface`
        // pins NAMES only. It cannot catch the arg struct rejecting the Tauri
        // wrapper's camelCase shape, nor a verb wired to the wrong seam. Same
        // proof shape as the vine-follow parity test.
        //
        // Both of these reach their seam on a default NodeState rather than
        // erroring: the store is process-local and present on a bare NodeState,
        // so `list` returns an empty projection and `cancel` is idempotently
        // Ok on an unknown key. So `Ok` here IS the wired signal — what would
        // fail is `UnknownCommand` (unregistered) or `BadArgs` (arg mismatch).
        let reg = build_registry();
        let key = "cd".repeat(64); // valid 128-hex identity pub
        let cases = [
            ("list_outbound_friend_requests", serde_json::json!({})),
            (
                "cancel_outbound_friend_request",
                serde_json::json!({ "identityPubHex": key }),
            ),
        ];
        for (method, args) in cases {
            let out = reg.dispatch(method, test_state(), test_sink(), args).await;
            assert!(
                out.is_ok(),
                "{method}: expected the seam to answer on a default NodeState, got {:?}",
                out.unwrap_err()
            );
        }

        // And the arg struct must actually REJECT a wrong shape — otherwise the
        // Ok above would also pass for a verb that ignores its arguments.
        let bad = reg
            .dispatch(
                "cancel_outbound_friend_request",
                test_state(),
                test_sink(),
                serde_json::json!({ "identity_pub_hex": "snake_case is wrong" }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(bad, RpcError::BadArgs(_)),
            "snake_case args must be rejected, got {bad:?}"
        );
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
    async fn vine_settings_rpcs_are_registered_and_wired() {
        // ZEB-811: unlike the follow verbs above, `get_vine_settings` and a
        // no-op `set_vine_settings` both answer `Ok` on a default (not yet
        // started) NodeState — neither seam requires a connected node. `Ok`
        // here IS the wired signal: `UnknownCommand` would mean unregistered,
        // `BadArgs` would mean the arg struct rejected the shape.
        let reg = build_registry();

        let out = reg
            .dispatch(
                "get_vine_settings",
                test_state(),
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .expect("get_vine_settings must reach the seam on a default NodeState");
        assert_eq!(
            out,
            serde_json::json!({ "shareFollows": true, "shareVinesPublicly": true }),
            "defaults must be public-by-intent true/true"
        );

        // Asymmetric values + read-back on the SAME state: pins WHICH field
        // each arg binds to. Both params are `bool`, so a positional swap
        // (`share_follows`/`share_vines_publicly` wired to each other's
        // parameter) would otherwise compile and pass silently — this repo
        // already treats that exact hazard as worth pinning, see
        // `file_sharing_rpcs_bind_args_to_the_right_parameters`.
        let state = test_state();
        reg.dispatch(
            "set_vine_settings",
            state.clone(),
            test_sink(),
            serde_json::json!({ "shareFollows": true, "shareVinesPublicly": false }),
        )
        .await
        .expect("set_vine_settings must reach the seam on a default NodeState");
        let back = reg
            .dispatch(
                "get_vine_settings",
                state,
                test_sink(),
                serde_json::json!({}),
            )
            .await
            .expect("read-back");
        assert_eq!(
            back,
            serde_json::json!({ "shareFollows": true, "shareVinesPublicly": false }),
            "each arg must bind to its own parameter"
        );

        // The arg struct must actually REJECT a wrong shape (snake_case, or
        // either field missing) — deny_unknown_fields is enforced elsewhere,
        // this proves the required-field/casing contract for THIS struct.
        let bad = reg
            .dispatch(
                "set_vine_settings",
                test_state(),
                test_sink(),
                serde_json::json!({ "share_follows": true, "share_vines_publicly": false }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(bad, RpcError::BadArgs(_)),
            "snake_case args must be rejected, got {bad:?}"
        );
    }

    #[tokio::test]
    async fn fetch_vine_video_rpc_is_registered_and_wired() {
        // ZEB-811 Task 9 review fix round 1 (Important): dedicated proof,
        // same shape as the vine-follow parity test above — this seam also
        // requires a connected node, so valid camelCase args on a default
        // NodeState must reach the `_impl` seam and surface its
        // `Command("not connected")` error (NOT `UnknownCommand`, which
        // would mean unregistered). snake_case args must be rejected by the
        // arg struct's `deny_unknown_fields` + required-field contract.
        let reg = build_registry();

        let err = reg
            .dispatch(
                "fetch_vine_video",
                test_state(),
                test_sink(),
                serde_json::json!({ "cid": "ab", "creatorAddress": "cd".repeat(16) }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "expected Command (not connected), got {err:?}"
        );

        let bad = reg
            .dispatch(
                "fetch_vine_video",
                test_state(),
                test_sink(),
                serde_json::json!({ "cid": "ab", "creator_address": "cd".repeat(16) }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(bad, RpcError::BadArgs(_)),
            "snake_case args must be rejected, got {bad:?}"
        );
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

    #[tokio::test]
    async fn file_sharing_rpcs_are_registered_and_wired() {
        // ZEB-781: same proof shape as the storage-buddy parity test. These
        // verbs existed as Tauri IPC only, so the risk this pins is an arg
        // struct that does not match the wrapper's camelCase shape —
        // `granteeAddress` / `sidecarId` / `sourcePath` are all easy to get
        // wrong, and BadArgs is the failure a headless caller would hit.
        let reg = build_registry();
        let cid = "cd".repeat(32); // 32 bytes, the width parse_cid_hex requires
        let grantee = "ab".repeat(16); // 16-byte OwnerAddr
        let cases = [
            ("list_received_grants", serde_json::json!({})),
            ("list_grants", serde_json::json!({ "cid": cid })),
            (
                "grant_read",
                serde_json::json!({ "cid": cid, "granteeAddress": grantee }),
            ),
            (
                "revoke_read",
                serde_json::json!({ "cid": cid, "granteeAddress": grantee }),
            ),
            ("dismiss_received_grant", serde_json::json!({ "cid": cid })),
            (
                "burn_content",
                serde_json::json!({ "sidecarId": "00000000-0000-4000-8000-000000000000" }),
            ),
            (
                "ingest_content_encrypted",
                serde_json::json!({ "sourcePath": "/nonexistent/zeb781" }),
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

    /// ZEB-781 (CodeRabbit, PR #551): the registration test above accepts any
    /// `Command` error, so it would still pass if `cid` and `granteeAddress`
    /// were wired to each other's parameters. These verbs parse their args
    /// BEFORE touching NodeState, and each parser has a distinct message, so a
    /// default state is enough to prove the binding: feed exactly one malformed
    /// arg and require the error naming THAT arg's parser.
    ///
    /// Scope note: `ingest_content_encrypted`'s `sourcePath` binding is not
    /// provable here — it snapshots NodeState (failing "not connected") before
    /// it ever opens the path, so a default state cannot reach the file layer.
    /// That one is covered by the live ZEB-770 exercise instead.
    #[tokio::test]
    async fn file_sharing_rpcs_bind_args_to_the_right_parameters() {
        let reg = build_registry();
        let cid = "cd".repeat(32);
        let grantee = "ab".repeat(16);
        // (method, args, substring the CORRECT parser must produce)
        let cases = [
            (
                "grant_read",
                serde_json::json!({ "cid": "zz", "granteeAddress": grantee }),
                "cid",
            ),
            (
                "grant_read",
                serde_json::json!({ "cid": cid, "granteeAddress": "zz" }),
                "owner address",
            ),
            (
                "revoke_read",
                serde_json::json!({ "cid": "zz", "granteeAddress": grantee }),
                "cid",
            ),
            (
                "revoke_read",
                serde_json::json!({ "cid": cid, "granteeAddress": "zz" }),
                "owner address",
            ),
            (
                "burn_content",
                serde_json::json!({ "sidecarId": "" }),
                "sidecar_id",
            ),
        ];
        for (method, args, expect) in cases {
            match reg.dispatch(method, test_state(), test_sink(), args).await {
                Err(RpcError::Command(msg)) => assert!(
                    msg.contains(expect),
                    "{method}: expected the {expect:?} parser to reject, got {msg:?} — \
                     args may be bound to the wrong parameters"
                ),
                other => panic!("{method}: expected a Command parse error, got {other:?}"),
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

    /// ZEB-977: dispatch proof for the three contacts verbs — registered,
    /// camelCase arg shape accepted, and each write impl deterministically
    /// fails at the "contacts dataset not loaded" seam on a default
    /// NodeState (which is exactly the proof that registration + arg
    /// parsing succeeded).
    #[tokio::test]
    async fn contacts_rpcs_are_registered_and_wired() {
        let reg = build_registry();
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("contacts_list", serde_json::Value::Null),
            (
                "set_contact_petname",
                serde_json::json!({
                    "ownerIdHex": "ab".repeat(16),
                    "petname": "Koya",
                }),
            ),
            (
                "set_contact_notes",
                serde_json::json!({
                    "ownerIdHex": "ab".repeat(16),
                    "notes": "met at the garden",
                }),
            ),
        ];
        for (cmd, args) in cases {
            let state = Arc::new(Mutex::new(NodeState::default()));
            match reg.dispatch(cmd, state, test_sink(), args).await {
                Err(RpcError::UnknownCommand) => panic!("{cmd} must be registered"),
                Err(RpcError::BadArgs(msg)) => {
                    panic!("{cmd}: arg struct rejected the wire shape: {msg}")
                }
                Err(RpcError::Command(msg)) => {
                    assert!(
                        msg.contains("contacts dataset not loaded"),
                        "{cmd}: expected the dataset-not-loaded seam error, got: {msg}"
                    );
                }
                Ok(v) => {
                    panic!("{cmd}: expected dataset-not-loaded on default NodeState, got {v:?}")
                }
            }
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
                // `nowMs` is optional — send it here to pin that the
                // as-of override parses on the wire (ZEB-715).
                serde_json::json!({ "communityId": community_id, "nowMs": 1_700_000_000_000u64 }),
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
        // OWNER_STILL_STARTING_MSG (proves the seam is shared, args parsed).
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
                    crate::OWNER_STILL_STARTING_MSG,
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

        // ZEB-898: statusText is optional on the RPC surface — omitting it
        // must parse (default "") and reach the same pre-node Command error,
        // not BadArgs. Headless agents setting only a display name were
        // getting HTTP 400 `missing field statusText`.
        let err = reg
            .dispatch(
                "republish_owner_card",
                test_state(),
                test_sink(),
                serde_json::json!({ "displayName": "OnlyName" }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert_eq!(msg, "owner card runtime not ready")
            }
            other => {
                panic!("republish_owner_card sans statusText: expected Command, got {other:?}")
            }
        }

        // The omitted field must default to "" specifically (not garbage).
        let args: RepublishOwnerCardArgs =
            serde_json::from_value(serde_json::json!({ "displayName": "OnlyName" }))
                .expect("statusText omitted must deserialize");
        assert_eq!(args.status_text, "");
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
                    crate::OWNER_STILL_STARTING_MSG,
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
                crate::OWNER_STILL_STARTING_MSG,
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
    #[serial_test::serial] // mutates process-global HARMONY_DATA_DIR (Qodo/CodeAnt #645)
    async fn connectivity_get_identity_discoverable_resolves_default_when_node_stopped() {
        // ZEB-881 / Qodo: with no settings path on NodeState (node stopped /
        // headless pre-init), the getter resolves the persisted settings via the
        // ZEB-380 app-data-dir fallback and reports the stored value — or the
        // first-run Default (ON) when no file exists — instead of a hard `false`
        // that would disagree with `load_or_default` and mis-seed the toggle.
        // Isolate the resolved path to a fresh temp dir (nextest runs each test in
        // its own process, so the env override does not leak across tests).
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::ScopedEnvVar::set("HARMONY_DATA_DIR", tmp.path());
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
        assert_eq!(
            out,
            serde_json::Value::Bool(true),
            "a fresh identity with no persisted file is discoverable by default (ZEB-881)"
        );
    }

    #[tokio::test]
    #[serial_test::serial] // mutates process-global HARMONY_DATA_DIR (Qodo/CodeAnt #645)
    async fn connectivity_set_identity_discoverable_persists_pre_owner() {
        // ZEB-890 (B2): pre-owner / node-stopped there is no live
        // `pkarr_identity_publisher`, but the setter must still PERSIST the
        // opt-out (the boot enable reads it at next start) rather than error —
        // mirroring the getter's ZEB-380 app-data-dir fallback and
        // `set_presence_visibility`. Before ZEB-890 this returned a Command error,
        // so the getter's ON default (resolved via the same fallback) was
        // un-turn-off-able on a stopped node — the reported privacy trap.
        //
        // Isolate the resolved path to a fresh temp dir (nextest runs each test in
        // its own process, so the env override does not leak across tests).
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::ScopedEnvVar::set("HARMONY_DATA_DIR", tmp.path());
        let reg = build_registry();
        let set = reg
            .dispatch(
                "connectivity_set_identity_discoverable",
                test_state(),
                test_sink(),
                serde_json::json!({ "enabled": false }),
            )
            .await;
        // Read back through the getter (same app-data-dir fallback) to prove the
        // opt-out durably landed.
        let got = reg
            .dispatch(
                "connectivity_get_identity_discoverable",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await;
        set.expect("setter persists the opt-out pre-owner without erroring");
        assert_eq!(
            got.expect("getter"),
            serde_json::Value::Bool(false),
            "a pre-owner opt-out must persist and read back as OFF (ZEB-890 B2)"
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
    async fn read_receipt_rpcs_are_registered_and_wired() {
        // ZEB-883: the three ZEB-214 read-receipt verbs must be reachable on the
        // headless surface so an agent node can opt in and *emit* a Seen (not
        // just observe one). Proves registration + arg parse + wiring to the
        // right seam, same shape as the vine parity tests above.
        //
        // On a default (owner-not-loaded) NodeState:
        //  - set/get need `crdt_state`, absent here → the seam surfaces its
        //    owner-not-loaded `Command` error (NOT `UnknownCommand`, which would
        //    mean unregistered; NOT `BadArgs`, which would mean the arg struct
        //    rejected the camelCase shape).
        //  - mark_dm_read is a best-effort no-op when the node isn't fully up
        //    (tunnel/signing handles absent) → `Ok`. `Ok` is still the wired
        //    signal: an unregistered verb would be `UnknownCommand`.
        let reg = build_registry();
        let space = "ab".repeat(16); // valid 16-byte (32-hex) space id

        let err = reg
            .dispatch(
                "set_space_read_receipt_pref",
                test_state(),
                test_sink(),
                serde_json::json!({ "spaceId": space, "enabled": true }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "set_space_read_receipt_pref: expected Command (owner not loaded), got {err:?}"
        );

        let err = reg
            .dispatch(
                "get_space_read_receipt_pref",
                test_state(),
                test_sink(),
                serde_json::json!({ "spaceId": space }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Command(_)),
            "get_space_read_receipt_pref: expected Command (owner not loaded), got {err:?}"
        );

        reg.dispatch(
            "mark_dm_read",
            test_state(),
            test_sink(),
            serde_json::json!({ "spaceId": space, "upToMs": 1234u64 }),
        )
        .await
        .expect("mark_dm_read is a best-effort no-op on a not-yet-up node → Ok");

        // Each arg struct must REJECT the snake_case wire shape — otherwise the
        // above would also pass for a verb that silently ignored its arguments.
        for (method, bad_args) in [
            (
                "set_space_read_receipt_pref",
                serde_json::json!({ "space_id": space, "enabled": true }),
            ),
            (
                "get_space_read_receipt_pref",
                serde_json::json!({ "space_id": space }),
            ),
            (
                "mark_dm_read",
                serde_json::json!({ "space_id": space, "up_to_ms": 1234u64 }),
            ),
        ] {
            let bad = reg
                .dispatch(method, test_state(), test_sink(), bad_args)
                .await
                .unwrap_err();
            assert!(
                matches!(bad, RpcError::BadArgs(_)),
                "{method}: snake_case args must be rejected, got {bad:?}"
            );
        }

        // ZEB-883 hardening (Qodo review): an oversized spaceId is rejected by a
        // cheap length precheck BEFORE any hex allocation, on every verb — the
        // repo convention for this externally-invokable surface. The arg struct
        // accepts any String, so this reaches the `_impl` and must surface the
        // length `Command` error, not attempt an unbounded decode.
        let oversized = "ab".repeat(4096); // valid hex chars, wrong length
        for method in [
            "set_space_read_receipt_pref",
            "get_space_read_receipt_pref",
            "mark_dm_read",
        ] {
            let mut args = serde_json::Map::new();
            args.insert("spaceId".into(), serde_json::json!(oversized));
            if method == "set_space_read_receipt_pref" {
                args.insert("enabled".into(), serde_json::json!(true));
            }
            if method == "mark_dm_read" {
                args.insert("upToMs".into(), serde_json::json!(1u64));
            }
            let err = reg
                .dispatch(
                    method,
                    test_state(),
                    test_sink(),
                    serde_json::Value::Object(args),
                )
                .await
                .unwrap_err();
            match err {
                RpcError::Command(msg) => assert!(
                    msg.contains("16 bytes") || msg.contains("32 hex"),
                    "{method}: expected length rejection, got: {msg}"
                ),
                other => panic!("{method}: expected Command length rejection, got {other:?}"),
            }
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
            "clear_space_local_cache",
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
            // tier-2 conviction voting (ZEB-720)
            "voting_create_tier2_proposal",
            "voting_signal_tier2",
            "voting_get_tier2_proposal",
            // channels
            "create_channel",
            "list_channels",
            "list_channel_messages",
            "list_mentions",
            "post_channel_message",
            "set_message_reaction",
            // channel artifacts (CAS)
            "ingest_channel_artifact",
            "download_channel_artifact",
            // per-member file sharing (ZEB-781)
            "ingest_content_encrypted",
            "list_grants",
            "list_received_grants",
            "grant_read",
            "revoke_read",
            "dismiss_received_grant",
            "burn_content",
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
            // vine settings (ZEB-811)
            "get_vine_settings",
            "set_vine_settings",
            // vine video fetch (ZEB-811 Task 9)
            "fetch_vine_video",
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
            // ZEB-783: outbound mirror of the inbound inbox.
            "list_outbound_friend_requests",
            "cancel_outbound_friend_request",
            // contacts (ZEB-977)
            "contacts_list",
            "set_contact_petname",
            "set_contact_notes",
            // DM invites (ZEB-236)
            "list_pending_dm_invites",
            "accept_dm_invite",
            "decline_dm_invite",
            // spaces / DMs
            "add_space",
            "send_dm",
            "read_dm_thread",
            // ZEB-214 read receipts, headless parity (ZEB-883)
            "set_space_read_receipt_pref",
            "get_space_read_receipt_pref",
            "mark_dm_read",
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
            "connectivity_pkarr_publication_status",
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
