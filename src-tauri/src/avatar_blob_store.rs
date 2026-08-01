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
//!
//! The blocking `std::fs` calls here are meant to run off the async runtime;
//! `fetch_avatar` invokes `get`/`put` inside `tokio::task::spawn_blocking`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use harmony_content::cid::ContentId;

/// Default cap on total avatar-cache bytes. At the 512 KiB per-avatar ceiling
/// (`max_blob_bytes`) this holds ~64 max-size avatars, and in practice many
/// hundreds (most avatars are far smaller). Eviction is oldest-write-first.
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Content-addressed on-disk avatar cache. Cheap to share via `Arc`.
pub struct AvatarBlobStore {
    dir: PathBuf,
    /// Cap on the *total* cache size; `put` prunes down to this.
    max_bytes: u64,
    /// Cap on a *single* blob. Mirrors the network fetch's `AVATAR_MAX_BYTES`
    /// ceiling so a disk hit can't bypass it: `get` rejects an over-cap file
    /// *before* reading it into memory, and `put` refuses to write one.
    max_blob_bytes: u64,
    /// Serializes prune scans so two concurrent `put`s can't both decide to
    /// evict the same files. Reads and writes of distinct CID files need no
    /// lock — the filesystem is the source of truth.
    prune_lock: Mutex<()>,
}

