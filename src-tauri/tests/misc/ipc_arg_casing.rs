//! Regression guard for ZEB-414: Tauri command argument casing.
//!
//! The frontend (`src/lib/*.ts`) invokes every command with **camelCase**
//! argument keys (the CLAUDE.md IPC convention; Tauri's default `#[tauri::command]`
//! maps camelCase JS args to snake_case Rust params). Overriding a command with
//! `rename_all = "snake_case"` makes it require snake_case keys from JS instead,
//! so any such command with a *multi-word* argument silently rejects the
//! frontend's camelCase call with `missing required key …`. That broke Accept /
//! Decline / Unfriend / referral-toggle and network-health export from the UI.
//!
//! This test fails if any source file re-introduces the override. Commands must
//! use plain `#[tauri::command]` (camelCase args). Verified safe: a sweep of the
//! frontend found zero snake_case invoke arguments.

use std::fs;
use std::path::{Path, PathBuf};

/// The forbidden attribute spelling. Assembled at runtime so this test file —
/// which necessarily names the pattern — never matches itself were it scanned.
fn forbidden() -> String {
    format!("tauri::command(rename_all = {:?})", "snake_case")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            // Skip Cargo's build directory — it holds generated/vendored `.rs`
            // we don't own; scanning it is slow and false-positive-prone.
            if path.file_name().map(|n| n == "target").unwrap_or(false) {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

#[test]
fn no_tauri_command_uses_snake_case_rename() {
    // Scan the entire crate, not just `src/` — commands live in `src/lib.rs`
    // today, but covering `tests/`, `build.rs`, and any future module dir means
    // a command defined outside `src/` can't silently slip past this guard.
    // `target/` is skipped in `collect_rs_files`. This file is self-safe: it
    // assembles the needle via `forbidden()` so it never matches its own text.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    assert!(!files.is_empty(), "no .rs files under {}", root.display());

    let needle = forbidden();
    let mut offenders = Vec::new();
    for file in &files {
        let count = fs::read_to_string(file)
            .expect("read source")
            .matches(&needle)
            .count();
        if count > 0 {
            let rel = file.strip_prefix(root).unwrap_or(file);
            offenders.push(format!("  {} ({count})", rel.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "Tauri commands must take camelCase args via plain `#[tauri::command]`, not \
         `rename_all = \"snake_case\"` (the frontend sends camelCase — ZEB-414). \
         Offending files:\n{}",
        offenders.join("\n"),
    );
}

/// ZEB-977 (was ZEB-419's `set_friend_nickname` pin): `set_contact_petname` /
/// `set_contact_notes` must use plain `#[tauri::command]`s so their
/// `owner_id_hex` / `petname` / `notes` Rust params map from the frontend's
/// camelCase `ownerIdHex` / `petname` / `notes` keys. A `rename_all =
/// "snake_case"` override would make the IPC reject the editors' calls with
/// `missing required key ownerIdHex`. The general scan above already forbids
/// the override crate-wide; this pins the specific contact commands + their
/// arg shapes so a refactor can't silently regress the casing contract.
#[test]
fn contact_setters_take_camelcase_args_via_plain_commands() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cmds_rs = root.join("src").join("contacts_commands.rs");
    let src = fs::read_to_string(&cmds_rs).expect("read src/contacts_commands.rs");

    // The exact signature lines we ship. Their presence proves: (a) the fns
    // exist, (b) they declare snake_case Rust params (Tauri maps the JS
    // camelCase keys onto these), and — paired with the crate-wide
    // `no_tauri_command_uses_snake_case_rename` scan — that they carry NO
    // `rename_all` override, so the JS keys stay camelCase.
    assert!(
        src.contains("pub async fn set_contact_petname(")
            && src.contains("pub async fn set_contact_notes("),
        "contact setter IPCs not found in src/contacts_commands.rs",
    );
    assert!(
        src.contains("owner_id_hex: String,")
            && src.contains("petname: Option<String>,")
            && src.contains("notes: Option<String>,"),
        "contact setters must declare snake_case Rust params `owner_id_hex` + \
         `petname`/`notes` so the frontend's camelCase keys map through Tauri \
         (ZEB-414 / ZEB-977)",
    );
}

#[test]
fn emoji_name_ipcs_use_plain_commands_with_camelcase_args() {
    let src = std::fs::read_to_string("src/lib.rs").expect("read src/lib.rs");
    assert!(
        src.contains("async fn set_emoji_name("),
        "set_emoji_name IPC not found in src/lib.rs",
    );
    assert!(
        src.contains("cid: String,")
            && src.contains("name: Option<String>,")
            && src.contains("mime: String,")
            && src.contains("size: u64,"),
        "set_emoji_name must declare cid/name/mime/size params",
    );
    assert!(
        src.contains("async fn list_emoji_names("),
        "list_emoji_names IPC not found",
    );
    assert!(
        src.contains("async fn preview_named_emoji("),
        "preview_named_emoji IPC not found",
    );
}
