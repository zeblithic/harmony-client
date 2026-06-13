//! ZEB-447 two-agent E2E harness. See `docs/specs/2026-06-13-zeb-447-two-agent-e2e-suite-design.md`.

pub mod bin_resolver;

pub use bin_resolver::resolve_harmony_app_bin;

pub mod node;
pub use node::{NodeConfig, NodeHandle};

pub mod events;
pub use events::{await_event, EventFrame};

pub mod run_dir;
pub use run_dir::RunDir;

pub mod driver;
pub use driver::poll_until;
