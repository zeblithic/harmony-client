// src-tauri/src/api/lock.rs — ZEB-445 one-node-per-profile lock.
//
// fd-lock = OS advisory lock: released automatically on process death, so
// stale-lock reclaim needs no PID-liveness logic. The file CONTENT (pid) is
// purely for the human-readable refusal message.

use fd_lock::RwLock;
use std::io::Write;
use std::path::Path;

pub struct ProfileLock {
    // Held for the process lifetime; dropping releases the OS lock.
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

pub fn acquire(dir: &Path) -> Result<ProfileLock, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("serve.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    // One leaked RwLock per acquire is intentional: the lock is held for the
    // full process lifetime by design, and the OS releases it on process
    // death (that's the stale-reclaim story — no PID-liveness logic needed).
    // Dropping the returned ProfileLock still releases the OS lock (tests).
    let lock: &'static mut RwLock<std::fs::File> = Box::leak(Box::new(RwLock::new(file)));
    let guard = lock.try_write().map_err(|_| read_holder_message(&path))?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("rewrite {}: {e}", path.display()))?;
        let _ = writeln!(f, "{}", std::process::id());
    }
    Ok(ProfileLock { _guard: guard })
}

fn read_holder_message(path: &Path) -> String {
    let holder = std::fs::read_to_string(path).unwrap_or_default();
    format!(
        "profile already in use (lock {}, holder pid {}): another harmony-app \
         (serve or GUI-with-API) owns this profile; stop it first",
        path.display(),
        holder.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = acquire(dir.path()).expect("first acquire succeeds");

        // match instead of expect_err: ProfileLock holds an fd-lock guard and
        // intentionally has no Debug impl.
        let err = match acquire(dir.path()) {
            Ok(_) => panic!("second acquire must fail while first is held"),
            Err(e) => e,
        };
        assert!(
            err.contains("already in use"),
            "refusal message should explain the holder, got: {err}"
        );
        assert!(
            err.contains(&std::process::id().to_string()),
            "refusal message should carry the holder pid, got: {err}"
        );

        drop(first);
        let third = acquire(dir.path());
        assert!(
            third.is_ok(),
            "acquire after drop must succeed, got: {:?}",
            third.err()
        );
    }
}
