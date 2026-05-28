//! ZEB-338: guard against the misleading "node not running?" error phrasing
//! creeping back into lib.rs. The honest message is
//! "Owner identity not loaded — please restart the app or recreate identity."

#[test]
fn no_misleading_node_not_running_phrasing_in_lib_rs() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("node not running?").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'node not running?' \
         in src/lib.rs — replace with 'Owner identity not loaded …'"
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
    // still see it. Replaced with OWNER_NOT_LOADED_MSG.
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("node not running or no owner identity").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'node not running or no owner \
         identity' in src/lib.rs — use OWNER_NOT_LOADED_MSG"
    );
}
