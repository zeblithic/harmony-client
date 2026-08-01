//! ZEB-841: durable, content-addressed on-disk cache for avatar image bytes.
//!
//! ZEB-839 persists a peer's `avatar_cid` to disk so their **name** renders
//! offline, but the image **bytes** behind that CID lived only in the RAM
//! `MemoryBookStore` runtime cache — lost on restart, so a peer who is offline
//! at boot rendered by name with a blank avatar. This module is the durable
//! *payload* behind that durable *pointer*: a small
//! `{app_data_dir}/avatars/{cid}.bin` blob cache, written through when
//! [`crate::fetch_avatar`] fetches over the network and read back first on
//! subsequent fetches. Because a CID is an immutable content-address, a disk
//! hit is always exactly the requested content — so reads are offline-capable
//! and have no staleness window.
//!
//! It mirrors the `mail.rs` blob-store pattern and lives under the per-identity
//! data dir, so it is wiped on identity reset alongside `mail/` (preserving the
//! ZEB-586 per-identity-isolation invariant). The cache is a pure optimization:
//! every method is best-effort and never surfaces an error to the caller — a
//! failure just degrades to a network re-fetch.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use harmony_content::cid::ContentId;

/// Default cap on total avatar-cache bytes. At the `AVATAR_MAX_BYTES` = 512 KiB
/// per-avatar ceiling this holds ~64 max-size avatars, and in practice many
/// hundreds (most avatars are far smaller). Eviction is oldest-write-first.
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Content-addressed on-disk avatar cache. Cheap to share via `Arc`.
pub struct AvatarBlobStore {
    dir: PathBuf,
    max_bytes: u64,
    /// Serializes prune scans so two concurrent `put`s can't both decide to
    /// evict the same files. Reads and writes of distinct CID files need no
    /// lock — the filesystem is the source of truth.
    prune_lock: Mutex<()>,
}

