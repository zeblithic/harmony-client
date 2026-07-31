//! ZEB-839 — durable last-known peer profile cards.
//!
//! Persists the VERIFIED profile-card cache (`owner_id` → last-known display
//! name / status / avatar CID) so peers render by name even while offline and
//! across app restarts. This is the disk-persisted card cache that ZEB-341
//! deliberately deferred ("Out of scope: disk-persisted card cache").
//!
//! Design: `docs/superpowers/specs/2026-07-31-zeb-839-durable-peer-profile-cache-design.md`
//!
//! Lifecycle & invariants:
//! - **Write-through** on [`crate::profile_card_broadcast::ProfileCardCache`]:
//!   every verified, strictly-newer card that lands in the in-memory cache is
//!   also upserted here (newer-HLC-wins — the same merge rule) and flushed to
//!   disk off the async hot path.
//! - **Loaded** at `start_node` and consulted as a fallback UNDER the live
//!   cache, so an offline peer (or a fresh restart) resolves from disk.
//! - **Per-identity isolation:** the file is scoped to the LOCAL owner's id, so
//!   a different identity on the same profile/machine cannot inherit the
//!   previous identity's peer-name knowledge (the ZEB-586 cross-identity lesson).
//! - **Verified-only by construction:** only cards that already passed
//!   `verify_card` reach the write-through, so a stored name is one we
//!   cryptographically verified was cert-bound to its `owner_id` when seen. We
//!   do not re-verify on load (revocation is a separate concern; see the spec).

use crate::owner_state_persist::{save_atomically, PersistError};
use crate::owner_state_types::Hlc;
use crate::profile_card_broadcast::{DiscoveredCardInfo, ProfileCardBroadcast};
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// On-disk schema version. Bump when the byte semantics change.
const CARD_STORE_SCHEMA_V1: u8 = 1;

/// Default soft cap on distinct owners retained. Entries are tiny (≤~200 B), so
/// 10k ≈ ~2 MB worst case. There is deliberately **no TTL** — a TTL would
/// re-introduce the exact "name vanishes after a while" symptom this store
/// removes. On overflow the least-recently-UPDATED entry (by our-side receipt
/// order) is evicted.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// The persisted snapshot of one peer's last-known card. Mirrors the in-memory
/// `CachedCard` fields; byte fields use the same bstr helpers as the wire type
/// for a compact, consistent local encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedCard {
    #[serde(
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_id: [u8; 16],
    pub display_name: String,
    pub status_text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub avatar_cid: Option<[u8; 32]>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub profile_page_root: Option<[u8; 32]>,
    pub shared_at: Hlc,
}

impl PersistedCard {
    /// Snapshot a verified broadcast into its persistable form.
    pub fn from_broadcast(card: &ProfileCardBroadcast) -> Self {
        Self {
            owner_id: card.owner_id,
            display_name: card.display_name.clone(),
            status_text: card.status_text.clone(),
            avatar_cid: card.avatar_cid,
            profile_page_root: card.profile_page_root,
            shared_at: card.shared_at.clone(),
        }
    }

    /// Convert to the frontend DTO the live cache also returns, so a
    /// store-served fallback is indistinguishable from a live hit.
    pub fn to_discovered(&self) -> DiscoveredCardInfo {
        DiscoveredCardInfo {
            owner_id_hex: hex::encode(self.owner_id),
            display_name: self.display_name.clone(),
            status_text: self.status_text.clone(),
            avatar_cid: self.avatar_cid.map(hex::encode),
            profile_page_root: self.profile_page_root.map(hex::encode),
        }
    }
}

struct StoredEntry {
    card: PersistedCard,
    /// Monotonic our-side update order, for least-recently-updated eviction.
    /// In-memory only; not persisted (rebuilt on load).
    seq: u64,
}

struct Inner {
    map: HashMap<[u8; 16], StoredEntry>,
    next_seq: u64,
}

/// Durable `owner_id` → last-known-card store. See module docs.
pub struct PersistentCardStore {
    path: PathBuf,
    max_entries: usize,
    inner: Mutex<Inner>,
    /// Serializes disk flushes so a later flush cannot be overwritten by an
    /// earlier one racing to `save_atomically`. Each flush snapshots the
    /// current map under `inner` *after* taking this lock, so the last writer
    /// writes the newest snapshot — no regress.
    flush_lock: Mutex<()>,
}

impl PersistentCardStore {
    /// Derive the owner-scoped path under the app data dir.
    pub fn path_for_owner(app_data_dir: &Path, owner_id_hex: &str) -> PathBuf {
        app_data_dir.join(format!("profile_cards.{owner_id_hex}.cbor"))
    }

