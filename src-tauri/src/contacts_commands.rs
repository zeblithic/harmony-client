//! Owner-private Contacts IPC surface (ZEB-977). Tauri commands
//! `contacts_list` / `set_contact_petname` / `set_contact_notes` plus their
//! Tauri-free testable cores and the shared `*_impl` functions the HTTP RPC
//! layer (`api/rpc.rs`) calls — unlike Notes, contacts ARE on the HTTP
//! surface (headless parity + e2e driveability). The dataset handles live on
//! `NodeState` and stay `None` until the FleetSyncEngine is wired at startup;
//! until then the commands reject with "contacts dataset not loaded".
//!
//! There is deliberately NO friend-status gate here (contrast the retired
//! `set_friend_nickname`): any valid person-level owner_id can be annotated —
//! that is the point of generalizing nicknames into contacts.

use crate::contacts_crdt::{ContactEntry, ContactsDoc, FieldWrite};
use crate::node_event_sink::NodeEventSink;
use crate::owner_state_types::Hlc;
use harmony_crdt_sync::ReplayTracker;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Max petname length, in chars (carried over from ZEB-419's nickname cap).
pub const MAX_PETNAME_LEN: usize = 64;

/// Max notes length, in chars. Generous for a personal aide-mémoire; bounds
/// storage and rejects pathological input.
pub const MAX_NOTES_LEN: usize = 4096;

const CONTACTS_NOT_LOADED_MSG: &str = "contacts dataset not loaded";

/// Fleet-sync dataset name — forms the Zenoh topic
/// `harmony/owner/{addr_hex}/ds/contacts-v1` and doubles as the engine's
/// lookup key tag.
pub const CONTACTS_DATASET: &str = "contacts-v1";

/// Inbound Zenoh sample size gate for the contacts dataset (root-publish
/// frames are pointer-sized; 256 KiB matches the sibling small owner
/// datasets — owner-trust, quorum, fleet-keys).
pub const CONTACTS_DATASET_MAX_BYTES: usize = 256 * 1024;

/// Flattened, frontend-facing view of a live contact entry.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContactView {
    pub owner_id_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub petname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub first_seen_ms: u64,
    /// Wall-clock ms of the last write (the HLC's `wall_ms`).
    pub updated_ms: u64,
}

