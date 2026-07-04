# Vendored netdev 0.45.0 (zeblithic patch — ZEB-626)

Byte-identical copy of netdev 0.45.0 from crates.io (MIT, LICENSE included;
`examples/`, `scripts/`, `Cargo.lock`, `Cargo.toml.orig` omitted), substituted
graph-wide via `[patch.crates-io]` in `../../Cargo.toml`, with one semantic
change:

**`src/os/macos/interface.rs` no longer queries the WiFi transmit rate.**
Upstream calls CoreWLAN (`CWWiFiClient interfaceWithName:`) per wireless
interface on every enumeration; that is a synchronous XPC call into macOS's
`wifid` which stalls ~60s per process for unentitled callers. Via
netwatch→iroh, every first `Endpoint::bind()` in every process paid it —
~66s/process measured in hermetic tests (ZEB-626 diagnosis, 2026-07-04).
`transmit_speed` is set to `None`; `src/os/macos/wifi.rs` and the three
wifi-only deps (`objc2`, `objc2-core-wlan`, `objc2-foundation`) are removed.
The `[[example]]` targets are stripped from `Cargo.toml` because `examples/`
is not vendored. Nothing else is modified; non-macOS code is untouched.

Guard: `vendored_netdev_never_computes_transmit_speed_on_macos` in
`src-tauri/src/iroh_endpoint.rs` fails on any macOS machine if an unpatched
netdev re-enters the graph.

**Exit condition:** drop this vendored copy when an upstream netdev/netwatch
release makes the WiFi query lazy/optional (the asks we would file are
documented in Linear ZEB-632 — do NOT contact upstream; that is Jake's call).
**Upkeep:** if a future netwatch/iroh bump requires netdev >0.45, cargo will
report this patch as unused — refresh the copy against the new version and
re-apply the change above.
