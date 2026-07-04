//! Cross-platform library for enumerating network interfaces with metadata.
//!
//! `netdev` provides a unified API for discovering local network interfaces
//! and retrieving commonly used metadata across platforms.
//!
//! Main entry points:
//! - [`get_interfaces`] returns a snapshot of all visible interfaces.
//! - [`Interface`] represents one interface and its collected metadata.
//! - [`get_default_interface`] and [`get_default_gateway`] are available with the `gateway` feature (default).
//!
/// ZEB-626 (zeblithic): compile-time marker proving THIS patched vendored copy
/// (not crates.io netdev 0.45.0) is in the dependency graph. Asserted by a
/// `const` block in harmony-app's `iroh_endpoint` tests — if a future
/// netwatch/iroh bump swaps an unpatched netdev back in, the test target
/// fails to COMPILE on every platform (no reliance on host WiFi hardware).
/// See README.zeblithic.md.
pub const ZEBLITHIC_ZEB_626_PATCH: bool = true;

pub mod interface;
pub mod net;
mod os;
pub mod prelude;
#[cfg(feature = "gateway")]
pub mod route;
pub mod stats;

pub use ipnet;

pub use interface::get_interfaces;
pub use interface::interface::Interface;
pub use net::device::NetworkDevice;
pub use net::mac::MacAddr;

#[cfg(feature = "gateway")]
pub use interface::get_default_interface;
#[cfg(feature = "gateway")]
pub use route::get_default_gateway;
