//! ZEB-975 regression guard: `get_backup_staleness` must resolve the
//! IDENTITY dir (`~/.harmony[/profiles/<p>]`) — the directory every writer
//! of `owner_state_crdt.cbor` / `last_backup.json` uses — never Tauri's
//! app-data dir.
//!
//! Before the fix, the command read both files from `app_data_dir()`, a
//! directory nothing ever writes them to, so the staleness banner always
//! evaluated an empty `OwnerState` with no backup record and could never
//! fire. The behavioral half of the regression is covered in
//! `recovery_cli::tests::staleness_from_dir_sees_real_export_and_engine_writes`
//! (reader-follows-writer through the real export path); this source scan
//! pins the remaining unwireable link — the Tauri command's directory
//! resolution — because an `AppHandle` cannot be constructed in tests.

use std::fs;
use std::path::Path;

#[test]
fn get_backup_staleness_resolves_identity_dir_not_app_data() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = fs::read_to_string(root.join("src").join("lib.rs")).expect("read src/lib.rs");

    // Scope the scan to the command body: from the fn signature to the next
    // fn declaration. (The doc comment ABOVE the signature is deliberately
    // excluded — prose may describe history; the code may not regress.)
    let start = src
        .find("async fn get_backup_staleness(")
        .expect("get_backup_staleness command not found in src/lib.rs");
    let end = src[start..]
        .find("\nasync fn ")
        .map(|i| start + i)
        .expect("no fn declaration after get_backup_staleness");
    let body = &src[start..end];

    assert!(
        body.contains("resolve_identity_dir"),
        "get_backup_staleness must resolve paths via resolve_identity_dir() — \
         the writers' directory (ZEB-975)"
    );
    assert!(
        !body.contains("app_data_dir"),
        "get_backup_staleness must NOT touch the app-data dir: no writer of \
         owner_state_crdt.cbor / last_backup.json uses it, so reading it \
         makes the staleness banner dead code (ZEB-975)"
    );
}
