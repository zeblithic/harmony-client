//! Owner-private Contacts CRDT (ZEB-977). Per-identity petname + private
//! notes, keyed by the person-level owner_id hex. LWW-element-set exactly like
//! `notes_crdt.rs`: per-entry LWW on `updated_at`, delete = tombstone via
//! `deleted_at`. Replicated only across the owner's own device fleet
//! (ZEB-417 substrate) — NEVER published, broadcast, or joined into any
//! peer-visible payload. The privacy guarantee is structural: these bytes
//! live in their own files and their own fleet-sync dataset, outside every
//! published serialization (guard-tested in `owner_state_publish` tests).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One annotated identity. `owner_id_hex` duplicates the map key (mirrors
/// `Note.id`) so an entry is self-describing on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactEntry {
    #[serde(rename = "oi")]
    pub owner_id_hex: String,
    #[serde(rename = "pn", default, skip_serializing_if = "Option::is_none")]
    pub petname: Option<String>,
    #[serde(rename = "nt", default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// LOCAL wall-clock ms when this device first created the entry. Set once,
    /// preserved across edits and un-tombstoning ("when did I first annotate
    /// this identity"), NOT a claim about when the peer was first observed.
    #[serde(rename = "fs")]
    pub first_seen_ms: u64,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "ua")]
    pub updated_at: Hlc,
    #[serde(rename = "da", default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<Hlc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsDoc {
    #[serde(rename = "co")]
    pub contacts: BTreeMap<String, ContactEntry>,
}

// Manual CanonicalPayload registration (the `impl_canonical!` macro in
// owner_state_types.rs is module-private) — mirrors `notes_crdt.rs`.
impl CanonicalPayloadSealed for ContactEntry {}
impl CanonicalPayload for ContactEntry {}
impl CanonicalPayloadSealed for ContactsDoc {}
impl CanonicalPayload for ContactsDoc {}

/// A field write: outer `None` = leave the field unchanged; `Some(None)` =
/// clear it; `Some(Some(v))` = set it (trimmed; blank ⇒ clear).
pub type FieldWrite = Option<Option<String>>;

