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
            collect_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

#[test]
fn no_tauri_command_uses_snake_case_rename() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files under {}", src.display());

    let needle = forbidden();
    let mut offenders = Vec::new();
    for file in &files {
        let count = fs::read_to_string(file)
            .expect("read source")
            .matches(&needle)
            .count();
        if count > 0 {
            let rel = file.strip_prefix(&src).unwrap_or(file);
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
