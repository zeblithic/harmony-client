// src-tauri/src/api/rpc.rs — ZEB-445 uniform RPC: POST /v1/rpc/{command}.
//
// Same command names, same camelCase JSON args, same DTOs, same error
// strings as the Tauri IPC — one mental model across GUI and API. The
// curated v1 surface is intentionally small; adding a command later is one
// rpc!() line plus its _impl seam.

use crate::node_event_sink::NodeEventSink;
use crate::NodeState;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum RpcError {
    UnknownCommand,
    BadArgs(String),
    Command(String),
}

pub type RpcFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, RpcError>> + Send>>;
pub type RpcHandler = Box<
    dyn Fn(Arc<Mutex<NodeState>>, Arc<dyn NodeEventSink>, serde_json::Value) -> RpcFuture
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
        state: Arc<Mutex<NodeState>>,
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
                move |$state: Arc<Mutex<NodeState>>,
                      $sink: Arc<dyn NodeEventSink>,
                      raw: serde_json::Value| {
                    Box::pin(async move {
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
struct CreateCommunityArgs {
    name: String,
    is_invite_only: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateInviteArgs {
    community_id: String,
    invitee_hint: Option<String>,
    expires_at: Option<u64>,
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
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostChannelMessageArgs {
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateFriendTokenArgs {
    expires_at: Option<u64>,
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

// ── Registry ─────────────────────────────────────────────────────────

/// Build the curated v1 RPC surface (29 commands). Every handler calls
/// the same `*_impl` seam its Tauri wrapper calls, so the GUI and the
/// headless API observe identical behavior and error strings.
pub fn build_registry() -> RpcRegistry {
    let mut m: HashMap<&'static str, RpcHandler> = HashMap::new();

    // Node lifecycle.
    rpc!(
        m,
        "start_node",
        StartNodeArgs,
        |state, sink, a| async move { crate::start_node_inner(a.endpoint, sink, None, &state).await }
    );
    rpc!(m, "stop_node", EmptyArgs, |state, sink, _a| async move {
        crate::stop_node_impl(&state, sink)
    });

    // Owner state.
    rpc!(m, "get_owner_state", EmptyArgs, |state, _sink, _a| {
        async move { crate::owner_commands::get_owner_state_impl(&state).await }
    });
    // ZEB-445 DoD: explicit headless identity bootstrap — first boot is
    // pre-mint; the GUI mints via WelcomeModal. `None` wry_handle: no Tauri
    // runtime headless.
    rpc!(m, "mint_owner_identity", EmptyArgs, |state, sink, _a| {
        async move { crate::owner_commands::mint_owner_identity_impl(&state, sink, None).await }
    });

    // Communities.
    rpc!(
        m,
        "list_owner_communities",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_owner_communities_impl(&state).await }
    );
    rpc!(
        m,
        "create_community",
        CreateCommunityArgs,
        |state, sink, a| async move {
            crate::create_community_impl(&state, sink, a.name, a.is_invite_only).await
        }
    );
    rpc!(
        m,
        "list_community_members",
        CommunityIdArgs,
        |state, _sink, a| async move {
            crate::list_community_members_impl(&state, a.community_id).await
        }
    );
    rpc!(
        m,
        "generate_invite",
        GenerateInviteArgs,
        |state, _sink, a| async move {
            crate::generate_invite_impl(&state, a.community_id, a.invitee_hint, a.expires_at).await
        }
    );
    rpc!(m, "redeem_invite", UrlArgs, |state, sink, a| async move {
        crate::redeem_invite_impl(&state, sink, a.url).await
    });
    rpc!(
        m,
        "join_open_community",
        CommunityIdArgs,
        |state, sink, a| async move {
            crate::join_open_community_impl(&state, sink, a.community_id).await
        }
    );
    rpc!(
        m,
        "leave_community",
        CommunityIdArgs,
        |state, sink, a| async move { crate::leave_community_impl(&state, sink, a.community_id).await }
    );

    // Channels.
    rpc!(
        m,
        "create_channel",
        CreateChannelArgs,
        |state, _sink, a| async move {
            crate::create_channel_impl(&state, a.community_id, a.name, a.write_power, a.kind).await
        }
    );
    rpc!(
        m,
        "list_channels",
        CommunityIdArgs,
        |state, _sink, a| async move { crate::list_channels_impl(&state, a.community_id).await }
    );
    rpc!(
        m,
        "list_channel_messages",
        ListChannelMessagesArgs,
        |state, _sink, a| async move {
            crate::list_channel_messages_impl(
                &state,
                a.community_id,
                a.channel_id,
                a.since,
                a.limit,
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
                &state,
                a.community_id,
                a.channel_id,
                a.body,
                a.reply_to,
            )
            .await
        }
    );

    // Friends.
    rpc!(m, "list_friends", EmptyArgs, |state, _sink, _a| {
        async move { crate::list_friends_impl(&state).await }
    });
    rpc!(
        m,
        "generate_friend_token",
        GenerateFriendTokenArgs,
        |state, _sink, a| async move { crate::generate_friend_token_impl(&state, a.expires_at).await }
    );
    rpc!(m, "redeem_friend_token", UrlArgs, |state, sink, a| {
        async move { crate::redeem_friend_token_impl(&state, sink, a.url).await }
    });
    rpc!(
        m,
        "add_friend_by_key",
        AddFriendByKeyArgs,
        |state, sink, a| async move {
            crate::add_friend_by_key_impl(&state, sink, a.identity_pub_hex).await
        }
    );
    rpc!(
        m,
        "list_pending_friend_requests",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_pending_friend_requests_impl(&state).await }
    );
    rpc!(
        m,
        "accept_friend_request",
        OwnerIdHexArgs,
        |state, sink, a| async move {
            crate::accept_friend_request_impl(&state, sink, a.owner_id_hex).await
        }
    );
    rpc!(
        m,
        "decline_friend_request",
        OwnerIdHexArgs,
        |state, sink, a| async move {
            crate::decline_friend_request_impl(&state, sink, a.owner_id_hex).await
        }
    );

    // Spaces / DMs.
    rpc!(m, "add_space", AddSpaceArgs, |state, _sink, a| async move {
        crate::add_space_impl(&state, a.kind, a.name, a.members).await
    });
    rpc!(m, "send_dm", SendDmArgs, |state, _sink, a| async move {
        crate::send_dm_impl(&state, a.space_id, a.content, a.mime_type).await
    });
    rpc!(
        m,
        "read_dm_thread",
        ReadDmThreadArgs,
        |state, _sink, a| async move {
            crate::read_dm_thread_impl(&state, a.space_id, a.limit, a.before_hlc).await
        }
    );

    // Connectivity.
    rpc!(
        m,
        "connectivity_get_my_reachability_record",
        EmptyArgs,
        |state, _sink, _a| async move {
            crate::connectivity_get_my_reachability_record_impl(&state).await
        }
    );
    rpc!(
        m,
        "connectivity_list_peer_reachability",
        EmptyArgs,
        |state, _sink, _a| async move { crate::connectivity_list_peer_reachability_impl(&state).await }
    );

    // Network health.
    rpc!(
        m,
        "network_health_snapshot",
        EmptyArgs,
        |state, _sink, _a| async move { crate::network_health_snapshot_impl(&state).await }
    );
    rpc!(
        m,
        "network_health_run_self_test",
        EmptyArgs,
        |state, _sink, _a| async move { crate::network_health_run_self_test_impl(&state).await }
    );

    RpcRegistry { handlers: m }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::FanoutSink;

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
    async fn registry_has_exactly_the_curated_v1_surface() {
        let reg = build_registry();
        let names = reg.command_names();
        // Curated v1 surface: node lifecycle (start_node, stop_node) +
        // identity (get_owner_state, mint_owner_identity) +
        // communities (list_owner_communities,
        // create_community, list_community_members, generate_invite,
        // redeem_invite, join_open_community, leave_community) + channels
        // (create_channel, list_channels, list_channel_messages,
        // post_channel_message) + friends (list_friends, generate_friend_token,
        // redeem_friend_token, add_friend_by_key, list_pending_friend_requests,
        // accept_friend_request, decline_friend_request) + spaces/DMs
        // (add_space, send_dm, read_dm_thread) + connectivity
        // (connectivity_get_my_reachability_record,
        // connectivity_list_peer_reachability) + network health
        // (network_health_snapshot, network_health_run_self_test) = 29.
        assert_eq!(names.len(), 29, "curated v1 surface drifted: {names:?}");
        for must in [
            "start_node",
            "stop_node",
            "get_owner_state",
            "mint_owner_identity",
            "create_community",
            "redeem_invite",
            "post_channel_message",
            "send_dm",
            "read_dm_thread",
            "network_health_run_self_test",
        ] {
            assert!(names.contains(&must), "missing command: {must}");
        }
    }
}
