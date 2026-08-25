//! Durable atomic file replacement (ZEB-548 Stage 1).
//!
//! A broadly-shared persistence primitive — 9 call sites spanning the
//! owner-fleet, community, identity-crypto, and app tiers — lifted out of
//! `harmony-app`'s `owner_state_persist` into this foundation leaf so every
//! tier depends on it *downward* instead of reaching sideways into a peer
//! module. Every error it *returns* is an I/O error, so callers wanting a
//! richer error convert at the boundary (e.g. `PersistError:
//! From<std::io::Error>` covers the `?` sites unchanged). A `path` with no
//! parent directory is a caller-precondition panic (see `# Panics`), not a
//! returned error.

#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Atomically replace `path` with `bytes`. Writes to a sibling
/// tempfile, fsyncs, renames into place, then (on Unix) fsyncs the
/// directory entry so the rename itself is durable.
///
/// The directory fsync is Unix-only: `File::open(dir)` fails on
/// Windows with `ERROR_ACCESS_DENIED` because Win32 does not expose a
/// regular file handle for directories. Windows' `MoveFileEx`/
/// `ReplaceFile` (used by `tempfile::NamedTempFile::persist`) is
/// already atomic for in-volume renames on NTFS, and NTFS journals
/// the directory update along with the file rename, so dropping the
/// dir fsync on Windows preserves the same crash semantics.
///
/// # Panics
/// Panics if `path` has no parent directory (e.g. a bare root). Every caller
/// passes a file path under a data dir, so this is unreachable in practice.
pub fn save_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("save_atomically: path has no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(std::io::Error::other)?;
    #[cfg(unix)]
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
