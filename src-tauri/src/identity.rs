//! Node identity management — Ed25519 + post-quantum key generation and persistence.
//!
//! Adapted from harmony-node's identity_file.rs. Same binary format so keys
//! are portable between standalone daemon and embedded Tauri client.

use std::path::{Path, PathBuf};

use harmony_identity::{PqPrivateIdentity, PrivateIdentity};
use zeroize::Zeroizing;

const VERSION: u8 = 0x01;
const PQ_KEY_LEN: usize = 96;
const ED25519_KEY_LEN: usize = 64;
const FILE_LEN: usize = 1 + PQ_KEY_LEN + ED25519_KEY_LEN; // 161

pub struct NodeIdentity {
    pub pq: PqPrivateIdentity,
    pub ed25519: PrivateIdentity,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("pq_address", &self.pq.public_identity().address_hash)
            .field(
                "ed25519_address",
                &self.ed25519.public_identity().address_hash,
            )
            .finish()
    }
}

/// Resolve the identity file path. Uses `~/.harmony/identity.key` by default.
///
/// Checks `$HOME` (Unix/macOS) then `$USERPROFILE` (Windows) for the home
/// directory. An explicit override path bypasses both.
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            "Cannot determine identity file path: neither $HOME nor $USERPROFILE is set"
                .to_string()
        })?;
    Ok(PathBuf::from(home).join(".harmony").join("identity.key"))
}

pub fn load(path: &Path) -> Result<NodeIdentity, String> {
    let buf = Zeroizing::new(
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?,
    );
    if buf.len() != FILE_LEN {
        return Err(format!(
            "Corrupt identity file: expected {FILE_LEN} bytes, got {}",
            buf.len()
        ));
    }
    if buf[0] != VERSION {
        return Err(format!(
            "Unsupported identity file version: {:#04x}",
            buf[0]
        ));
    }
    let pq = PqPrivateIdentity::from_private_bytes(&buf[1..1 + PQ_KEY_LEN])
        .map_err(|e| format!("Corrupt PQ identity in key file: {e}"))?;
    let ed25519 = PrivateIdentity::from_private_bytes(&buf[1 + PQ_KEY_LEN..])
        .map_err(|e| format!("Corrupt Ed25519 identity in key file: {e}"))?;
    #[cfg(unix)]
    warn_permissions(path);
    Ok(NodeIdentity { pq, ed25519 })
}

pub fn save(path: &Path, identity: &NodeIdentity) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let pq_bytes = Zeroizing::new(identity.pq.to_private_bytes());
    let ed_bytes = Zeroizing::new(identity.ed25519.to_private_bytes());
    let mut buf = Zeroizing::new(Vec::with_capacity(FILE_LEN));
    buf.push(VERSION);
    buf.extend_from_slice(&pq_bytes);
    buf.extend_from_slice(ed_bytes.as_slice());

    // Atomic write: tmp file with restricted permissions → fsync → rename.
    let tmp_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        path.with_file_name(name)
    };

    struct TmpGuard<'a>(&'a Path);
    impl Drop for TmpGuard<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let guard = TmpGuard(&tmp_path);

    {
        #[cfg(unix)]
        let f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
        };
        #[cfg(not(unix))]
        let f = {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
        };
        use std::io::Write;
        (&f).write_all(&buf)
            .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("Failed to fsync {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "Failed to rename {} → {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    std::mem::forget(guard);
    Ok(())
}

/// Load identity from file, or generate and save a new one if it doesn't exist.
pub fn load_or_generate(path: &Path) -> Result<NodeIdentity, String> {
    if path.exists() {
        return load(path);
    }
    let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
    let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let identity = NodeIdentity { pq, ed25519 };
    save(path, &identity)?;
    Ok(identity)
}

#[cfg(unix)]
fn warn_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:#05o}"),
                "identity file has open permissions, should be 0600"
            );
        }
    }
}