impl AvatarBlobStore {
    /// Open (creating the directory) an avatar cache under `dir` with a total
    /// byte budget and a per-blob ceiling. Never fails: a dir-creation error is
    /// logged and the store still returns — every `get` then misses and every
    /// `put` no-ops, so the caller silently falls back to network fetches.
    ///
    /// Also sweeps any stale `*.tmp` left by a crash mid-write (they are never
    /// counted toward the budget nor evicted, so without this they would leak).
    pub fn load(dir: &Path, max_bytes: u64, max_blob_bytes: u64) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "avatar cache: create_dir_all failed; cache effectively disabled"
            );
        }
        sweep_stale_temp_files(dir);
        Self {
            dir: dir.to_path_buf(),
            max_bytes,
            max_blob_bytes,
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
    /// Returns `None` on a miss, a malformed CID, an I/O error, an
    /// **over-`max_blob_bytes`** file, or bytes that fail verification — in the
    /// last two cases the offending file is removed so the next fetch
    /// re-populates it (self-heal). The size check happens **before** reading,
    /// so a corrupt/oversized on-disk blob can't force a large allocation or
    /// bypass the ceiling the network path enforces. A `Some` result is
    /// guaranteed to be exactly the content the CID names.
    pub fn get(&self, cid_hex: &str) -> Option<Vec<u8>> {
        let cid = Self::parse_cid(cid_hex)?;
        let path = self.blob_path(cid_hex);
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() > self.max_blob_bytes {
            tracing::warn!(
                cid = %cid_hex,
                size = meta.len(),
                cap = self.max_blob_bytes,
                "avatar cache: on-disk blob exceeds per-blob cap; removing (self-heal)"
            );
            let _ = std::fs::remove_file(&path);
            return None;
        }
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
    /// Rejects a malformed CID, bytes over `max_blob_bytes`, or bytes that fail
    /// `hash == cid`. The caller ([`crate::fetch_avatar`]) already returns
    /// size-capped, verified bytes, but re-checking keeps the store's on-disk
    /// invariant local and total — a `get` can then trust any file it finds. A
    /// rejection or write error is logged and swallowed.
    pub fn put(&self, cid_hex: &str, bytes: &[u8]) {
        let Some(cid) = Self::parse_cid(cid_hex) else {
            tracing::warn!(cid = %cid_hex, "avatar cache: refusing put for malformed CID");
            return;
        };
        if bytes.len() as u64 > self.max_blob_bytes {
            tracing::warn!(
                cid = %cid_hex,
                size = bytes.len(),
                cap = self.max_blob_bytes,
                "avatar cache: refusing put; blob exceeds per-blob cap"
            );
            return;
        }
        if !cid.verify_hash(bytes) {
            tracing::warn!(cid = %cid_hex, "avatar cache: refusing put; bytes fail hash==cid");
            return;
        }
        // Durable + concurrency-safe write via the repo's atomic-save helper:
        // fsyncs the file and the parent directory (so a committed blob survives
        // a power loss) and uses a uniquely-named temp so concurrent same-CID
        // writes never collide (`tempfile::NamedTempFile`).
        if let Err(e) = crate::owner_state_persist::save_atomically(&self.blob_path(cid_hex), bytes)
        {
            tracing::warn!(cid = %cid_hex, error = %e, "avatar cache: write failed");
            return;
        }
        self.prune();
    }

    /// Evict oldest-written blobs until total size is within `max_bytes`.
    ///
    /// Recomputes the total by scanning the directory each call (the cache is
    /// small — hundreds of files at most). Eviction keys on filesystem mtime, so
    /// recency survives restart with no separate index. Since avatar content is
    /// immutable, a wrong eviction only costs a cheap network re-fetch —
    /// write-time ordering is an adequate proxy for a true access-ordered LRU
    /// without a metadata write on every read.
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
            // Count only committed blobs; skip in-flight `*.tmp` writes.
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

/// Delete any leftover temp files under `dir` — anything that is not a committed
/// `{cid}.bin` blob. `save_atomically`'s `NamedTempFile` removes its own temp on
/// clean drop, but a hard crash (SIGKILL / power loss) between create and
/// `persist` can orphan one (named `.tmp…`, so it is never counted toward the
/// budget nor evicted). This cache dir holds only `*.bin` blobs, so removing
/// every non-`.bin` file at load is a safe, naming-agnostic sweep.
fn sweep_stale_temp_files(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let is_blob = path.extension().and_then(|e| e.to_str()) == Some("bin");
        if !is_blob && path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_content::cid::ContentFlags;
    use std::time::{Duration, SystemTime};

    // Generous per-blob cap for tests not exercising the size limit.
    const BIG_BLOB_CAP: u64 = 1024 * 1024;

    fn store(dir: &Path, max_bytes: u64) -> AvatarBlobStore {
        AvatarBlobStore::load(dir, max_bytes, BIG_BLOB_CAP)
    }

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
        let s = store(dir.path(), DEFAULT_MAX_BYTES);
        let data = b"avatar-bytes-here".to_vec();
        let cid = cid_hex_for(&data);

        s.put(&cid, &data);
        assert_eq!(s.get(&cid).as_deref(), Some(data.as_slice()));
        // After a successful write only the committed `.bin` remains — no temp.
        let non_blob = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().and_then(|x| x.to_str()) != Some("bin"));
        assert!(!non_blob, "only the committed .bin should remain after put");
    }

    #[test]
    fn get_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path(), DEFAULT_MAX_BYTES);
        let cid = cid_hex_for(b"never-stored");
        assert!(s.get(&cid).is_none());
        // Malformed CIDs are clean misses, not panics.
        assert!(s.get("not-hex").is_none());
        assert!(s.get("ab").is_none()); // valid hex, wrong length
    }

    #[test]
    fn get_rejects_and_removes_tampered_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path(), DEFAULT_MAX_BYTES);
        // CID names "real" content, but the file on disk holds different bytes.
        let cid = cid_hex_for(b"the-real-avatar");
        let path = dir.path().join(format!("{cid}.bin"));
        std::fs::write(&path, b"TAMPERED-DIFFERENT-BYTES").unwrap();

        assert!(s.get(&cid).is_none(), "tampered bytes must not verify");
        assert!(!path.exists(), "self-heal must remove the bad file");
    }

    #[test]
    fn get_rejects_and_removes_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny per-blob cap; the on-disk file blows past it.
        let s = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES, 64);
        let cid = cid_hex_for(b"whatever");
        let path = dir.path().join(format!("{cid}.bin"));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        assert!(s.get(&cid).is_none(), "over-cap file must not be served");
        assert!(!path.exists(), "self-heal must remove the oversized file");
    }

    #[test]
    fn put_refuses_oversized_blob() {
        let dir = tempfile::tempdir().unwrap();
        let s = AvatarBlobStore::load(dir.path(), DEFAULT_MAX_BYTES, 64);
        let data = vec![7u8; 4096];
        let cid = cid_hex_for(&data);

        s.put(&cid, &data);
        assert!(s.get(&cid).is_none(), "over-cap blob must not be written");
        assert!(!dir.path().join(format!("{cid}.bin")).exists());
    }

    #[test]
    fn eviction_drops_lru_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        // Budget holds two 1000-byte blobs but not three.
        let s = store(dir.path(), 2500);

        let a = vec![0u8; 1000];
        let b = vec![1u8; 1000];
        let c = vec![2u8; 1000];
        let (ca, cb, cc) = (cid_hex_for(&a), cid_hex_for(&b), cid_hex_for(&c));

        let now = SystemTime::now();
        s.put(&ca, &a);
        set_mtime(
            &dir.path().join(format!("{ca}.bin")),
            now - Duration::from_secs(100),
        );
        s.put(&cb, &b);
        set_mtime(
            &dir.path().join(format!("{cb}.bin")),
            now - Duration::from_secs(50),
        );

        // Putting the third blob pushes total to 3000 > 2500; prune must evict
        // the oldest (A), leaving the two most recent.
        s.put(&cc, &c);

        assert!(s.get(&ca).is_none(), "oldest blob A should be evicted");
        assert_eq!(s.get(&cb).as_deref(), Some(b.as_slice()), "B retained");
        assert_eq!(s.get(&cc).as_deref(), Some(c.as_slice()), "C retained");
    }

    #[test]
    fn fresh_instance_reads_existing_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"survives-restart".to_vec();
        let cid = cid_hex_for(&data);

        {
            let s = store(dir.path(), DEFAULT_MAX_BYTES);
            s.put(&cid, &data);
        } // drop — simulate process exit

        // A fresh store over the same dir serves the previously-written blob.
        let reopened = store(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(reopened.get(&cid).as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn load_sweeps_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        // A committed blob plus orphaned temps from a crash mid-write, in both
        // the `.tmp`-extension shape and `tempfile::NamedTempFile`'s leading-dot
        // shape (`.tmp<random>`). The sweep is naming-agnostic: anything not a
        // `.bin` blob goes.
        let data = b"kept".to_vec();
        let cid = cid_hex_for(&data);
        std::fs::write(dir.path().join(format!("{cid}.bin")), &data).unwrap();
        let orphan_ext = dir.path().join("deadbeef.12345.7.tmp");
        let orphan_tempfile = dir.path().join(".tmpA1b2C3");
        std::fs::write(&orphan_ext, b"partial").unwrap();
        std::fs::write(&orphan_tempfile, b"partial").unwrap();

        let reopened = store(dir.path(), DEFAULT_MAX_BYTES);

        assert!(!orphan_ext.exists(), "load must sweep .tmp orphans");
        assert!(
            !orphan_tempfile.exists(),
            "load must sweep NamedTempFile-style orphans"
        );
        assert_eq!(
            reopened.get(&cid).as_deref(),
            Some(data.as_slice()),
            "committed blob is untouched by the sweep"
        );
    }
}