/// Trim + treat blank as absent, so a whitespace-only petname/notes value can
/// never occupy a field (parity with the frontend `nonEmpty()` ladder guard).
fn norm(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// What an annotation write would do, computed WITHOUT a minted HLC so the
/// command layer can skip minting entirely on a no-op (the peek-and-commit
/// discipline from `notes_commands.rs`: never advance the tracker for a write
/// that doesn't apply — a phantom mint skews the durability indicator).
enum AnnotationPlan {
    /// Nothing would change: pure clear on an absent/tombstoned entry, or a
    /// write whose normalized fields equal the live entry's current fields.
    NoOp,
    /// Both fields end up empty on a live entry → tombstone it.
    Tombstone,
    /// Set the entry to these fields (creating it when `creates`).
    Write {
        petname: Option<String>,
        notes: Option<String>,
        creates: bool,
    },
}

impl ContactsDoc {
    /// Live (non-tombstoned) entry by owner_id hex.
    pub fn get(&self, owner_id_hex: &str) -> Option<&ContactEntry> {
        self.contacts
            .get(&owner_id_hex.to_lowercase())
            .filter(|e| e.deleted_at.is_none())
    }

    /// Live entries in key order.
    pub fn list(&self) -> Vec<&ContactEntry> {
        self.contacts
            .values()
            .filter(|e| e.deleted_at.is_none())
            .collect()
    }

    /// Compute what a write would do against the current doc (no mutation).
    fn plan(&self, key: &str, petname: &FieldWrite, notes: &FieldWrite) -> AnnotationPlan {
        match self.contacts.get(key) {
            Some(e) => {
                let was_tombstoned = e.deleted_at.is_some();
                // A tombstoned entry's fields are semantically empty: a new
                // write starts from a blank record, not the pre-delete values.
                let mut new_petname = if was_tombstoned {
                    None
                } else {
                    e.petname.clone()
                };
                let mut new_notes = if was_tombstoned {
                    None
                } else {
                    e.notes.clone()
                };
                if let Some(w) = petname {
                    new_petname = norm(w.clone());
                }
                if let Some(w) = notes {
                    new_notes = norm(w.clone());
                }
                if new_petname.is_none() && new_notes.is_none() {
                    if was_tombstoned {
                        return AnnotationPlan::NoOp; // clearing an already-dead entry
                    }
                    return AnnotationPlan::Tombstone;
                }
                if !was_tombstoned && new_petname == e.petname && new_notes == e.notes {
                    return AnnotationPlan::NoOp; // identical content — no LWW bump
                }
                AnnotationPlan::Write {
                    petname: new_petname,
                    notes: new_notes,
                    creates: false,
                }
            }
            None => {
                let new_petname = petname.clone().and_then(norm);
                let new_notes = notes.clone().and_then(norm);
                if new_petname.is_none() && new_notes.is_none() {
                    return AnnotationPlan::NoOp; // nothing to create
                }
                AnnotationPlan::Write {
                    petname: new_petname,
                    notes: new_notes,
                    creates: true,
                }
            }
        }
    }

    /// Whether an annotation write would visibly change the doc. The command
    /// layer calls this under the doc lock BEFORE minting an HLC, so no-op
    /// writes never advance the tracker.
    pub fn would_change(&self, owner_id_hex: &str, petname: &FieldWrite, notes: &FieldWrite) -> bool {
        !matches!(
            self.plan(&owner_id_hex.to_lowercase(), petname, notes),
            AnnotationPlan::NoOp
        )
    }

    /// Apply a local annotation write stamped `at`. Creates the entry
    /// (`first_seen_ms = now_ms`) when absent, un-tombstones on a real write
    /// (preserving `created_at`/`first_seen_ms`), and tombstones when both
    /// fields end up empty — an annotation record with nothing in it should
    /// not linger. LWW-safe like `NotesDoc::upsert`: a stale `at` against an
    /// existing entry is ignored, and an identical-content write is a no-op
    /// (no LWW bump). Returns whether the doc visibly changed (callers skip
    /// `notify_dirty` — and skip minting via [`Self::would_change`] — on
    /// `false`).
    pub fn apply_annotation(
        &mut self,
        owner_id_hex: &str,
        petname: FieldWrite,
        notes: FieldWrite,
        at: Hlc,
        now_ms: u64,
    ) -> bool {
        let key = owner_id_hex.to_lowercase();
        if let Some(e) = self.contacts.get(&key) {
            if !at.is_strictly_newer_than(&e.updated_at) {
                return false; // stale write, ignore (LWW)
            }
        }
        match self.plan(&key, &petname, &notes) {
            AnnotationPlan::NoOp => false,
            AnnotationPlan::Tombstone => {
                let e = self
                    .contacts
                    .get_mut(&key)
                    .expect("Tombstone plan implies an existing entry");
                e.updated_at = at.clone();
                e.deleted_at = Some(at);
                true
            }
            AnnotationPlan::Write {
                petname: new_petname,
                notes: new_notes,
                creates,
            } => {
                if creates {
                    self.contacts.insert(
                        key.clone(),
                        ContactEntry {
                            owner_id_hex: key,
                            petname: new_petname,
                            notes: new_notes,
                            first_seen_ms: now_ms,
                            created_at: at.clone(),
                            updated_at: at,
                            deleted_at: None,
                        },
                    );
                } else {
                    let e = self
                        .contacts
                        .get_mut(&key)
                        .expect("non-creating Write plan implies an existing entry");
                    e.petname = new_petname;
                    e.notes = new_notes;
                    e.updated_at = at;
                    e.deleted_at = None;
                }
                true
            }
        }
    }

    /// Merge a remote doc, per-entry LWW on `updated_at`. `changed` reflects
    /// whether the VISIBLE projection (petname, notes, or live/tombstoned
    /// status) changed. Forward-skewed stamps are REJECTED, never clamped
    /// (ZEB-847 C9, same as `NotesDoc::merge_from`): a future stamp would
    /// out-LWW every later honest edit forever.
    pub fn merge_from(&mut self, remote: ContactsDoc) -> MergeOutcome {
        let receiver_now = crate::clock_trust::receiver_now_ms();
        let mut changed = false;
        for (key, r) in remote.contacts {
            if crate::clock_trust::wall_exceeds_forward_skew_logged(
                r.updated_at.wall_ms,
                receiver_now,
                "contacts.entry.updated_at",
            ) {
                continue;
            }
            let accept = match self.contacts.get(&key) {
                Some(l) => r.updated_at.is_strictly_newer_than(&l.updated_at),
                None => true,
            };
            if !accept {
                continue;
            }
            let visibly_changed = match self.contacts.get(&key) {
                Some(l) => {
                    l.petname != r.petname
                        || l.notes != r.notes
                        || l.deleted_at.is_some() != r.deleted_at.is_some()
                }
                None => r.deleted_at.is_none(), // a new LIVE entry is visible; new-but-tombstoned isn't
            };
            changed |= visibly_changed;
            self.contacts.insert(key, r);
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

    fn set_pet(doc: &mut ContactsDoc, hex: &str, pet: &str, at: Hlc, now: u64) -> bool {
        doc.apply_annotation(hex, Some(Some(pet.into())), None, at, now)
    }

    #[test]
    fn set_get_roundtrips_lowercases_and_trims() {
        let mut d = ContactsDoc::default();
        assert!(set_pet(&mut d, "AABB", "  Koya  ", hlc(1, "A"), 100));
        let e = d.get("aabb").expect("live entry");
        assert_eq!(e.petname.as_deref(), Some("Koya"));
        assert_eq!(e.first_seen_ms, 100);
        assert!(d.get("AABB").is_some(), "get lowercases");
    }

    #[test]
    fn partial_write_preserves_other_field() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 5);
        assert!(d.apply_annotation("aa", None, Some(Some("met at the garden".into())), hlc(2, "A"), 6));
        let e = d.get("aa").unwrap();
        assert_eq!(e.petname.as_deref(), Some("Koya"), "petname untouched by a notes-only write");
        assert_eq!(e.notes.as_deref(), Some("met at the garden"));
        assert_eq!(e.first_seen_ms, 5, "first_seen survives edits");
    }

    #[test]
    fn clearing_both_fields_tombstones() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 5);
        assert!(d.apply_annotation("aa", Some(None), None, hlc(2, "A"), 6));
        assert!(d.get("aa").is_none(), "entry hidden once both fields empty");
        assert!(
            d.contacts.get("aa").unwrap().deleted_at.is_some(),
            "tombstone retained for convergence"
        );
    }

    #[test]
    fn blank_write_counts_as_clear() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 5);
        assert!(d.apply_annotation("aa", Some(Some("   ".into())), None, hlc(2, "A"), 6));
        assert!(d.get("aa").is_none(), "whitespace petname clears; empty record tombstones");
    }

    #[test]
    fn clear_on_absent_entry_is_noop_returns_false() {
        let mut d = ContactsDoc::default();
        assert!(!d.apply_annotation("aa", Some(None), Some(None), hlc(1, "A"), 5));
        assert!(d.contacts.is_empty(), "no entry materialized by a pure clear");
    }

    #[test]
    fn clear_on_tombstoned_entry_is_noop() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 5);
        d.apply_annotation("aa", Some(None), None, hlc(2, "A"), 6);
        assert!(
            !d.apply_annotation("aa", Some(None), None, hlc(3, "A"), 7),
            "clearing an already-tombstoned entry is a no-op"
        );
    }

    #[test]
    fn untombstone_preserves_first_seen_and_created_at() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 100);
        let created = d.contacts.get("aa").unwrap().created_at.clone();
        d.apply_annotation("aa", Some(None), None, hlc(2, "A"), 200);
        assert!(d.get("aa").is_none());
        assert!(set_pet(&mut d, "aa", "Koya again", hlc(3, "A"), 999));
        let e = d.get("aa").expect("resurrected");
        assert_eq!(e.first_seen_ms, 100, "first_seen preserved across un-tombstone");
        assert_eq!(e.created_at, created, "created_at preserved");
        assert_eq!(e.notes, None, "fields start blank after resurrection");
    }

    #[test]
    fn identical_content_write_is_noop_no_lww_bump() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "Koya", hlc(1, "A"), 5);
        let before = d.contacts.get("aa").unwrap().updated_at.clone();
        assert!(
            !set_pet(&mut d, "aa", " Koya ", hlc(2, "A"), 6),
            "re-writing the same (trimmed) content is a no-op"
        );
        assert_eq!(
            d.contacts.get("aa").unwrap().updated_at,
            before,
            "no LWW bump on identical content"
        );
        assert!(!d.would_change("aa", &Some(Some("Koya".into())), &None));
        assert!(d.would_change("aa", &Some(Some("Other".into())), &None));
        assert!(!d.would_change("zz", &Some(None), &Some(None)), "pure clear on absent");
    }

    #[test]
    fn stale_update_is_ignored() {
        let mut d = ContactsDoc::default();
        set_pet(&mut d, "aa", "new", hlc(5, "A"), 1);
        assert!(!set_pet(&mut d, "aa", "old", hlc(1, "B"), 2), "stale write is a no-op");
        assert_eq!(d.get("aa").unwrap().petname.as_deref(), Some("new"));
    }

    #[test]
    fn lww_newer_update_wins_via_merge() {
        let mut a = ContactsDoc::default();
        set_pet(&mut a, "aa", "old", hlc(1, "A"), 1);
        let mut b = a.clone();
        set_pet(&mut b, "aa", "new", hlc(2, "B"), 1);
        let out = a.merge_from(b);
        assert!(out.changed);
        assert_eq!(a.get("aa").unwrap().petname.as_deref(), Some("new"));
    }

    #[test]
    fn concurrent_edit_converges_deterministically() {
        let mut a = ContactsDoc::default();
        set_pet(&mut a, "aa", "fromA", hlc(2, "A"), 1);
        let mut b = ContactsDoc::default();
        set_pet(&mut b, "aa", "fromB", hlc(2, "B"), 1);
        let mut a2 = a.clone();
        a2.merge_from(b.clone());
        let mut b2 = b.clone();
        b2.merge_from(a.clone());
        assert_eq!(
            a2.get("aa").unwrap().petname,
            b2.get("aa").unwrap().petname,
            "convergent"
        );
    }

    #[test]
    fn delete_tombstone_propagates_and_hides() {
        let mut a = ContactsDoc::default();
        set_pet(&mut a, "aa", "Koya", hlc(1, "A"), 1);
        let mut b = a.clone();
        b.apply_annotation("aa", Some(None), None, hlc(2, "B"), 2);
        let out = a.merge_from(b);
        assert!(out.changed);
        assert!(a.get("aa").is_none(), "tombstone hides the entry");
        assert!(a.contacts.get("aa").unwrap().deleted_at.is_some());
    }

    #[test]
    fn merge_of_identical_docs_reports_unchanged() {
        let mut a = ContactsDoc::default();
        set_pet(&mut a, "aa", "Koya", hlc(1, "A"), 1);
        let b = a.clone();
        let out = a.merge_from(b);
        assert!(!out.changed, "no visible change on identical merge");
    }

    /// ZEB-847 (C9) parity with NotesDoc: a future-stamped remote entry must
    /// be REJECTED at merge — an accepted future stamp would out-LWW every
    /// later honest edit forever.
    #[test]
    fn future_dated_edit_is_rejected_at_merge() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut local = ContactsDoc::default();
        set_pet(&mut local, "aa", "honest", hlc(now_ms, "A"), now_ms);
        let mut remote = ContactsDoc::default();
        set_pet(
            &mut remote,
            "aa",
            "malicious-future",
            hlc(now_ms + 400 * 24 * 60 * 60 * 1000, "B"),
            now_ms,
        );
        let out = local.merge_from(remote);
        assert!(!out.changed, "future-dated edit must not be visible");
        assert_eq!(local.get("aa").unwrap().petname.as_deref(), Some("honest"));
    }

    #[test]
    fn present_dated_edit_still_merges_normally() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut local = ContactsDoc::default();
        set_pet(&mut local, "aa", "honest", hlc(now_ms, "A"), now_ms);
        let mut remote = ContactsDoc::default();
        set_pet(&mut remote, "aa", "later-edit", hlc(now_ms + 1000, "B"), now_ms);
        let out = local.merge_from(remote);
        assert!(out.changed);
        assert_eq!(local.get("aa").unwrap().petname.as_deref(), Some("later-edit"));
    }

    #[test]
    fn cbor_round_trips_canonically() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut d = ContactsDoc::default();
        d.apply_annotation(
            "aa",
            Some(Some("Koya".into())),
            Some(Some("garden club".into())),
            hlc(1, "A"),
            7,
        );
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: ContactsDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }
}
