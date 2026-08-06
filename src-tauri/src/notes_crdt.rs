//! Owner-private Notes CRDT (ZEB-361 / ZEB-417 SP1). LWW-element-set: per-id
//! LWW on `updated_at`, delete = tombstone via `deleted_at`. Plain text only.

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    #[serde(rename = "id")]
    pub id: String, // ULID assigned by the IPC layer
    #[serde(rename = "tx")]
    pub text: String,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "ua")]
    pub updated_at: Hlc,
    #[serde(rename = "da", default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<Hlc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesDoc {
    #[serde(rename = "no")]
    pub notes: BTreeMap<String, Note>,
}

// Manual CanonicalPayload registration: the `impl_canonical!` macro in
// owner_state_types.rs is module-private, so we register these types with the
// two impls the macro expands to (mirroring `fleet_sync::FleetRootPublish`).
impl CanonicalPayloadSealed for Note {}
impl CanonicalPayload for Note {}
impl CanonicalPayloadSealed for NotesDoc {}
impl CanonicalPayload for NotesDoc {}

impl NotesDoc {
    /// Live (non-tombstoned) note by id.
    pub fn get(&self, id: &str) -> Option<&Note> {
        self.notes.get(id).filter(|n| n.deleted_at.is_none())
    }

    /// Live notes, oldest-first by id (ULID == creation order).
    pub fn list(&self) -> Vec<&Note> {
        self.notes
            .values()
            .filter(|n| n.deleted_at.is_none())
            .collect()
    }

    /// Insert or update. Caller supplies a freshly minted, monotone HLC.
    /// New id: created_at == updated_at. Existing: LWW on updated_at; a newer
    /// upsert un-tombstones (edit-after-delete with a newer clock wins).
    pub fn upsert(&mut self, id: String, text: String, at: Hlc) {
        match self.notes.get_mut(&id) {
            Some(n) if at.is_strictly_newer_than(&n.updated_at) => {
                n.text = text;
                n.updated_at = at;
                n.deleted_at = None;
            }
            Some(_) => {} // stale upsert, ignore
            None => {
                self.notes.insert(
                    id.clone(),
                    Note {
                        id,
                        text,
                        created_at: at.clone(),
                        updated_at: at,
                        deleted_at: None,
                    },
                );
            }
        }
    }

    /// Tombstone a note (LWW on the delete HLC).
    pub fn delete(&mut self, id: &str, at: Hlc) {
        if let Some(n) = self.notes.get_mut(id) {
            if at.is_strictly_newer_than(&n.updated_at) {
                n.updated_at = at.clone();
                n.deleted_at = Some(at);
            }
        }
    }

    /// Merge a remote doc, per-id LWW on `updated_at`. `changed` reflects
    /// whether the VISIBLE projection (text or live/tombstoned status) changed,
    /// so it drives a UI refresh only when something user-visible moved.
    pub fn merge_from(&mut self, remote: NotesDoc) -> MergeOutcome {
        // ZEB-847 (C9): bound each remote note's `updated_at` against the
        // receiver's own clock; a future stamp would LWW-win and silently drop
        // a legitimate local edit. Sampled once for the whole merge; `None`
        // (unreadable clock) ⇒ apply-all.
        let receiver_now = crate::clock_trust::receiver_now_ms();
        let mut changed = false;
        for (id, r) in remote.notes {
            if crate::clock_trust::wall_exceeds_forward_skew_logged(
                r.updated_at.wall_ms,
                receiver_now,
                "notes.note.updated_at",
            ) {
                continue;
            }
            let accept = match self.notes.get(&id) {
                Some(l) => r.updated_at.is_strictly_newer_than(&l.updated_at),
                None => true,
            };
            if !accept {
                continue;
            }
            let visibly_changed = match self.notes.get(&id) {
                Some(l) => l.text != r.text || l.deleted_at.is_some() != r.deleted_at.is_some(),
                None => r.deleted_at.is_none(), // a brand-new LIVE note is visible; a new-but-tombstoned isn't
            };
            changed |= visibly_changed;
            self.notes.insert(id, r);
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;
    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    #[test]
    fn lww_newer_update_wins() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "old".into(), hlc(1, "A"));
        let mut b = a.clone();
        b.upsert("n1".into(), "new".into(), hlc(2, "B"));
        let out = a.merge_from(b);
        assert!(out.changed);
        assert_eq!(a.get("n1").unwrap().text, "new");
    }

