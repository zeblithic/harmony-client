# ZEB-626: cut the ~66s/process macOS first-bind stall (netdev CoreWLAN wifid XPC) — design

**Status:** approved (Jake, 2026-07-04)
**Ticket:** ZEB-626 (retitled after diagnosis correction). Companion dossier: ZEB-632 (would-be-upstream report — DO NOT SEND; upstream contact is Jake's alone).

## 1. Diagnosis (what this fixes, and what it doesn't)

The ticket originally tracked "iroh 1.0 per-endpoint Drop-drain teardown (~57s/process)" from the
ZEB-619 upgrade finding (PR #391). That premise is **wrong**, established three ways:

1. **Source (iroh 1.0.1 / netwatch 0.19.1 / tokio 1.52.3, vendored registry copies):**
   `EndpointInner::drop → abort()` is synchronous and joins nothing (`iroh/src/socket.rs:220-230`,
   `:1213-1234`); the netmon monitor and its BSD route reader are async abort-on-drop tokio tasks
   (`netwatch/src/netmon.rs:35-78`, `netmon/bsd.rs:21-58`); the only `spawn_blocking` on the whole
   teardown chain is `drop(std_sock)`/`libc::close` (`netwatch/src/udp.rs:910`) — instant for a
   loopback fd. `Endpoint::close().await` performs the drain, bounded ~≤3s (`socket.rs:1156-1164`).
2. **Measurement (Koya, Darwin 25.5, 2026-07-04):** a test binding **zero** endpoints runs in
   0.011s; **one** endpoint (bind + close) = 66.0s; **three** endpoints + real protocol work =
   65.8s. Direct binary run with per-line timestamps shows the process exits **the same second**
   the libtest result line prints — 0s of post-body teardown. The cost is a **fixed ~66s per
   process, inside the test body, at first bind**.
3. **Process sample (5s mid-stall, all 4,443 samples in one stack):**
   `iroh::endpoint::Builder::bind` (`endpoint.rs:286`) → `EndpointInner::bind` (`socket.rs:1036`)
   → `netwatch::netmon::Monitor::new` (`netmon.rs:66`) → `Actor::new` (`actor.rs:67`) →
   `netwatch::interfaces::State::new` → `netdev::interface::get_interfaces()`
   (`netwatch/src/interfaces.rs:259`) → `netdev::os::macos::wifi::get_wifi_transmit_rate`
   (`netdev-0.45.0/src/os/macos/interface.rs:48`, `wifi.rs:14`) → CoreWLAN
   `CWWiFiClient interfaceWithName:` → `-[CWFInterface capabilities]` → **synchronous XPC to the
   macOS `wifid` daemon** (`__NSXPCCONNECTION_IS_WAITING_FOR_A_SYNC_REPLY`), which stalls ~60s for
   unentitled processes.

Nothing consumes the field this computes: the only reference to `transmit_speed` outside netdev in
the whole iroh dependency tree is netwatch's mock constructor setting it to `None`
(`netwatch/src/interfaces.rs:118`). netdev 0.45.0 and netwatch 0.19.1 are the latest releases —
no upstream fix exists to upgrade into, and no upstream issue exists (surveyed 2026-07-04).

Consequences of the misdiagnosis worth undoing:

- CI runs `ubuntu-latest` and **never pays the stall**; the ZEB-619 containment (nextest
  `iroh-endpoint` test-group, `max-threads = 4`, over a filterset matching ~275-320 tests) throttles
  CI for no benefit and is the likely driver of rust-test ~5min → ~16-21min per push.
- The production macOS app pays the same ~60s at its first `Endpoint::bind()` on boot.
- The ZEB-619 "drain contention plateau" (~330-360s/test under full parallelism) was per-process
  wifid XPC stalls contending, not drains.

## 2. Design

### 2.1 Vendored netdev patch

Copy netdev 0.45.0 verbatim from the cargo registry into `src-tauri/vendor/netdev`, keeping its
LICENSE (MIT). Semantic diff, kept minimal for auditability:

- `src/os/macos/interface.rs`: replace
  `iface.transmit_speed = get_wifi_transmit_rate(&iface.name);` with
  `iface.transmit_speed = None;` plus a comment citing ZEB-626/ZEB-632 (CoreWLAN → wifid sync XPC
  stalls ~60s/process for unentitled processes; nothing in our tree consumes the field). Remove the
  now-unused `get_wifi_transmit_rate` import.
- Delete `src/os/macos/wifi.rs` and its module declaration.
- Prune CoreWLAN-only dependencies from the vendored `Cargo.toml` **iff** `wifi.rs` was their sole
  user (verify per dependency: `objc2-core-wlan` certainly; `objc2`/`objc2-foundation` only if no
  other netdev macOS code uses them). Keep `system-configuration` (used by `sc.rs`).
- Version stays `0.45.0` so the `[patch.crates-io]` substitution is exact.
- Add `vendor/netdev/README.zeblithic.md` describing provenance (crates.io 0.45.0), the exact
  divergence, and the exit condition (drop the vendored copy when an upstream netdev/netwatch
  release makes the WiFi query lazy/optional — asks documented in ZEB-632).

### 2.2 Cargo wiring

In `src-tauri/Cargo.toml`'s existing `[patch.crates-io]` block (zenoh-link precedent, ~line 243):
`netdev = { path = "vendor/netdev" }`, with a comment block in the established style explaining the
stall, the ZEB-626/632 provenance, and the version-pin upkeep rule (a future netwatch requiring
netdev >0.45 makes cargo report the patch unused — refresh the vendored copy then). The patch
applies workspace-wide **deliberately**: production macOS boot sheds the same stall.

netdev also becomes a **dev-dependency** of harmony-app (same version; resolved to the vendored
copy by the patch) so the guard test in 2.4 can call it directly.

### 2.3 De-containment

- `src-tauri/.config/nextest.toml`: remove the `[test-groups] iroh-endpoint = { max-threads = 4 }`
  block, its `[[profile.default.overrides]]` filterset, and the ZEB-619 header comment explaining
  them.
- `src-tauri/tests/referral_catalog_roundtrip_integration.rs`: revert the ZEB-619-widened outer
  timeout to its pre-widening value (recover the original from git history of that change).

### 2.4 Guard test (patch-presence tripwire)

A `#[cfg(target_os = "macos")]` unit test in `src-tauri/src/iroh_endpoint.rs`'s test module:
enumerate interfaces via `netdev::interface::get_interfaces()` and assert every **Wireless80211**
interface has `transmit_speed.is_none()`. (Scoped to wireless — final review correction: wired
links legitimately get a `transmit_speed` from netdev's shared unix SIOCGIFXMEDIA path, which the
patch deliberately leaves untouched; an all-interfaces assertion would false-fail on any docked
Mac.) Deterministic (no wall-clock assertion): if a future dependency bump silently swaps in an
unpatched netdev, this fails on any macOS machine with a WiFi interface instead of the suite
re-bloating quietly. On Linux/CI the test does not exist (cfg-gated); CI's guard is the rust-test
job duration itself.

### 2.5 warm_up helper kept, docs corrected

`warm_up_iroh_global_init` (src-tauri/src/iroh_endpoint.rs:507) and all ~26 call sites stay: post-
patch it costs milliseconds and still serializes residual first-bind init ahead of asserted
timeouts. Its doc block (`:481-506`) is rewritten: the ~10s-CI/~30s-macOS/~76s-parallel numbers and
the process-global-init framing are replaced with the corrected mechanism (netdev CoreWLAN stall,
now patched out; remaining first-bind cost is small) and post-patch measurements.

## 3. Verification

1. **Micro re-measurement** (same probes as diagnosis): no-bind control ≈0.01s (unchanged);
   single-endpoint test 66s → expect low single-digit seconds; referral roundtrip 66s → expect
   low single-digit seconds. Direct-binary timestamp run confirms 0s post-body remains.
2. **Plateau check:** run the formerly-throttled filterset at default parallelism locally; confirm
   per-test times stay near their isolated times (no ~330s plateau).
3. **Standard gates:** `cargo fmt --check`, `clippy --workspace --all-targets -D warnings`,
   full `cargo nextest run --workspace --all-targets --features test-fixtures`, vitest/tsc (CI
   parity; frontend untouched).
4. **CI scoreboard:** rust-test duration on the PR run — expectation ~5-9min (from ~16-21min).
   If ubuntu exposes real contention once uncapped, re-add a larger cap (reversible; not expected).
5. **Prod-boot smoke (optional, nice-to-have):** time `harmony-app serve` cold boot to iroh-ready
   on an **isolated profile**. The fleet-koya node is NOT restarted (iroh-1.0 flag-day pending).

## 4. Out of scope

- Any upstream contact (issues/PRs to netdev, n0-computer) — prohibited; ZEB-632 holds the dossier.
- ZEB-631 probabilistic test selection (own ticket; recalibrate after this lands).
- Endpoint pooling in test helpers (refuted: nextest is process-per-test; per-process endpoint
  count is already 2-3; cost is per-process, not per-endpoint).
- e2e-harness changes (long-lived node processes amortize the stall; unaffected either way).

## 5. Risks & maintenance

- **Version skew:** future iroh/netwatch bumps requiring netdev >0.45 → cargo warns the patch is
  unused at lock-update time; the macOS guard test fails; remedy is refreshing `vendor/netdev`
  against the new version (re-applying the one-line change per README.zeblithic.md).
- **Windows/Linux:** the vendored diff touches only `os/macos`; other platforms are byte-identical
  to crates.io 0.45.0.
- **Licensing:** netdev is MIT; LICENSE ships with the vendored copy.
