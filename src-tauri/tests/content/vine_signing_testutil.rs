//! ZEB-673: shared signer registry for vine integration tests.
//!
//! Strict wire admission requires real signer identities, but the tests'
//! readable creator names ("alice-addr") are worth keeping. This registry
//! lazily mints one identity per name; `addr()` translates a name to its
//! signer's derived hex address. Process-global so every mention of a
//! name agrees on the identity. Mirrors the unit-test registry in
//! `vine_feed_cache::tests`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

type SignerRegistry = HashMap<String, Arc<harmony_identity::PrivateIdentity>>;

fn signers() -> &'static Mutex<SignerRegistry> {
    static SIGNERS: OnceLock<Mutex<SignerRegistry>> = OnceLock::new();
    SIGNERS.get_or_init(Default::default)
}

pub fn signer_for(name: &str) -> Arc<harmony_identity::PrivateIdentity> {
    let mut guard = signers().lock().unwrap();
    guard
        .entry(name.to_string())
        .or_insert_with(|| {
            Arc::new(harmony_identity::PrivateIdentity::generate(
                &mut rand::rngs::OsRng,
            ))
        })
        .clone()
}

/// Derived hex address for a named test signer.
pub fn addr(name: &str) -> String {
    hex::encode(signer_for(name).public_identity().address_hash)
}

/// Descriptor topic for a named test signer.
pub fn topic(name: &str) -> String {
    format!("harmony/vines/{}", addr(name))
}

/// Reaction topic for a vine by named creator, reacted by named reactor.
pub fn reaction_topic(creator: &str, vine_id: &str, reactor: &str) -> String {
    format!(
        "harmony/vines/{}/reactions/{vine_id}/{}",
        addr(creator),
        addr(reactor)
    )
}
