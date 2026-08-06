//! ZEB-338 + ZEB-801 + ZEB-872: guard against misleading owner-not-loaded error
//! phrasing creeping back into production sources. The honest messages are the
//! two ZEB-801 constants — OWNER_STILL_STARTING_MSG ("… the app is still
//! starting. Try again in a moment.") and OWNER_NO_IDENTITY_MSG ("… no identity
//! is set up on this device yet.") — reached through
//! NodeState::owner_not_loaded_msg / owner_not_loaded_msg_locked, never the old
//! destructive "recreate identity" text.
//!
//! ZEB-872 broadened the first three scans from `src/lib.rs` only to **all**
//! production Rust under `src/` (the original lib.rs-only scope was inherited
//! from ZEB-338 and let the phrasing survive in `community_fork.rs` /
//! `community_membership.rs`). The scan skips pure comment lines so a doc
//! comment may still *describe* the forbidden phrase (e.g. `owner_loaded.rs`)
//! without tripping the guard.

use std::path::PathBuf;

/// Every `.rs` file under the crate's `src/`, returned as `(display_path, contents)`.
fn production_rs_sources() -> Vec<(String, String)> {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir under src/") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let contents = std::fs::read_to_string(&path).expect("read .rs source");
                out.push((path.display().to_string(), contents));
            }
        }
    }
    assert!(
        !out.is_empty(),
        "no .rs sources found under src/ — the phrasing guard would silently pass"
    );
    out
}

/// `(path, 1-based line)` for every occurrence of `needle` on a **non-comment**
/// line across all production sources. Pure comment lines (trimmed start `//`,
/// which covers `//`, `///`, `//!`) are skipped so documentation may reference
/// the forbidden phrase; trailing comments on code lines are still scanned.
fn code_offenders(needle: &str) -> Vec<String> {
    let mut offenders = Vec::new();
    for (path, src) in production_rs_sources() {
        for (idx, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(needle) {
                offenders.push(format!("{path}:{}", idx + 1));
            }
        }
    }
    offenders
}

#[test]
fn no_misleading_node_not_running_phrasing_in_production_sources() {
    let offenders = code_offenders("node not running?");
    assert!(
        offenders.is_empty(),
        "phrasing regression: {} code site(s) still say 'node not running?' — \
         replace with an honest owner-not-loaded message. Offenders: {offenders:?}",
        offenders.len()
    );
}

#[test]
fn no_misleading_no_owner_identity_phrasing_in_production_sources() {
    // Matches the bare interrogative "no owner identity?" so it catches both the
    // em-dash (`missing — no owner identity?`) and parenthetical
    // (`missing (no owner identity?)`) variants. The `?` is load-bearing:
    // legitimate declarative uses ("no owner identity on this device") lack it.
    let offenders = code_offenders("no owner identity?");
    assert!(
        offenders.is_empty(),
        "phrasing regression: {} code site(s) still say 'no owner identity?' — \
         replace with an honest owner-not-loaded message. Offenders: {offenders:?}",
        offenders.len()
    );
}

#[test]
fn no_node_not_running_or_no_owner_identity_phrasing_in_production_sources() {
    // PR #169 review: this mixed phrasing guarded owner-derived handles (e.g.
    // dm_outbox) on the same path as the standardized message, so users could
    // still see it. Replaced with the classified owner-not-loaded message.
    let offenders = code_offenders("node not running or no owner identity");
    assert!(
        offenders.is_empty(),
        "phrasing regression: {} code site(s) still say 'node not running or no owner \
         identity' — use the classified owner-not-loaded message. Offenders: {offenders:?}",
        offenders.len()
    );
}

#[test]
fn no_destructive_recreate_identity_advice_in_lib_rs() {
    // ZEB-801: the pre-fix owner-not-loaded message advised "recreate identity"
    // — unrecoverable on a file-store identity — during ordinary startup. The
    // user-facing sentence must never return. (The word "recreate" may still
    // appear in explanatory comments describing why we avoid it; this guards
    // the specific destructive sentence, not the word.)
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src
        .matches("please restart the app or recreate identity")
        .count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still advise the destructive \
         'restart the app or recreate identity' — use OWNER_STILL_STARTING_MSG / \
         OWNER_NO_IDENTITY_MSG"
    );
}

#[test]
fn owner_not_loaded_messages_only_inlined_in_const_or_docs() {
    // PR #169 (CodeRabbit + Greptile) + ZEB-801: every *code* site must reach
    // the owner-not-loaded message through `NodeState::owner_not_loaded_msg`,
    // never an inline copy of the value, so the phrasing can't drift across
    // call sites. Each literal is permitted only in (a) ITS OWN const
    // definition and (b) `///` doc-comments documenting the returned error text.
    //
    // Each literal is bound to its specific constant (CodeRabbit): a swapped
    // definition, or one whose literal was removed, fails `defined` below —
    // a check that accepts either name for either literal would not catch that.
    const PAIRS: [(&str, &str); 2] = [
        (
            "OWNER_STILL_STARTING_MSG",
            "Owner identity not loaded — the app is still starting. Try again in a moment.",
        ),
        (
            "OWNER_NO_IDENTITY_MSG",
            "Owner identity not loaded — no identity is set up on this device yet.",
        ),
    ];
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");

    for (const_name, literal) in PAIRS {
        let needle = format!("const {const_name}");
        let mut defined = false;
        let mut offenders = Vec::new();
        let mut prev = "";
        for (idx, line) in src.lines().enumerate() {
            if line.contains(literal) {
                // This literal's OWN const definition — tolerate the two-line
                // form (name on the previous line) and a same-line reformat.
                if line.contains(&needle) || prev.contains(&needle) {
                    defined = true;
                } else if !line.trim_start().starts_with("///") {
                    offenders.push(idx + 1);
                }
            }
            prev = line;
        }
        assert!(
            defined,
            "{const_name} has no matching `const {const_name}` definition holding its own literal"
        );
        assert!(
            offenders.is_empty(),
            "inline copies of {const_name} at lib.rs lines {offenders:?} — \
             route through owner_not_loaded_msg so the text stays single-sourced"
        );
    }
}
