pub mod cert;
pub mod persist;
pub mod sas;
pub mod session;
pub mod state_machine;
pub mod transport;
pub mod types;
pub mod zenoh_transport;

pub use types::*;

/// Zenoh key prefix for pairing wire messages. Used by `zenoh_transport`
/// when building per-phase publish keys (e.g. `<prefix>/<session>/<phase>`).
pub const PAIRING_KEY_PREFIX: &str = "harmony/pairing/v2/lan";

/// Same as [`PAIRING_KEY_PREFIX`] with a trailing slash. Provided as a
/// separate const so the per-Zenoh-event prefix-match in the node event
/// loop can do `starts_with` against a `&'static str` instead of
/// formatting a fresh `String` on every subscription sample (the path is
/// hot — fires for every community update, mail message, voice packet,
/// etc., not just pairing). Keep these two consts in sync if either ever
/// changes — there's no concat! workaround because PAIRING_KEY_PREFIX is
/// a `const &str`, not a literal.
pub const PAIRING_KEY_PREFIX_SLASH: &str = "harmony/pairing/v2/lan/";

/// Zenoh subscription glob for pairing wire messages.
pub const PAIRING_KEY_GLOB: &str = "harmony/pairing/v2/lan/**";

/// Maximum size in bytes of any wire message on the pairing scope. Pairing
/// messages are tiny (DISCOVER ~150 bytes; ENROLL with full OwnerState
/// snapshot ~5-10 KB). This cap rejects malicious or accidentally-massive
/// payloads on harmony/pairing/v2/lan/** subscriptions before any decode
/// work is done.
pub const MAX_PAIRING_WIRE_BYTES: usize = 64 * 1024;
