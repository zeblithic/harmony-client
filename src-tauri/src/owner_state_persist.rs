//! On-disk persistence for the Phase-2 OwnerState CRDT and the
//! RootReplayTracker (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Persistence layer". Two files written via atomic-rename + fsync,
//! each prefixed with a 1-byte schema version.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use tempfile;

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("file corrupt (truncated or invalid CBOR)")]
    Corrupt,
    #[error("unknown schema version byte: {0:#x}")]
    UnknownSchemaVersion(u8),
}

/// Atomically replace `path` with `bytes`. Writes to a sibling
/// tempfile, fsyncs, renames into place, then fsyncs the directory
/// entry so the rename itself is durable.
pub fn save_atomically(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let dir = path.parent().expect("save_atomically: path has no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| PersistError::Io(std::io::Error::other(e)))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_atomically_creates_file_with_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"hello world").unwrap();
        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, b"hello world");
    }

    #[test]
    fn save_atomically_replaces_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"old").unwrap();
        save_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn dropped_tempfile_does_not_corrupt_existing_file() {
        // Crash-survival: simulate a save that begins (creates a tempfile)
        // but is dropped before persist. The original file must remain
        // intact.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"original").unwrap();

        // Simulate a partial save: create a tempfile, write, but drop
        // without persist (mimics a crash mid-save).
        {
            let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
            tmp.write_all(b"partial junk").unwrap();
            // tmp drops here — tempfile auto-deletes
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }
}