impl AvatarBlobStore {
    /// Open (creating the directory) an avatar cache under `dir` with the given
    /// byte budget. Never fails: a dir-creation error is logged and the store
    /// still returns — every `get` then misses and every `put` no-ops, so the
    /// caller silently falls back to network fetches.
    pub fn load(dir: &Path, max_bytes: u64) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "avatar cache: create_dir_all failed; cache effectively disabled"
            );
        }
        Self {
            dir: dir.to_path_buf(),
            max_bytes,
            prune_lock: Mutex::new(()),
        }
    }

    fn blob_path(&self, cid_hex: &str) -> PathBuf {
        self.dir.join(format!("{cid_hex}.bin"))
    }

    /// Parse a 64-char hex CID into a [`ContentId`], or `None` if malformed.
    ///
    /// This runs before any path is built, so a malformed CID can never reach
    /// [`Self::blob_path`] — valid hex cannot contain `/` or `..`, which is the
    /// path-traversal guard.
    fn parse_cid(cid_hex: &str) -> Option<ContentId> {
        let bytes = hex::decode(cid_hex).ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        Some(ContentId::from_bytes(arr))
    }

    /// Read cached avatar bytes for `cid_hex`, verifying `hash == cid`.
    ///
    /// Returns `None` on a miss, a malformed CID, an I/O error, or bytes that
    /// fail verification — in the last case the corrupt/tampered file is removed
    /// so the next fetch re-populates it (self-heal). A `Some` result is
    /// guaranteed to be exactly the content the CID names.
    pub fn get(&self, cid_hex: &str) -> Option<Vec<u8>> {
        let cid = Self::parse_cid(cid_hex)?;
        let path = self.blob_path(cid_hex);
        let bytes = std::fs::read(&path).ok()?;
        if cid.verify_hash(&bytes) {
            Some(bytes)
        } else {
            tracing::warn!(
                cid = %cid_hex,
                "avatar cache: on-disk bytes failed hash==cid; removing (self-heal)"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }

    /// Write `bytes` for `cid_hex` (best-effort), then prune to the byte budget.
    ///
    /// Only bytes that pass `hash == cid` are written. The caller
    /// ([`crate::fetch_avatar`]) already returns verified bytes, but re-checking
    /// keeps the store's on-disk invariant local and total — a `get` can then
    /// trust any file it finds. A malformed CID or write error is logged and
    /// swallowed.
    pub fn put(&self, cid_hex: &str, bytes: &[u8]) {
        let Some(cid) = Self::parse_cid(cid_hex) else {
            tracing::warn!(cid = %cid_hex, "avatar cache: refusing put for malformed CID");
            return;
        };
        if !cid.verify_hash(bytes) {
            tracing::warn!(cid = %cid_hex, "avatar cache: refusing put; bytes fail hash==cid");
            return;
        }
        let path = self.blob_path(cid_hex);
        if let Err(e) = atomic_write(&path, bytes) {
            tracing::warn!(cid = %cid_hex, error = %e, "avatar cache: write failed");
            return;
        }
        self.prune();
    }

    /// Evict oldest-written blobs until total size is within `max_bytes`.
    ///
    /// Eviction keys on filesystem mtime, so recency survives restart with no
    /// separate index. Since avatar content is immutable, a wrong eviction only
    /// costs a cheap network re-fetch — write-time ordering is an adequate proxy
    /// for a true access-ordered LRU without a metadata write on every read.
    fn prune(&self) {
        let _g = self
            .prune_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut entries: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        let mut total: u64 = 0;
        for ent in rd.flatten() {
            let path = ent.path();
            // Count only committed blobs; skip in-flight `*.bin.tmp` writes.
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let Ok(md) = ent.metadata() else {
                continue;
            };
            if !md.is_file() {
                continue;
            }
            total += md.len();
            let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
            entries.push((path, mtime, md.len()));
        }
        if total <= self.max_bytes {
            return;
        }
        entries.sort_by_key(|(_, mtime, _)| *mtime); // oldest first
        for (path, _, size) in entries {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

/// Atomically write `bytes` to `path` via a sibling temp file + rename, so a
/// crash mid-write never leaves a partially-written `{cid}.bin` that a later
/// `get` would read and then reject.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_content::cid::ContentFlags;
    use std::time::{Duration, SystemTime};

    /// Mint the real CID for `data` (PublicDurable, non-inline) — the same
    /// shape `verify_hash` checks against.
    fn cid_hex_for(data: &[u8]) -> String {
        let cid = ContentId::for_book(data, ContentFlags::default()).expect("for_book");
        hex::encode(cid.to_bytes())
    }

    /// Force a file's mtime so eviction ordering is deterministic (mtime is the
    /// LRU key). Requires write permission, per `File::set_modified`.
    fn set_mtime(path: &Path, t: SystemTime) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        f.set_modified(t).expect("set_modified");
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES);
        let data = b"avatar-bytes-here".to_vec();
        let cid = cid_hex_for(&data);

        store.put(&cid, &data);
        assert_eq!(store.get(&cid).as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn get_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES);
        // Well-formed CID, nothing on disk.
        let cid = cid_hex_for(b"never-stored");
        assert!(store.get(&cid).is_none());
        // Malformed CID is also a clean miss, not a panic.
        assert!(store.get("not-hex").is_none());
        assert!(store.get("ab").is_none()); // valid hex, wrong length
    }

    #[test]
    fn get_rejects_and_removes_tampered_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES);
        // CID names "real" content, but the file on disk holds different bytes.
        let cid = cid_hex_for(b"the-real-avatar");
        let path = dir.path().join(format!("{cid}.bin"));
        std::fs::write(&path, b"TAMPERED-DIFFERENT-BYTES").unwrap();

        assert!(store.get(&cid).is_none(), "tampered bytes must not verify");
        assert!(!path.exists(), "self-heal must remove the bad file");
    }

    #[test]
    fn eviction_drops_lru_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        // Budget holds two 1000-byte blobs but not three.
        let store = AvatarBlobStore::load(dir.path(), 2500);

        let a = vec![0u8; 1000];
        let b = vec![1u8; 1000];
        let c = vec![2u8; 1000];
        let (ca, cb, cc) = (cid_hex_for(&a), cid_hex_for(&b), cid_hex_for(&c));

        let now = SystemTime::now();
        store.put(&ca, &a);
        set_mtime(
            &dir.path().join(format!("{ca}.bin")),
            now - Duration::from_secs(100),
        );
        store.put(&cb, &b);
        set_mtime(
            &dir.path().join(format!("{cb}.bin")),
            now - Duration::from_secs(50),
        );

        // Putting the third blob pushes total to 3000 > 2500; prune must evict
        // the oldest (A), leaving the two most recent.
        store.put(&cc, &c);

        assert!(store.get(&ca).is_none(), "oldest blob A should be evicted");
        assert_eq!(store.get(&cb).as_deref(), Some(b.as_slice()), "B retained");
        assert_eq!(store.get(&cc).as_deref(), Some(c.as_slice()), "C retained");
    }

    #[test]
    fn fresh_instance_reads_existing_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"survives-restart".to_vec();
        let cid = cid_hex_for(&data);

        {
            let store = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES);
            store.put(&cid, &data);
        } // drop — simulate process exit

        // A fresh store over the same dir serves the previously-written blob.
        let reopened = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(reopened.get(&cid).as_deref(), Some(data.as_slice()));
    }
}
