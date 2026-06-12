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
    std::fs::write(&path, token).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
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
