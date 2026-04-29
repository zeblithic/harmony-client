pub mod cert;
pub mod persist;
pub mod sas;
pub mod session;
pub mod state_machine;
pub mod transport;
pub mod types;
pub mod zenoh_transport;

pub use types::*;

/// Zenoh key prefix for pairing wire messages.
pub const PAIRING_KEY_PREFIX: &str = "harmony/pairing/v2/lan";

/// Zenoh subscription glob for pairing wire messages.
pub const PAIRING_KEY_GLOB: &str = "harmony/pairing/v2/lan/**";

/// Maximum size in bytes of any wire message on the pairing scope. Pairing
/// messages are tiny (DISCOVER ~150 bytes; ENROLL with full OwnerState
/// snapshot ~5-10 KB). This cap rejects malicious or accidentally-massive
/// payloads on harmony/pairing/v2/lan/** subscriptions before any decode
/// work is done.
pub const MAX_PAIRING_WIRE_BYTES: usize = 64 * 1024;