fn to_view(e: &ContactEntry) -> ContactView {
    ContactView {
        owner_id_hex: e.owner_id_hex.clone(),
        petname: e.petname.clone(),
        notes: e.notes.clone(),
        first_seen_ms: e.first_seen_ms,
        updated_ms: e.updated_at.wall_ms,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---- Testable cores (no Tauri State) ----

/// Live contacts in key (owner_id hex) order.
pub(crate) async fn contacts_list_core(doc: &Arc<Mutex<ContactsDoc>>) -> Vec<ContactView> {
    let d = doc.lock().await;
    d.list().into_iter().map(to_view).collect()
}

/// Apply a petname/notes write for `owner_id_hex` (already validated +
/// lowercased by the caller). Returns `(view_after_write, changed)`:
/// `view_after_write` is `None` when the write left no live entry (a clear
/// that tombstoned it, or a no-op against an absent entry).
///
/// HLC discipline (mirrors `notes_upsert_core`): the no-op check runs under
/// the doc lock BEFORE any minting, so a write that wouldn't change the doc
/// never advances the shared tracker. For an existing entry the candidate HLC
/// is peeked and committed atomically under a SINGLE tracker-lock hold; a
/// candidate that wouldn't beat the entry's `updated_at` (a concurrent newer
/// edit from a sibling device already won) bails with a recoverable error.
/// Lock order is doc→tracker, same as notes; deadlock-free since no path
/// locks the tracker while holding the doc in the opposite order.
pub(crate) async fn set_contact_field_core(
    doc: &Arc<Mutex<ContactsDoc>>,
    tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>,
    adopt_floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
    owner_id_hex: &str,
    petname: FieldWrite,
    notes: FieldWrite,
) -> Result<(Option<ContactView>, bool), String> {
    let mut d = doc.lock().await;
    if !d.would_change(owner_id_hex, &petname, &notes) {
        return Ok((d.get(owner_id_hex).map(to_view), false));
    }
    let at = if let Some(existing) = d.contacts.get(&owner_id_hex.to_lowercase()) {
        let mut t = tracker.lock().await;
        let candidate = crate::fleet_sync::peek_next_hlc(t.accepted(), adopt_floor, device_id);
        if !candidate.is_strictly_newer_than(&existing.updated_at) {
            return Err(
                "contact write was superseded (a newer edit from another device already won)"
                    .to_string(),
            );
        }
        t.observe_local(candidate.clone());
        candidate
    } else {
        crate::fleet_sync::mint_next_hlc(tracker, adopt_floor, device_id).await
    };
    let changed = d.apply_annotation(owner_id_hex, petname, notes, at, now_ms());
    debug_assert!(changed, "would_change passed but apply was a no-op");
    Ok((d.get(owner_id_hex).map(to_view), changed))
}

// ---- Shared impls (Tauri wrappers AND the HTTP RPC layer call these) ----

struct ContactsHandles {
    doc: Arc<Mutex<ContactsDoc>>,
    tracker: Arc<Mutex<ReplayTracker<String, Hlc>>>,
    adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: String,
    sync: Arc<crate::fleet_sync::FleetSyncEngine<ContactsDoc>>,
}

/// Snapshot the dataset handles under the sync NodeState mutex; release it
/// before any await (the notes-commands pattern).
fn snapshot_handles(state: &std::sync::Mutex<crate::NodeState>) -> Result<ContactsHandles, String> {
    let g = state
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    Ok(ContactsHandles {
        doc: g.contacts_doc.clone().ok_or(CONTACTS_NOT_LOADED_MSG)?,
        tracker: g.contacts_tracker.clone().ok_or(CONTACTS_NOT_LOADED_MSG)?,
        adopt_floor: g.hlc_adopt_floor.clone(),
        device_id: g
            .contacts_device_id
            .clone()
            .ok_or(CONTACTS_NOT_LOADED_MSG)?,
        sync: g.contacts_sync.clone().ok_or(CONTACTS_NOT_LOADED_MSG)?,
    })
}

/// Validate + normalize a field value: trim, blank ⇒ clear, cap length.
fn validate_field(
    value: Option<String>,
    max_len: usize,
    what: &str,
) -> Result<Option<String>, String> {
    let trimmed = value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(v) = &trimmed {
        if v.chars().count() > max_len {
            return Err(format!("{what} too long (max {max_len} characters)"));
        }
    }
    Ok(trimmed)
}

async fn set_contact_field_shared(
    state: &std::sync::Mutex<crate::NodeState>,
    sink: &dyn NodeEventSink,
    owner_id_hex: String,
    petname: FieldWrite,
    notes: FieldWrite,
) -> Result<Option<ContactView>, String> {
    // Validate the owner_id (reject malformed before any state work) — the
    // 16-byte person-level master owner_id, same decoder as the friend IPCs.
    crate::decode_owner_id_16(&owner_id_hex)?;
    let h = snapshot_handles(state)?;
    let (view, changed) = set_contact_field_core(
        &h.doc,
        &h.tracker,
        &h.adopt_floor,
        &h.device_id,
        &owner_id_hex.to_lowercase(),
        petname,
        notes,
    )
    .await?;
    if changed {
        h.sync.notify_dirty();
        sink.emit("contacts-changed", serde_json::Value::Null);
        // FriendsPanel joins petnames into FriendDto rows; keep it live.
        sink.emit("friend-list-changed", serde_json::Value::Null);
    }
    Ok(view)
}

pub(crate) async fn contacts_list_impl(
    state: &std::sync::Mutex<crate::NodeState>,
) -> Result<Vec<ContactView>, String> {
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.contacts_doc.clone().ok_or(CONTACTS_NOT_LOADED_MSG)?
    };
    Ok(contacts_list_core(&doc).await)
}

pub(crate) async fn set_contact_petname_impl(
    state: &std::sync::Mutex<crate::NodeState>,
    sink: &dyn NodeEventSink,
    owner_id_hex: String,
    petname: Option<String>,
) -> Result<Option<ContactView>, String> {
    let petname = validate_field(petname, MAX_PETNAME_LEN, "petname")?;
    set_contact_field_shared(state, sink, owner_id_hex, Some(petname), None).await
}

pub(crate) async fn set_contact_notes_impl(
    state: &std::sync::Mutex<crate::NodeState>,
    sink: &dyn NodeEventSink,
    owner_id_hex: String,
    notes: Option<String>,
) -> Result<Option<ContactView>, String> {
    let notes = validate_field(notes, MAX_NOTES_LEN, "notes")?;
    set_contact_field_shared(state, sink, owner_id_hex, None, Some(notes)).await
}

// ---- One-time ZEB-419 migration ----

/// Import the legacy `friend_nicknames.json` (ZEB-419) into a fresh
/// `contacts.cbor`, then rename the legacy file to `*.json.migrated` so it
/// can never re-import. Runs only while `contacts.cbor` does not exist yet.
/// Failures are logged, never fatal: a failed import leaves the legacy file
/// in place for the next boot to retry.
pub(crate) fn migrate_friend_nicknames_to_contacts(
    cipher: &crate::fleet_dataset_file::DatasetCipher,
    contacts_path: &std::path::Path,
    legacy_nicknames_path: &std::path::Path,
    device_id: &str,
) {
    if contacts_path.exists() || !legacy_nicknames_path.exists() {
        return;
    }
    // Error-preserving read — NOT `load_or_default`, which maps a corrupt or
    // unreadable file to an empty set. Here that would save an empty contacts
    // doc and rename the legacy file away, permanently discarding recoverable
    // nicknames. On any read/parse failure: leave the legacy file untouched
    // (a later fix or build can retry) and skip the migration.
    let legacy: crate::friend_nicknames::FriendNicknames = match std::fs::read(
        legacy_nicknames_path,
    )
    .map_err(|e| e.to_string())
    .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                error = %err,
                path = %legacy_nicknames_path.display(),
                "contacts migration: legacy nicknames unreadable; leaving file in place (no import, no rename)"
            );
            return;
        }
    };
    let mut doc = ContactsDoc::default();
    let now = now_ms();
    for (hex, e) in &legacy.entries {
        // Synthesize the HLC from the legacy LWW key: preserves the relative
        // ordering among imported entries and loses (correctly) to any real
        // post-migration edit, whose minted HLC is current wall-clock.
        // Clamped to the local clock: a legacy stamp written under a skewed
        // clock would otherwise sit in the future and permanently reject
        // every later edit (`set_contact_field_core`'s superseded gate never
        // mints past it, and sibling merges skew-reject it). Only the LWW key
        // is clamped; `first_seen_ms` keeps the legacy timestamp.
        let at = Hlc {
            wall_ms: e.updated_ms.min(now),
            logical: 0,
            device_id: device_id.to_string(),
        };
        doc.apply_annotation(hex, Some(Some(e.nickname.clone())), None, at, e.updated_ms);
    }
    if let Err(err) = crate::contacts_persist::save(cipher, contacts_path, &doc) {
        tracing::error!(
            error = %err,
            "contacts migration: save failed; legacy nicknames left in place"
        );
        return;
    }
    let migrated = legacy_nicknames_path.with_extension("json.migrated");
    if let Err(err) = std::fs::rename(legacy_nicknames_path, &migrated) {
        tracing::warn!(
            error = %err,
            "contacts migration: legacy rename failed (import itself succeeded)"
        );
    } else {
        tracing::info!(
            count = legacy.entries.len(),
            "migrated ZEB-419 friend nicknames into the contacts store"
        );
    }
}

