//! ZEB-338: owner-identity-loaded precondition helper.
//!
//! `start_node` tolerates the absence of an owner identity (pre-mint),
//! leaving the owner-derived `NodeState` fields as `None`. Owner-touching
//! IPCs require those fields. This helper extracts all of them atomically
//! (all-or-`NotLoaded`) so new code gets one clear precondition check and
//! one honest error instead of nine ad-hoc `.ok_or("crdt_state missing …")`
//! sites with a misleading "node not running?" message.
//!
//! Migration policy (spec §4.3): this is the recommended pattern for NEW
//! owner-touching IPCs. The ~144 existing ad-hoc sites are NOT mass-migrated
//! here — they get a phrasing-only sweep in the same PR (Task 5). Existing
//! IPCs adopt this helper incrementally as they're touched for other reasons.

use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;

use crate::community_channel_log_engine::ChannelLogRegistry;
use crate::community_state_sync::CommunitySyncRegistry;
use crate::dm_outbox::DmOutbox;
use crate::event_loop::CommunityAdapterRequest;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{Hlc, OwnerAddr};

/// All owner-derived handles, extracted atomically from `NodeState`.
/// Field names mirror `NodeState`'s, except `device_id`/`self_owner` which
/// drop the `dm_` prefix for readability at call sites.
pub struct OwnerLoadedHandles {
    pub crdt_state: Arc<TokioMutex<OwnerState>>,
    pub hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    pub device_id: String,
    pub self_owner: OwnerAddr,
    pub community_registry: Arc<CommunitySyncRegistry>,
    pub community_adapter_request_tx: mpsc::Sender<CommunityAdapterRequest>,
    pub channel_log_registry: Arc<ChannelLogRegistry>,
    pub dm_outbox: Arc<TokioMutex<DmOutbox>>,
    pub generation: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum OwnerLoadError {
    #[error(
        "Owner identity not loaded. The app may be restarting after a mint — try again in a moment."
    )]
    NotLoaded,
    #[error("NodeState lock poisoned: {0}")]
    LockPoisoned(String),
}

impl From<OwnerLoadError> for String {
    fn from(e: OwnerLoadError) -> String {
        e.to_string()
    }
}

/// Extract the owner-loaded handles, or `NotLoaded` if any is absent.
pub fn require_owner_loaded(
    state: &Mutex<crate::NodeState>,
) -> Result<OwnerLoadedHandles, OwnerLoadError> {
    let g = state
        .lock()
        .map_err(|e| OwnerLoadError::LockPoisoned(e.to_string()))?;
    Ok(OwnerLoadedHandles {
        crdt_state: g.crdt_state.clone().ok_or(OwnerLoadError::NotLoaded)?,
        hlc_tracker: g.hlc_tracker.clone().ok_or(OwnerLoadError::NotLoaded)?,
        device_id: g.dm_device_id.clone().ok_or(OwnerLoadError::NotLoaded)?,
        self_owner: g.dm_self_owner.ok_or(OwnerLoadError::NotLoaded)?,
        community_registry: g
            .community_registry
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        community_adapter_request_tx: g
            .community_adapter_request_tx
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        channel_log_registry: g
            .channel_log_registry
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        dm_outbox: g.dm_outbox.clone().ok_or(OwnerLoadError::NotLoaded)?,
        generation: g.generation,
    })
}