    #[test]
    fn delete_tombstone_propagates_and_hides() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "hi".into(), hlc(1, "A"));
        let mut b = a.clone();
        b.delete("n1", hlc(2, "B"));
        a.merge_from(b);
        assert!(a.get("n1").is_none(), "deleted note hidden from get/list");
        assert!(
            a.notes.get("n1").unwrap().deleted_at.is_some(),
            "tombstone retained for convergence"
        );
    }

    #[test]
    fn concurrent_edit_converges_deterministically() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "fromA".into(), hlc(2, "A"));
        let mut b = NotesDoc::default();
        b.upsert("n1".into(), "fromB".into(), hlc(2, "B"));
        let mut a2 = a.clone();
        a2.merge_from(b.clone());
        let mut b2 = b.clone();
        b2.merge_from(a.clone());
        assert_eq!(
            a2.get("n1").unwrap().text,
            b2.get("n1").unwrap().text,
            "convergent"
        );
    }

    #[test]
    fn stale_update_is_ignored() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "new".into(), hlc(5, "A"));
        let mut b = NotesDoc::default();
        b.upsert("n1".into(), "old".into(), hlc(1, "B"));
        let out = a.merge_from(b);
        assert!(!out.changed);
        assert_eq!(a.get("n1").unwrap().text, "new");
    }

    #[test]
    fn list_excludes_tombstoned_and_is_stable() {
        let mut d = NotesDoc::default();
        d.upsert("n1".into(), "first".into(), hlc(1, "A"));
        d.upsert("n2".into(), "second".into(), hlc(2, "A"));
        d.delete("n1", hlc(3, "A"));
        let live: Vec<_> = d.list().into_iter().map(|n| n.id.clone()).collect();
        assert_eq!(live, vec!["n2".to_string()]);
    }

    /// ZEB-847 (T-OWNER, Finding C9): a remote `NotesDoc` can carry a note
    /// stamped with an arbitrary future `updated_at`. `merge_from` is
    /// per-id LWW on `updated_at` (`notes_crdt.rs:91-109`), so an
    /// un-rejected future stamp would out-LWW every later honest edit
    /// forever, silently dropping the user's real edit.
    #[test]
    fn future_dated_note_edit_is_rejected_at_merge() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Local holds an honest edit at real `now`.
        let mut local = NotesDoc::default();
        local.upsert("n1".into(), "honest".into(), hlc(now_ms, "A"));

        // Remote carries the same note, edited, but stamped 400 days ahead —
        // it would out-LWW the honest edit forever if not rejected.
        let mut remote = NotesDoc::default();
        remote.upsert(
            "n1".into(),
            "malicious-future".into(),
            hlc(now_ms + 400 * 24 * 60 * 60 * 1000, "B"),
        );

        let out = local.merge_from(remote);

        assert!(!out.changed, "future-dated edit must not be visible");
        assert_eq!(
            local.get("n1").unwrap().text,
            "honest",
            "future-dated note edit must not override an honest edit"
        );
    }

    /// Companion to the above: an honest edit at real `now` (well within the
    /// forward-skew tolerance) must still merge normally.
    #[test]
    fn present_dated_note_edit_still_merges_normally() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut local = NotesDoc::default();
        local.upsert("n1".into(), "honest".into(), hlc(now_ms, "A"));

        let mut remote = NotesDoc::default();
        // Strictly newer than local's, but well within the forward-skew
        // tolerance window.
        remote.upsert("n1".into(), "later-edit".into(), hlc(now_ms + 1000, "B"));

        let out = local.merge_from(remote);

        assert!(out.changed);
        assert_eq!(
            local.get("n1").unwrap().text,
            "later-edit",
            "an in-window edit must merge normally"
        );
    }

    #[test]
    fn note_cbor_round_trips_canonically() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut d = NotesDoc::default();
        d.upsert("n1".into(), "hi".into(), hlc(1, "A"));
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: NotesDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }
}