// ---- Tauri wrappers ----

/// List all live contact annotations.
#[tauri::command]
pub async fn contacts_list(
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Vec<ContactView>, String> {
    contacts_list_impl(state.inner()).await
}

/// Set or clear the local-only petname for any identity (`None`/blank
/// clears). Never published or broadcast; fleet-synced across the owner's
/// own devices only.
#[tauri::command]
pub async fn set_contact_petname(
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
    owner_id_hex: String,
    petname: Option<String>,
) -> Result<Option<ContactView>, String> {
    set_contact_petname_impl(state.inner(), &app, owner_id_hex, petname).await
}

/// Set or clear the local-only private notes for any identity (`None`/blank
/// clears). Never published or broadcast; fleet-synced across the owner's
/// own devices only.
#[tauri::command]
pub async fn set_contact_notes(
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
    owner_id_hex: String,
    notes: Option<String>,
) -> Result<Option<ContactView>, String> {
    set_contact_notes_impl(state.inner(), &app, owner_id_hex, notes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)] // test fixture tuple, named at the two call layers
    fn fixtures() -> (
        Arc<Mutex<ContactsDoc>>,
        Arc<Mutex<ReplayTracker<String, Hlc>>>,
        crate::hlc_adopt_floor::HlcAdoptFloor,
    ) {
        (
            Arc::new(Mutex::new(ContactsDoc::default())),
            Arc::new(Mutex::new(ReplayTracker::new("dev-A".into()))),
            crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        )
    }

    #[tokio::test]
    async fn set_get_list_roundtrip() {
        let (doc, tracker, floor) = fixtures();
        let (view, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aabb",
            Some(Some("  Koya  ".into())),
            None,
        )
        .await
        .unwrap();
        assert!(changed);
        let view = view.expect("live entry after set");
        assert_eq!(view.petname.as_deref(), Some("Koya"));
        let listed = contacts_list_core(&doc).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].owner_id_hex, "aabb");
    }

    #[tokio::test]
    async fn blank_clears_and_both_cleared_tombstones() {
        let (doc, tracker, floor) = fixtures();
        set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(Some("Koya".into())),
            Some(Some("gardener".into())),
        )
        .await
        .unwrap();
        // Clear petname (blank string), notes remain → still live.
        let (view, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(Some("   ".into())),
            None,
        )
        .await
        .unwrap();
        assert!(changed);
        assert_eq!(view.unwrap().petname, None);
        // Clear notes too → entry tombstoned, view None, list empty.
        let (view, changed) =
            set_contact_field_core(&doc, &tracker, &floor, "dev-A", "aa", None, Some(None))
                .await
                .unwrap();
        assert!(changed);
        assert!(view.is_none(), "tombstoned entry yields no view");
        assert!(contacts_list_core(&doc).await.is_empty());
    }

    #[tokio::test]
    async fn noop_writes_mint_no_hlc() {
        let (doc, tracker, floor) = fixtures();
        // Pure clear on an absent entry: no mint.
        let (view, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(None),
            Some(None),
        )
        .await
        .unwrap();
        assert!(!changed);
        assert!(view.is_none());
        assert!(
            tracker.lock().await.accepted().get("dev-A").is_none(),
            "no HLC minted for a no-op write"
        );
        // Identical-content rewrite: no mint beyond the original.
        set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(Some("Koya".into())),
            None,
        )
        .await
        .unwrap();
        let after_first = tracker.lock().await.accepted().get("dev-A").cloned();
        let (_, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(Some(" Koya ".into())),
            None,
        )
        .await
        .unwrap();
        assert!(!changed, "identical content is a no-op");
        assert_eq!(
            tracker.lock().await.accepted().get("dev-A").cloned(),
            after_first,
            "no HLC minted for an identical-content write"
        );
    }

    #[tokio::test]
    async fn superseded_write_errs_and_mints_nothing() {
        let (doc, tracker, floor) = fixtures();
        set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aa",
            Some(Some("mine".into())),
            None,
        )
        .await
        .unwrap();
        // A sibling device edits with a far-future HLC (inbound merge).
        doc.lock().await.apply_annotation(
            "aa",
            Some(Some("remote".into())),
            None,
            Hlc {
                wall_ms: u64::MAX,
                logical: u32::MAX,
                device_id: "dev-B".into(),
            },
            1,
        );
        // Our fresh-tracker write mints ≈now — strictly older — must Err and
        // must not advance the fresh tracker.
        let fresh: Arc<Mutex<ReplayTracker<String, Hlc>>> =
            Arc::new(Mutex::new(ReplayTracker::new("dev-C".into())));
        let fresh_floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let result = set_contact_field_core(
            &doc,
            &fresh,
            &fresh_floor,
            "dev-C",
            "aa",
            Some(Some("stale".into())),
            None,
        )
        .await;
        assert!(result.is_err(), "superseded write must Err, got {result:?}");
        assert!(
            fresh.lock().await.accepted().get("dev-C").is_none(),
            "a superseded write must not mint an HLC"
        );
        assert_eq!(
            doc.lock().await.get("aa").unwrap().petname.as_deref(),
            Some("remote"),
            "the newer remote edit stands"
        );
    }

    #[tokio::test]
    async fn caps_enforced() {
        let long_pet: String = "x".repeat(MAX_PETNAME_LEN + 1);
        assert!(validate_field(Some(long_pet), MAX_PETNAME_LEN, "petname").is_err());
        let long_notes: String = "y".repeat(MAX_NOTES_LEN + 1);
        assert!(validate_field(Some(long_notes), MAX_NOTES_LEN, "notes").is_err());
        let exact: String = "z".repeat(MAX_PETNAME_LEN);
        assert_eq!(
            validate_field(Some(exact.clone()), MAX_PETNAME_LEN, "petname").unwrap(),
            Some(exact)
        );
        assert_eq!(
            validate_field(Some("   ".into()), MAX_PETNAME_LEN, "petname").unwrap(),
            None,
            "blank normalizes to clear"
        );
    }

    /// The generalization at the heart of ZEB-977: annotation works for an
    /// arbitrary valid identity with NO friend relationship at all (the core
    /// has no friend-graph access to gate on — this pins that shape).
    #[tokio::test]
    async fn no_friend_gate_arbitrary_identity_annotatable() {
        let (doc, tracker, floor) = fixtures();
        let arbitrary = "0123456789abcdef0123456789abcdef"; // 32 hex chars
        let (view, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            arbitrary,
            Some(Some("stranger I met".into())),
            Some(Some("from the town hall".into())),
        )
        .await
        .unwrap();
        assert!(changed);
        let view = view.unwrap();
        assert_eq!(view.owner_id_hex, arbitrary);
        assert_eq!(view.notes.as_deref(), Some("from the town hall"));
    }

    #[test]
    fn migration_imports_legacy_nicknames_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let contacts_path = dir.path().join("contacts.cbor");
        let legacy_path = dir.path().join("friend_nicknames.json");
        let mut legacy = crate::friend_nicknames::FriendNicknames::default();
        legacy.set("AABB", Some("Koya"), 111);
        legacy.set("ccdd", Some("Priya"), 222);
        legacy.save(&legacy_path).unwrap();

        super::migrate_friend_nicknames_to_contacts(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &legacy_path,
            "dev-A",
        );

        let doc = crate::contacts_persist::load(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
        )
        .unwrap();
        assert_eq!(
            doc.get("aabb").unwrap().petname.as_deref(),
            Some("Koya"),
            "legacy key lowercased + nickname imported as petname"
        );
        assert_eq!(doc.get("ccdd").unwrap().petname.as_deref(), Some("Priya"));
        assert_eq!(doc.get("aabb").unwrap().first_seen_ms, 111);
        assert!(!legacy_path.exists(), "legacy file renamed away");
        assert!(
            dir.path().join("friend_nicknames.json.migrated").exists(),
            "legacy preserved under .migrated"
        );
    }

    #[test]
    fn migration_skips_when_contacts_exist() {
        let dir = tempfile::tempdir().unwrap();
        let contacts_path = dir.path().join("contacts.cbor");
        crate::contacts_persist::save(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &ContactsDoc::default(),
        )
        .unwrap();
        let legacy_path = dir.path().join("friend_nicknames.json");
        let mut legacy = crate::friend_nicknames::FriendNicknames::default();
        legacy.set("aabb", Some("Koya"), 111);
        legacy.save(&legacy_path).unwrap();

        super::migrate_friend_nicknames_to_contacts(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &legacy_path,
            "dev-A",
        );

        assert!(legacy_path.exists(), "legacy untouched when contacts exist");
        let doc = crate::contacts_persist::load(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
        )
        .unwrap();
        assert!(
            doc.get("aabb").is_none(),
            "no import into an existing store"
        );
    }

    #[test]
    fn migration_noop_without_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let contacts_path = dir.path().join("contacts.cbor");
        super::migrate_friend_nicknames_to_contacts(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &dir.path().join("friend_nicknames.json"),
            "dev-A",
        );
        assert!(
            !contacts_path.exists(),
            "no store materialized from nothing"
        );
    }

    /// A far-future legacy `updated_ms` (skewed clock at write time) must be
    /// clamped at import: an unclamped stamp would out-LWW every freshly
    /// minted HLC forever, making the imported entry permanently uneditable
    /// (`set_contact_field_core`'s superseded gate) — and sibling merges
    /// would skew-reject it.
    #[tokio::test]
    async fn migration_clamps_future_legacy_stamp_and_stays_editable() {
        let dir = tempfile::tempdir().unwrap();
        let contacts_path = dir.path().join("contacts.cbor");
        let legacy_path = dir.path().join("friend_nicknames.json");
        let far_future = super::now_ms() + 400 * 24 * 60 * 60 * 1000;
        let mut legacy = crate::friend_nicknames::FriendNicknames::default();
        legacy.set("aabb", Some("SkewedImport"), far_future);
        legacy.save(&legacy_path).unwrap();

        super::migrate_friend_nicknames_to_contacts(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &legacy_path,
            "dev-A",
        );

        let imported = crate::contacts_persist::load(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
        )
        .unwrap();
        let entry = imported.contacts.get("aabb").expect("imported");
        assert!(
            entry.updated_at.wall_ms <= super::now_ms(),
            "imported LWW stamp clamped to the local clock"
        );
        assert_eq!(
            entry.first_seen_ms, far_future,
            "first_seen keeps the legacy timestamp; only the LWW key clamps"
        );

        // The load-bearing consequence: the imported entry is still editable
        // through the real write path.
        let doc = Arc::new(Mutex::new(imported));
        let tracker: Arc<Mutex<ReplayTracker<String, Hlc>>> =
            Arc::new(Mutex::new(ReplayTracker::new("dev-A".into())));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let (view, changed) = set_contact_field_core(
            &doc,
            &tracker,
            &floor,
            "dev-A",
            "aabb",
            Some(Some("Edited".into())),
            None,
        )
        .await
        .expect("imported entry must remain editable");
        assert!(changed);
        assert_eq!(view.unwrap().petname.as_deref(), Some("Edited"));
    }

    /// A corrupt legacy file must abort the migration WITHOUT renaming it —
    /// `load_or_default`-style empty-on-corrupt would save an empty store and
    /// rename the recoverable bytes away permanently.
    #[test]
    fn migration_leaves_corrupt_legacy_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let contacts_path = dir.path().join("contacts.cbor");
        let legacy_path = dir.path().join("friend_nicknames.json");
        std::fs::write(&legacy_path, b"not json at all").unwrap();

        super::migrate_friend_nicknames_to_contacts(
            &crate::fleet_dataset_file::test_cipher(),
            &contacts_path,
            &legacy_path,
            "dev-A",
        );

        assert!(
            legacy_path.exists(),
            "corrupt legacy file left in place for recovery"
        );
        assert!(
            !dir.path().join("friend_nicknames.json.migrated").exists(),
            "no rename on a failed parse"
        );
        assert!(
            !contacts_path.exists(),
            "no empty store written over a failed import"
        );
    }

    /// Two-engine cross-DEVICE convergence proofs for the Contacts dataset —
    /// a port of the notes `two_engine_sync` suite: two real
    /// `FleetSyncEngine<ContactsDoc>` instances configured as `start_node`
    /// configures production (merge_from merger, `publish_seen: true`, lookup
    /// tag `b"contacts-v1"`), sharing one CAS and cross-wired
    /// publisher↔subscriber, driving the REAL `set_contact_field_core` write
    /// path. The Zenoh hop can't be unit-tested without a session; the
    /// forwarder stands in byte-for-byte.
    mod two_engine_sync {
        use super::{contacts_list_core, set_contact_field_core};
        use crate::contacts_crdt::ContactsDoc;
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{
            FleetPersist, FleetSyncConfig, FleetSyncEngine, Merger, SyncError, DEFAULT_DEBOUNCE_MS,
        };
        use crate::owner_state_crypto::KeyTree;
        use crate::owner_state_types::Hlc;
        use harmony_crdt_sync::ReplayTracker;
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{mpsc, Mutex};

        /// Convergence is the assertion; on-disk durability is covered by
        /// `contacts_persist.rs`. A no-op sink keeps these tests free of
        /// per-engine tempdir plumbing.
        struct NoopContactsPersist;
        impl FleetPersist<ContactsDoc> for NoopContactsPersist {
            fn persist(&self, _: &ContactsDoc, _: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
                Ok(())
            }
        }

        struct ContactsEngine {
            engine: FleetSyncEngine<ContactsDoc>,
            doc: Arc<Mutex<ContactsDoc>>,
            tracker: Arc<Mutex<ReplayTracker<String, Hlc>>>,
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
            out_rx: mpsc::Receiver<Vec<u8>>,
            in_tx: mpsc::Sender<Vec<u8>>,
        }

        fn build(device_id: &str, kt: Arc<KeyTree>, cas: Arc<dyn ContentStore>) -> ContactsEngine {
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            let doc = Arc::new(Mutex::new(ContactsDoc::default()));
            let tracker = Arc::new(Mutex::new(ReplayTracker::new(device_id.to_string())));
            let merger: Merger<ContactsDoc> = Arc::new(|local, remote| local.merge_from(remote));
            let adopt_floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
            let engine = FleetSyncEngine::<ContactsDoc>::new(FleetSyncConfig {
                keys: Some(crate::owner_state_crypto::FleetKeySet::new(kt)),
                device_id: device_id.to_string(),
                state: Arc::clone(&doc),
                merger,
                replay_tracker: Arc::clone(&tracker),
                content_store: cas,
                publisher_tx: out_tx,
                subscriber_rx: in_rx,
                persist: Arc::new(NoopContactsPersist),
                lookup_key_tag: super::super::CONTACTS_DATASET.as_bytes(),
                debounce_ms: DEFAULT_DEBOUNCE_MS,
                publish_seen: true,
                on_applied: None,
                sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
                adopt_floor: adopt_floor.clone(),
            });
            ContactsEngine {
                engine,
                doc,
                tracker,
                adopt_floor,
                out_rx,
                in_tx,
            }
        }

        /// Stand-in for the Zenoh hop (see the notes suite for the rationale
        /// on exit conditions).
        fn spawn_forwarder(
            mut a_out: mpsc::Receiver<Vec<u8>>,
            b_in: mpsc::Sender<Vec<u8>>,
            mut b_out: mpsc::Receiver<Vec<u8>>,
            a_in: mpsc::Sender<Vec<u8>>,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(f) = a_out.recv() => {
                            if b_in.send(f).await.is_err() {
                                break;
                            }
                        }
                        Some(f) = b_out.recv() => {
                            if a_in.send(f).await.is_err() {
                                break;
                            }
                        }
                        else => break,
                    }
                }
            })
        }

        async fn teardown(
            a_engine: FleetSyncEngine<ContactsDoc>,
            b_engine: FleetSyncEngine<ContactsDoc>,
            fwd: tokio::task::JoinHandle<()>,
        ) {
            let _ = a_engine.shutdown().await;
            let _ = b_engine.shutdown().await;
            match tokio::time::timeout(Duration::from_secs(2), fwd).await {
                Ok(joined) => joined.expect("forwarder task panicked"),
                Err(_) => panic!("forwarder did not terminate after both engines shut down"),
            }
        }

        /// Bounded condition-polling — a deterministic readiness barrier.
        async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
        where
            F: FnMut() -> Fut,
            Fut: std::future::Future<Output = bool>,
        {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if cond().await {
                    return true;
                }
                if tokio::time::Instant::now() > deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        }

        async fn set_pet(
            doc: &Arc<Mutex<ContactsDoc>>,
            tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>,
            adopt_floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
            device: &str,
            hex: &str,
            pet: &str,
        ) -> Option<super::ContactView> {
            set_contact_field_core(
                doc,
                tracker,
                adopt_floor,
                device,
                hex,
                Some(Some(pet.into())),
                None,
            )
            .await
            .expect("local contact write")
            .0
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn petname_set_on_one_device_appears_on_the_sibling() {
            let kt = Arc::new(KeyTree::derive(&[0x77u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            set_pet(&a.doc, &a.tracker, &a.adopt_floor, "dev-A", "aabb", "Koya")
                .await
                .expect("live");
            a.engine.flush_now().await.expect("flush A");

            let b_doc = Arc::clone(&b.doc);
            let converged = wait_until(
                || {
                    let b_doc = Arc::clone(&b_doc);
                    async move {
                        contacts_list_core(&b_doc).await.iter().any(|c| {
                            c.owner_id_hex == "aabb" && c.petname.as_deref() == Some("Koya")
                        })
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(converged, "B never received A's petname within 5s");

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn independent_annotations_converge_to_the_union() {
            let kt = Arc::new(KeyTree::derive(&[0x88u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            set_pet(
                &a.doc,
                &a.tracker,
                &a.adopt_floor,
                "dev-A",
                "aa11",
                "from-A",
            )
            .await;
            set_pet(
                &b.doc,
                &b.tracker,
                &b.adopt_floor,
                "dev-B",
                "bb22",
                "from-B",
            )
            .await;
            a.engine.flush_now().await.unwrap();
            b.engine.flush_now().await.unwrap();

            let a_doc = Arc::clone(&a.doc);
            let b_doc = Arc::clone(&b.doc);
            let converged = wait_until(
                || {
                    let (a_doc, b_doc) = (Arc::clone(&a_doc), Arc::clone(&b_doc));
                    async move {
                        let al = contacts_list_core(&a_doc).await;
                        let bl = contacts_list_core(&b_doc).await;
                        let has = |l: &[super::ContactView], k: &str| {
                            l.iter().any(|c| c.owner_id_hex == k)
                        };
                        has(&al, "aa11") && has(&al, "bb22") && has(&bl, "aa11") && has(&bl, "bb22")
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(converged, "devices did not converge to the union within 5s");

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_edits_same_contact_converge() {
            let kt = Arc::new(KeyTree::derive(&[0x99u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            // Seed on A, replicate to B.
            set_pet(&a.doc, &a.tracker, &a.adopt_floor, "dev-A", "aabb", "v1").await;
            a.engine.flush_now().await.unwrap();
            {
                let b_doc = Arc::clone(&b.doc);
                assert!(
                    wait_until(
                        || {
                            let b_doc = Arc::clone(&b_doc);
                            async move {
                                contacts_list_core(&b_doc)
                                    .await
                                    .iter()
                                    .any(|c| c.owner_id_hex == "aabb")
                            }
                        },
                        Duration::from_secs(5),
                    )
                    .await,
                    "seed entry did not replicate to B"
                );
            }

            // Both edit the SAME contact concurrently; LWW winner must land on both.
            let (ra, rb) = tokio::join!(
                set_contact_field_core(
                    &a.doc,
                    &a.tracker,
                    &a.adopt_floor,
                    "dev-A",
                    "aabb",
                    Some(Some("edited-by-A".into())),
                    None,
                ),
                set_contact_field_core(
                    &b.doc,
                    &b.tracker,
                    &b.adopt_floor,
                    "dev-B",
                    "aabb",
                    Some(Some("edited-by-B".into())),
                    None,
                ),
            );
            ra.unwrap();
            rb.unwrap();
            a.engine.flush_now().await.unwrap();
            b.engine.flush_now().await.unwrap();

            let a_doc = Arc::clone(&a.doc);
            let b_doc = Arc::clone(&b.doc);
            let converged = wait_until(
                || {
                    let (a_doc, b_doc) = (Arc::clone(&a_doc), Arc::clone(&b_doc));
                    async move {
                        let get = |l: Vec<super::ContactView>| {
                            l.into_iter()
                                .find(|c| c.owner_id_hex == "aabb")
                                .and_then(|c| c.petname)
                        };
                        let at = get(contacts_list_core(&a_doc).await);
                        let bt = get(contacts_list_core(&b_doc).await);
                        match (at, bt) {
                            (Some(at), Some(bt)) => {
                                at == bt && (at == "edited-by-A" || at == "edited-by-B")
                            }
                            _ => false,
                        }
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(
                converged,
                "concurrent edits did not converge to one LWW winner"
            );

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn clear_on_one_device_tombstones_on_the_sibling() {
            let kt = Arc::new(KeyTree::derive(&[0xAAu8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            set_pet(
                &a.doc,
                &a.tracker,
                &a.adopt_floor,
                "dev-A",
                "aabb",
                "ephemeral",
            )
            .await;
            a.engine.flush_now().await.unwrap();
            {
                let b_doc = Arc::clone(&b.doc);
                assert!(
                    wait_until(
                        || {
                            let b_doc = Arc::clone(&b_doc);
                            async move {
                                contacts_list_core(&b_doc)
                                    .await
                                    .iter()
                                    .any(|c| c.owner_id_hex == "aabb")
                            }
                        },
                        Duration::from_secs(5),
                    )
                    .await,
                    "entry did not replicate to B before the clear"
                );
            }

            // Clear the only field on A → tombstone; B must converge to empty.
            let (view, changed) = set_contact_field_core(
                &a.doc,
                &a.tracker,
                &a.adopt_floor,
                "dev-A",
                "aabb",
                Some(None),
                None,
            )
            .await
            .unwrap();
            assert!(changed);
            assert!(view.is_none(), "clear of the only field tombstones");
            a.engine.flush_now().await.unwrap();

            let b_doc = Arc::clone(&b.doc);
            let converged = wait_until(
                || {
                    let b_doc = Arc::clone(&b_doc);
                    async move {
                        !contacts_list_core(&b_doc)
                            .await
                            .iter()
                            .any(|c| c.owner_id_hex == "aabb")
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(converged, "B still lists the contact A cleared");

            teardown(a.engine, b.engine, fwd).await;
        }
    }
}
