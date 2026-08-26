//! ZEB-548 Stage 2 (ZEB-990): the transport/dm/owner spine of harmony-client,
//! extracted from harmony-app as ONE coarse crate — iroh/zenoh transport,
//! tunnels, DM pipeline, friend graph/handshakes, fleet + owner state and
//! their sync engines. harmony-app re-exports every module so existing
//! `harmony_app::X` / `crate::X` call sites resolve unchanged.

// The extracted-tier modules the spine references as `crate::X`. Re-exported
// at the root so the moved files' paths stay byte-stable (same shim pattern
// harmony-app uses for its own call sites).
pub use harmony_core_types::{
    enrollment_verify, owner_state_crypto, owner_state_types, revoked_device_projection,
};
pub use harmony_foundation::{clock_trust, hlc_adopt_floor, node_event_sink};
pub use harmony_identity_crypto::{content_store, device_dataset_file, identity};

// CanonicalPayload registrations for the wire types that moved here whose
// certification previously lived in harmony-app's canonical_impls (the sealed
// trait's orphan rule requires the impl in the crate defining the type).
mod canonical_impls;

pub mod admission_oracle;
pub mod butler_deposit;
pub mod community_topology;
pub mod dm_crypto;
pub mod dm_envelope;
pub mod dm_inbox_crdt;
pub mod dm_inbox_ingest;
pub mod dm_inbox_persist;
pub mod dm_outbox;
pub mod dm_outhold;
pub mod dm_outhold_apply;
pub mod dm_outhold_persist;
pub mod dm_read_receipt;
pub mod dm_signing;
pub mod dm_tunnel_contact;
pub mod fleet_dataset_file;
pub mod fleet_key_epoch;
pub mod fleet_net;
pub mod fleet_net_persist;
pub mod fleet_peer_seed;
pub mod fleet_peer_seed_persist;
pub mod fleet_sync;
pub mod friend_graph;
pub mod friend_intro;
pub mod friend_rendezvous;
pub mod friend_requests;
pub mod inflight_handshake_gate;
pub mod iroh_butler_acceptor;
pub mod iroh_dial_driver;
pub mod iroh_endpoint;
pub mod iroh_framing;
pub mod iroh_friend_acceptor;
pub mod iroh_pex_acceptor;
pub mod iroh_transport_lifecycle;
pub mod iroh_tunnel_acceptor;
pub mod iroh_tunnel_dm_transport;
pub mod iroh_zenoh_registration;
pub mod network_health;
pub mod owner_quorum_enroll;
pub mod owner_quorum_sync;
pub mod owner_state;
pub mod owner_state_crdt;
pub mod owner_state_persist;
pub mod owner_state_sync;
pub mod owner_trust_sync;
pub mod peer_liveness;
pub mod pending_dm_invites;
pub mod pkarr_friend_publisher;
pub mod pkarr_invite_publisher;
pub mod protocol_versioning;
pub mod reachability_record;
pub mod reachability_resolver;
pub mod reconnect_supervisor;
pub mod referral_catalog;
pub mod relay_acceptor_watchdog;
pub mod reply_spill;
pub mod tunnel_manager;
pub mod tunnel_task;
pub(crate) mod zenoh_inbound_admission;
pub mod zenoh_iroh_link;
pub mod zenoh_iroh_transport;