    /// Load (or start empty) the store for the local owner. A missing file is
    /// an empty store; a corrupt/unreadable file logs a warning and starts
    /// empty — this is a self-healing cache, not authoritative state (it
    /// refills from live broadcasts), so it must never fail node start.
    pub fn load_for_owner(app_data_dir: &Path, owner_id_hex: &str) -> Self {
        let path = Self::path_for_owner(app_data_dir, owner_id_hex);
        let cards = match load_cards(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "ZEB-839: profile-card store unreadable; starting empty (will refill from broadcasts)"
                );
                Vec::new()
            }
        };
        Self::from_cards(path, DEFAULT_MAX_ENTRIES, cards)
    }

    fn from_cards(path: PathBuf, max_entries: usize, cards: Vec<PersistedCard>) -> Self {
        let mut map = HashMap::with_capacity(cards.len());
        let mut next_seq = 0u64;
        for card in cards {
            map.insert(
                card.owner_id,
                StoredEntry {
                    card,
                    seq: next_seq,
                },
            );
            next_seq += 1;
        }
        Self {
            path,
            max_entries,
            inner: Mutex::new(Inner { map, next_seq }),
            flush_lock: Mutex::new(()),
        }
    }

    /// Upsert a card with newer-HLC-wins. Returns `true` iff the in-memory map
    /// changed (i.e. a flush is worth scheduling). Fast + synchronous — safe to
    /// call from the async cache hot path; the disk flush is separate.
    pub fn upsert(&self, card: &PersistedCard) -> bool {
        let mut inner = self.inner.lock().expect("card store poisoned");
        if let Some(existing) = inner.map.get(&card.owner_id) {
            if !card
                .shared_at
                .is_strictly_newer_than(&existing.card.shared_at)
            {
                return false;
            }
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.map.insert(
            card.owner_id,
            StoredEntry {
                card: card.clone(),
                seq,
            },
        );
        // Enforce the soft cap: evict the least-recently-updated entry. The
        // just-inserted entry has the highest seq, so it is never the victim.
        if inner.map.len() > self.max_entries {
            if let Some(victim) = inner.map.iter().min_by_key(|(_, e)| e.seq).map(|(k, _)| *k) {
                inner.map.remove(&victim);
            }
        }
        true
    }

    /// Last-known card for an owner, if any.
    pub fn get(&self, owner_id: &[u8; 16]) -> Option<PersistedCard> {
        let inner = self.inner.lock().expect("card store poisoned");
        inner.map.get(owner_id).map(|e| e.card.clone())
    }

    /// `owner_id` → last-known display name, for bulk roster / network-health
    /// enrichment fallback (mirrors the live cache's `display_names_by_owner`).
    pub fn display_names_by_owner(&self) -> HashMap<[u8; 16], String> {
        let inner = self.inner.lock().expect("card store poisoned");
        inner
            .map
            .iter()
            .map(|(owner, e)| (*owner, e.card.display_name.clone()))
            .collect()
    }

    /// Number of retained owners.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("card store poisoned").map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Flush the current snapshot to disk atomically. Serialized via
    /// `flush_lock` so concurrent flushes cannot regress the file. Synchronous
    /// I/O — call from a blocking context (`spawn_blocking`), never inline on
    /// the async executor.
    pub fn persist(&self) -> Result<(), PersistError> {
        let _flush = self.flush_lock.lock().expect("card store flush poisoned");
        let bytes = {
            let inner = self.inner.lock().expect("card store poisoned");
            encode_cards(inner.map.values().map(|e| (&e.card, e.seq)))?
        };
        save_atomically(&self.path, &bytes)
    }
}

