// src-tauri/src/api/auth.rs — ZEB-445 bearer-token auth.
//
// Trust boundary = same user on the same machine (matches the keychain's).
// The token lives in a 0600 file so browser pages (which CAN open localhost
// WebSockets without CORS preflight) cannot obtain it.

use rand::RngCore;
use std::path::Path;

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn write_token_file(dir: &Path, token: &str) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("token");
    // Create with 0600 at open time and tighten any pre-existing file BEFORE
    // the token bytes land — a write-then-chmod sequence would expose the
    // token under permissive umasks for the gap between the two calls.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        // `mode(0o600)` only applies at creation; a stale token file from a
        // previous run keeps its old bits, so tighten explicitly while the
        // file is still empty.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    use std::io::Write;
    f.write_all(token.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    // Windows: user-profile default ACLs already restrict to the owner.
    Ok(path)
}

/// 256-bit random token + local-only trust boundary → plain equality is fine.
pub fn check_bearer(expected: &str, header_value: Option<&str>) -> bool {
    matches!(header_value, Some(h) if h.strip_prefix("Bearer ") == Some(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars_and_random() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(b.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let token = generate_token();
        let path = write_token_file(dir.path(), &token).expect("write token file");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            token,
            "file content must be the token"
        );
    }

    #[test]
    fn bearer_check_accepts_exact_and_rejects_everything_else() {
        let token = "abc123";
        assert!(check_bearer(token, Some("Bearer abc123")));
        assert!(!check_bearer(token, Some("Bearer wrong")));
        assert!(!check_bearer(token, Some("abc123"))); // missing "Bearer " prefix
        assert!(!check_bearer(token, Some("bearer abc123"))); // wrong case
        assert!(!check_bearer(token, Some("Bearer abc1234"))); // superstring
        assert!(!check_bearer(token, Some(""))); // empty header
        assert!(!check_bearer(token, None)); // absent header
    }
}
