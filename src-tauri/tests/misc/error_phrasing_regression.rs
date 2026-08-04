//! ZEB-338 + ZEB-801: guard against misleading owner-not-loaded error phrasing
//! creeping back into lib.rs. The honest messages are the two ZEB-801 constants
//! — OWNER_STILL_STARTING_MSG ("… the app is still starting. Try again in a
//! moment.") and OWNER_NO_IDENTITY_MSG ("… no identity is set up on this device
//! yet.") — reached through NodeState::owner_not_loaded_msg /
//! owner_not_loaded_msg_locked, never the old destructive "recreate identity"
//! text.

#[test]
fn no_misleading_node_not_running_phrasing_in_lib_rs() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("node not running?").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'node not running?' \
         in src/lib.rs — replace with the classified owner-not-loaded message"
    );
}

#[test]
fn no_misleading_no_owner_identity_phrasing_in_lib_rs() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("missing — no owner identity?").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'missing — no owner identity?'"
    );
}

#[test]
fn no_node_not_running_or_no_owner_identity_phrasing_in_lib_rs() {
    // PR #169 review: this mixed phrasing guarded owner-derived handles (e.g.
    // dm_outbox) on the same path as the standardized message, so users could
    // still see it. Replaced with the classified owner-not-loaded message.
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("node not running or no owner identity").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'node not running or no owner \
         identity' in src/lib.rs — use the classified owner-not-loaded message"
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
    // the owner-not-loaded message through NodeState::owner_not_loaded_msg /
    // owner_not_loaded_msg_locked (which read the two constants below), never an
    // inline copy of the value, so the phrasing can't drift across call sites.
    // Each literal is permitted only in (a) its const definition and (b) `///`
    // doc-comments documenting the returned error text.
    const LITERALS: [&str; 2] = [
        "Owner identity not loaded — the app is still starting. Try again in a moment.",
        "Owner identity not loaded — no identity is set up on this device yet.",
    ];
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");

    let mut prev = "";
    let mut offenders = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let is_doc = line.trim_start().starts_with("///");
        // Tolerate both the two-line const form (name on the previous line) and
        // a future same-line reformat (name + literal together).
        let is_const_def = line.contains("const OWNER_STILL_STARTING_MSG")
            || prev.contains("const OWNER_STILL_STARTING_MSG")
            || line.contains("const OWNER_NO_IDENTITY_MSG")
            || prev.contains("const OWNER_NO_IDENTITY_MSG");
        if LITERALS.iter().any(|lit| line.contains(lit)) && !is_doc && !is_const_def {
            offenders.push(idx + 1);
        }
        prev = line;
    }

    assert!(
        offenders.is_empty(),
        "inline copies of an owner-not-loaded message at lib.rs lines {offenders:?} — \
         route through owner_not_loaded_msg[_locked] so the text stays single-sourced"
    );
}
