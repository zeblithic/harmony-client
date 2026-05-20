//! D-FROST per-community signed-event log engine. Mirrors the
//! `community_voting_log_engine.rs` pattern: one topic per community
//! at `harmony/community/{community_id}/dfrost`; mpsc-based publisher
//! and subscriber channels bridged to Zenoh by the event-loop adapter
//! (deferred — out of scope for ZEB-307; this ticket ships the engine,
//! a follow-up ships the adapter).

use crate::community_dfrost_types::SignedCommitteeEvent;
use crate::owner_state_types::OwnerAddr;
use std::collections::HashMap;

/// Replay-defense tracker keyed on `(actor, device_id)`. Records the
/// max-observed `(wall_ms, logical)` HLC per signer; any inbound event
/// whose HLC is at-or-below the recorded max is considered a replay /
/// loopback and silently dropped.
#[derive(Default)]
pub struct DfrostReplayTracker {
    seen_max: HashMap<(OwnerAddr, String), (u64, u32)>,
}

impl DfrostReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, event: &SignedCommitteeEvent) -> bool {
        match self
            .seen_max
            .get(&(event.actor, event.hlc.device_id.clone()))
        {
            Some((w, l)) => (event.hlc.wall_ms, event.hlc.logical) <= (*w, *l),
            None => false,
        }
    }

    pub fn record(&mut self, event: &SignedCommitteeEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let new_hlc = (event.hlc.wall_ms, event.hlc.logical);
        self.seen_max
            .entry(key)
            .and_modify(|cur| {
                if new_hlc > *cur {
                    *cur = new_hlc;
                }
            })
            .or_insert(new_hlc);
    }
}

#[cfg(test)]
mod tests {
    use crate::community_dfrost_log_engine::DfrostReplayTracker;
    use crate::community_dfrost_types::{
        DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
    };
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn test_event(actor: OwnerAddr, wall_ms: u64, logical: u32) -> SignedCommitteeEvent {
        let payload = ThresholdSignPayload {
            ceremony_id: [0u8; 32],
            message_hash: [0u8; 32],
            commitment_bytes: vec![],
            share_bytes: vec![],
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms,
                logical,
                device_id: "dev-a".into(),
            },
            actor,
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn replay_tracker_dedups_repeat_event() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        let e = test_event(addr, 100, 0);
        assert!(!t.contains(&e), "fresh event not contained");
        t.record(&e);
        assert!(t.contains(&e), "recorded event is contained");
    }

    #[test]
    fn replay_tracker_dedups_per_actor_device() {
        let mut t = DfrostReplayTracker::new();
        let addr_a = OwnerAddr([1u8; 16]);
        let addr_b = OwnerAddr([2u8; 16]);
        t.record(&test_event(addr_a, 100, 0));
        assert!(
            !t.contains(&test_event(addr_b, 100, 0)),
            "different actor not deduped"
        );
    }

    #[test]
    fn replay_tracker_advances_on_higher_hlc() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        t.record(&test_event(addr, 100, 0));
        let later = test_event(addr, 100, 1);
        assert!(!t.contains(&later), "advancing logical not deduped");
        t.record(&later);
        assert!(t.contains(&later), "advanced event recorded");
        assert!(t.contains(&test_event(addr, 100, 0)));
    }
}
