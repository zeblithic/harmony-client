# Vendored `zenoh-link` 1.9.0 — ZEB-368 fork

This is a verbatim copy of crates.io `zenoh-link` **1.9.0** plus a minimal, additive fork that
teaches Zenoh's closed locator dispatch the `iroh/<64-hex>` scheme. It is applied via
`[patch.crates-io]` in `../../Cargo.toml`, which replaces `zenoh-link` **graph-wide** — so
`zenoh-transport`'s own dispatch compiles against this copy (we don't inject a manager; we *become*
the dispatch). Pristine upstream: <https://github.com/eclipse-zenoh/zenoh> tag `1.9.0`, crate
`zenoh-link`.

## The diff (all in `src/lib.rs`, search `ZEB-368`)

1. `pub const IROH_LOCATOR_PREFIX = "iroh"`, the `IrohLinkManagerFactory` type, the
   `IROH_LINK_MANAGER_FACTORY` `OnceLock`, and `register_iroh_link_manager_factory()` — the
   process-global seam harmony populates before `zenoh::open`.
2. `LinkKind::Iroh` enum variant.
3. `LinkKind::try_from` / `new_supported_links` / `ALL_SUPPORTED_LINKS` — route/list `iroh`.
4. `LocatorInspector::is_reliable` (`Ok(true)`) + `is_multicast` (`Ok(false)`) — **panic-safety**:
   their `_ => unreachable!()` catch-alls would otherwise crash the session on the first `iroh`
   locator (`is_multicast` is consulted for every locator during routing).
5. `LinkManagerBuilderUnicast::make` — the `LinkKind::Iroh` arm dispatches to the registered
   factory, or `bail!`s (never panics) if harmony hasn't registered one.

## Testing the fork

The fork is a pure `[patch]` dependency (not a harmony workspace member), so its behavior is tested
from the **harmony side** — harmony depends on `zenoh-link` directly, so
`tests/iroh_zenoh_registration_integration.rs` calls `zenoh_link::LinkKind::try_from`,
`zenoh_link::LinkManagerBuilderUnicast::make`, and exercises the factory through a real
`zenoh::open`. (Keeping the vendored crate out of the workspace avoids subjecting upstream zenoh code
to harmony's `-D warnings` clippy gate.)

## Re-vendoring on a zenoh upgrade

1. Copy `~/.cargo/registry/src/*/zenoh-link-<NEWVER>/{src/lib.rs,Cargo.toml,README.md}` over this
   directory.
2. Re-apply the 5 numbered additions above (grep the OLD copy for `ZEB-368`).
3. Keep `version = "<NEWVER>"` so `[patch]` satisfies zenoh / zenoh-transport's `=<NEWVER>` pin.
4. A full workspace build + `tests/iroh_zenoh_registration_integration.rs`.

## Why a fork (and not a plugin / a config option)

zenoh 1.9.0 exposes **no** public or `#[internal]`-gated API to register a custom unicast transport:
`LinkKind` is a closed enum, `LinkManagerBuilderUnicast::make` is a closed `match`, and the live
`TransportManager` is `pub(crate)`. Teaching the one closed dispatch crate (`zenoh-link`, a single
file) about `iroh` is the smallest possible change — see the ZEB-368 design doc
`docs/specs/2026-06-02-zeb-321-phase2-zenoh-over-iroh-ingestion-design.md`.