/// Encode an iterator of (card, seq) to the on-disk byte form (schema byte +
/// CBOR). The snapshot is written in ascending `seq` order so the
/// least-recently-updated → most-recently-updated ordering survives a
/// persist→reload cycle: `from_cards` reassigns `seq` from the loaded Vec's
/// position, so an arbitrary `HashMap` iteration order here would scramble the
/// eviction recency across restarts (the §4.6 invariant). `seq` itself is not
/// persisted — only the order it implies.
fn encode_cards<'a>(
    entries: impl Iterator<Item = (&'a PersistedCard, u64)>,
) -> Result<Vec<u8>, PersistError> {
    let mut ordered: Vec<(&PersistedCard, u64)> = entries.collect();
    ordered.sort_by_key(|(_, seq)| *seq);
    let snapshot: Vec<&PersistedCard> = ordered.into_iter().map(|(c, _)| c).collect();
    let mut bytes = vec![CARD_STORE_SCHEMA_V1];
    into_writer(&snapshot, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    Ok(bytes)
}

/// Load + decode the cards file. Missing file → empty vec. Version/CBOR errors
/// surface as `PersistError` (the caller treats them as "start empty").
fn load_cards(path: &Path) -> Result<Vec<PersistedCard>, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        CARD_STORE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let cards: Vec<PersistedCard> =
                from_reader(&mut cursor).map_err(|e| PersistError::CborDecode(e.to_string()))?;
            // Reject trailing bytes — defensive against truncation edge cases
            // that decode "successfully" but stop short.
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(cards)
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "dev".into(),
        }
    }

    fn card(owner: u8, name: &str, at: u64) -> PersistedCard {
        PersistedCard {
            owner_id: [owner; 16],
            display_name: name.into(),
            status_text: String::new(),
            avatar_cid: None,
            profile_page_root: None,
            shared_at: hlc(at),
        }
    }

    fn empty_store(dir: &Path) -> PersistentCardStore {
        PersistentCardStore::from_cards(
            dir.join("profile_cards.abc.cbor"),
            DEFAULT_MAX_ENTRIES,
            vec![],
        )
    }

    #[test]
    fn path_is_owner_scoped() {
        let base = Path::new("/data");
        let a = PersistentCardStore::path_for_owner(base, "aaaa");
        let b = PersistentCardStore::path_for_owner(base, "bbbb");
        assert_ne!(
            a, b,
            "different owners must map to different files (ZEB-586 isolation)"
        );
        assert!(a.to_string_lossy().contains("aaaa"));
    }

    #[test]
    fn upsert_newer_wins_older_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = empty_store(dir.path());
        assert!(store.upsert(&card(1, "old", 100)), "first insert changes");
        assert!(store.upsert(&card(1, "new", 200)), "strictly-newer changes");
        assert_eq!(store.get(&[1; 16]).unwrap().display_name, "new");
        // Older / equal HLC is ignored (replay-safe).
        assert!(!store.upsert(&card(1, "stale", 150)), "older is ignored");
        assert!(!store.upsert(&card(1, "same", 200)), "equal HLC is ignored");
        assert_eq!(store.get(&[1; 16]).unwrap().display_name, "new");
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = empty_store(dir.path());
        assert!(store.get(&[9; 16]).is_none());
    }

    #[test]
    fn persist_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile_cards.owner.cbor");
        let store = PersistentCardStore::from_cards(path.clone(), DEFAULT_MAX_ENTRIES, vec![]);
        let mut c = card(7, "Alice", 42);
        c.status_text = "hacking".into();
        c.avatar_cid = Some([0xab; 32]);
        store.upsert(&c);
        store.upsert(&card(8, "Bob", 43));
        store.persist().unwrap();

        // Reload from disk via the production load path.
        let loaded = PersistentCardStore::load_for_owner(dir.path(), "owner");
        assert_eq!(loaded.len(), 2);
        let a = loaded.get(&[7; 16]).unwrap();
        assert_eq!(a.display_name, "Alice");
        assert_eq!(a.status_text, "hacking");
        assert_eq!(a.avatar_cid, Some([0xab; 32]));
        assert_eq!(loaded.get(&[8; 16]).unwrap().display_name, "Bob");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = PersistentCardStore::load_for_owner(dir.path(), "never-written");
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_corrupt_or_unknown_schema_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.cbor");
        // Unknown schema byte.
        std::fs::write(&path, [0xFF, 0x00]).unwrap();
        assert!(matches!(
            load_cards(&path),
            Err(PersistError::UnknownSchemaVersion(0xFF))
        ));
        // Valid schema byte, junk CBOR.
        std::fs::write(&path, [CARD_STORE_SCHEMA_V1, 0xA1, 0x66]).unwrap();
        assert!(matches!(
            load_cards(&path),
            Err(PersistError::CborDecode(_) | PersistError::Corrupt)
        ));
        // Empty file.
        std::fs::write(&path, []).unwrap();
        assert!(matches!(load_cards(&path), Err(PersistError::Corrupt)));
    }

    #[test]
    fn corrupt_file_loads_empty_rather_than_failing() {
        // load_for_owner must self-heal: a corrupt file starts an empty store,
        // never an error (it refills from live broadcasts).
        let dir = tempfile::tempdir().unwrap();
        let path = PersistentCardStore::path_for_owner(dir.path(), "owner");
        std::fs::write(&path, [CARD_STORE_SCHEMA_V1, 0xDE, 0xAD]).unwrap();
        let loaded = PersistentCardStore::load_for_owner(dir.path(), "owner");
        assert!(loaded.is_empty());
    }

    #[test]
    fn cap_evicts_least_recently_updated() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentCardStore::from_cards(dir.path().join("x.cbor"), 3, vec![]);
        store.upsert(&card(1, "a", 10)); // seq 0
        store.upsert(&card(2, "b", 10)); // seq 1
        store.upsert(&card(3, "c", 10)); // seq 2
        assert_eq!(store.len(), 3);
        // Refresh owner 1 so it is no longer the oldest by update order.
        store.upsert(&card(1, "a2", 20)); // seq 3
                                          // Insert a 4th distinct owner → evicts the least-recently-updated (owner 2).
        store.upsert(&card(4, "d", 10)); // seq 4
        assert_eq!(store.len(), 3);
        assert!(
            store.get(&[2; 16]).is_none(),
            "owner 2 (oldest update) evicted"
        );
        assert!(
            store.get(&[1; 16]).is_some(),
            "recently-refreshed owner 1 kept"
        );
        assert!(store.get(&[4; 16]).is_some(), "newest owner 4 kept");
    }

    #[test]
    fn persist_writes_cards_in_ascending_update_order() {
        // Deterministic regression for the seq-ordered encode (CodeRabbit): the
        // on-disk order must be ascending by update recency, NOT arbitrary
        // HashMap order — otherwise `from_cards` reassigns seq on reload from a
        // scrambled order and the eviction invariant is lost across restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile_cards.owner.cbor");
        let store = PersistentCardStore::from_cards(path.clone(), 10, vec![]);
        store.upsert(&card(1, "a", 10)); // seq 0
        store.upsert(&card(2, "b", 10)); // seq 1
        store.upsert(&card(3, "c", 10)); // seq 2
        store.upsert(&card(1, "a2", 20)); // seq 3 — owner 1 moves to most-recent
        store.persist().unwrap();
        // File order must be [owner2 (seq1), owner3 (seq2), owner1 (seq3)].
        let on_disk = load_cards(&path).unwrap();
        let order: Vec<u8> = on_disk.iter().map(|c| c.owner_id[0]).collect();
        assert_eq!(
            order,
            vec![2, 3, 1],
            "cards persisted in ascending-seq order"
        );
    }

    #[test]
    fn eviction_order_survives_persist_reload() {
        // The §4.6 least-recently-updated eviction must hold across a
        // persist→reload cycle: after reload, inserting past the cap must evict
        // the same victim the pre-persist store would have.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile_cards.owner.cbor");
        let store = PersistentCardStore::from_cards(path.clone(), 3, vec![]);
        store.upsert(&card(1, "a", 10)); // seq 0
        store.upsert(&card(2, "b", 10)); // seq 1
        store.upsert(&card(3, "c", 10)); // seq 2
        store.upsert(&card(1, "a2", 20)); // seq 3 — owner 1 refreshed (most recent)
        store.persist().unwrap();

        let reloaded = PersistentCardStore::from_cards(path.clone(), 3, load_cards(&path).unwrap());
        assert_eq!(reloaded.len(), 3);
        reloaded.upsert(&card(4, "d", 10)); // evicts least-recently-updated
        assert_eq!(reloaded.len(), 3);
        assert!(
            reloaded.get(&[2; 16]).is_none(),
            "owner 2 (oldest update) evicted after reload"
        );
        assert!(
            reloaded.get(&[1; 16]).is_some(),
            "recently-refreshed owner 1 survives reload+evict"
        );
        assert!(reloaded.get(&[4; 16]).is_some());
    }

    #[test]
    fn display_names_by_owner_maps_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = empty_store(dir.path());
        store.upsert(&card(1, "Alice", 1));
        store.upsert(&card(2, "Bob", 1));
        let names = store.display_names_by_owner();
        assert_eq!(names.get(&[1; 16]).map(String::as_str), Some("Alice"));
        assert_eq!(names.get(&[2; 16]).map(String::as_str), Some("Bob"));
    }

    #[test]
    fn to_discovered_hex_encodes_bytes() {
        let mut c = card(0x11, "Nina", 5);
        c.avatar_cid = Some([0x22; 32]);
        let d = c.to_discovered();
        assert_eq!(d.owner_id_hex, hex::encode([0x11; 16]));
        assert_eq!(d.display_name, "Nina");
        assert_eq!(d.avatar_cid, Some(hex::encode([0x22; 32])));
        assert!(d.profile_page_root.is_none());
    }
}
