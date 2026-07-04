# ZEB-626: vendored netdev patch + de-containment — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the ~66s/process macOS first-bind stall (netdev's CoreWLAN→wifid sync XPC) via a vendored one-line netdev patch, then retire the nextest containment that the misdiagnosed stall forced.

**Architecture:** Vendor netdev 0.45.0 into `src-tauri/vendor/netdev` with the WiFi-transmit-rate query removed, substituted graph-wide via the existing `[patch.crates-io]` block (zenoh-link precedent). A macOS-only guard test trips if an unpatched netdev ever re-enters the graph. With the stall gone, the ZEB-619 nextest `iroh-endpoint` 4-thread cap and the widened referral timeout revert.

**Tech Stack:** Rust / cargo `[patch.crates-io]`, cargo-nextest, netdev 0.45.0 (MIT).

**Spec:** `docs/specs/2026-07-04-zeb-626-netdev-corewlan-stall-design.md` (approved 2026-07-04).

## Global Constraints

- ONE cargo invocation at a time, always from `src-tauri/`; `caffeinate -dims` any run expected >2min.
- Vendored netdev version stays exactly `0.45.0` (patch substitution must be exact).
- The vendored diff touches ONLY `os/macos` code + manifest pruning; all other platforms byte-identical to crates.io.
- No upstream contact of any kind (netdev, n0-computer, anyone) — ZEB-632 is the dossier.
- The fleet-koya node is never restarted (iroh-1.0 flag-day pending).
- CI clippy (rust 1.94) is authoritative over local; clippy gate = `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- Never weaken a test assertion; timeout changes follow the referral file's own header rule.
- Registry source to copy from: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/netdev-0.45.0`.

---

### Task 1: Vendored patched netdev + cargo wiring + guard test

**Files:**
- Create: `src-tauri/vendor/netdev/` (from registry copy: `src/`, `build.rs`, `LICENSE`, `README.md`, `Cargo.toml`)
- Create: `src-tauri/vendor/netdev/README.zeblithic.md`
- Modify: `src-tauri/vendor/netdev/Cargo.toml` (strip examples + prune 3 deps)
- Modify: `src-tauri/vendor/netdev/src/os/macos/interface.rs` (~line 6 import, ~line 48 call)
- Delete: `src-tauri/vendor/netdev/src/os/macos/wifi.rs`
- Modify: `src-tauri/vendor/netdev/src/os/macos/mod.rs` (drop `mod wifi;`)
- Modify: `src-tauri/Cargo.toml` (`[patch.crates-io]` ~line 243; `[dev-dependencies]` ~line 206)
- Test: `src-tauri/src/iroh_endpoint.rs` (guard test in existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: patched netdev in the whole cargo graph; `vendored_netdev_never_computes_transmit_speed_on_macos` guard test; the post-patch single-endpoint timing measurement `T1_SECS` that Task 2's doc rewrite cites.

- [ ] **Step 1: Copy the crate (exclude examples/scripts/lockfile/normalized-orig)**

```bash
REG=~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/netdev-0.45.0
mkdir -p src-tauri/vendor/netdev
cp -R "$REG/src" src-tauri/vendor/netdev/src
cp "$REG/build.rs" "$REG/LICENSE" "$REG/README.md" "$REG/Cargo.toml" src-tauri/vendor/netdev/
```

(Do NOT copy `examples/`, `scripts/`, `Cargo.lock`, `Cargo.toml.orig` — path deps don't need them, and the `[[example]]` targets get stripped next.)

- [ ] **Step 2: Prune the vendored `Cargo.toml`**

Delete ALL five `[[example]]` sections (`default_gateway`, `default_interface`, `global_ips`, `list_interfaces`, `serialize`, `stats` — six blocks total) since `examples/` is not vendored, and delete exactly these three dependency sections (wifi.rs is their sole user, verified 2026-07-04):

```toml
[target.'cfg(target_os = "macos")'.dependencies.objc2]
version = "0.6"

[target.'cfg(target_os = "macos")'.dependencies.objc2-core-wlan]
version = "0.3"

[target.'cfg(target_os = "macos")'.dependencies.objc2-foundation]
version = "0.3"
```

Keep everything else (notably `objc2-core-foundation`, `objc2-system-configuration`, `plist` — used by `sc.rs`/darwin code; the iOS-only `block2`/`dispatch2` sections are harmless and stay to minimize the diff).

- [ ] **Step 3: Remove the CoreWLAN query in `src/os/macos/interface.rs`**

Import (line ~6), before:
```rust
        macos::sc::SCInterface, macos::wifi::get_wifi_transmit_rate,
        unix::interface::unix_interfaces,
```
after:
```rust
        macos::sc::SCInterface, unix::interface::unix_interfaces,
```

Call site (~line 47-49), before:
```rust
        if iface.if_type == InterfaceType::Wireless80211 {
            iface.transmit_speed = get_wifi_transmit_rate(&iface.name);
        }
```
after:
```rust
        if iface.if_type == InterfaceType::Wireless80211 {
            // ZEB-626 (zeblithic): upstream computes this via CoreWLAN
            // `CWWiFiClient interfaceWithName:`, a SYNCHRONOUS XPC call into
            // macOS's wifid that stalls ~60s per process for unentitled
            // callers (every test binary / CLI). Nothing in our dependency
            // tree consumes transmit_speed (netwatch/iroh want addrs+routes),
            // so the query is pure cost. Would-be-upstream report: ZEB-632.
            iface.transmit_speed = None;
        }
```

- [ ] **Step 4: Delete the wifi module**

```bash
rm src-tauri/vendor/netdev/src/os/macos/wifi.rs
```

`src/os/macos/mod.rs`, before:
```rust
pub mod interface;
pub mod sc;

mod wifi;
```
after:
```rust
pub mod interface;
pub mod sc;
```

- [ ] **Step 5: Write `src-tauri/vendor/netdev/README.zeblithic.md`**

```markdown
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
```

- [ ] **Step 6: Wire the patch + dev-dependency in `src-tauri/Cargo.toml`**

In `[patch.crates-io]` (~line 243), after the `zenoh-link` entry:

```toml
# netdev (ZEB-626): upstream 0.45.0 queries the WiFi transmit rate via a
# synchronous CoreWLAN->wifid XPC call during interface enumeration; for
# unentitled processes (every test binary) that stalls ~60s, and it runs
# inside the first iroh Endpoint::bind() of every process via netwatch.
# Nothing consumes the field. The vendored copy sets transmit_speed = None
# on macOS; see vendor/netdev/README.zeblithic.md for provenance, the
# guard test, and the version-upkeep rule. Upstream asks live in ZEB-632
# (dossier only — no upstream contact).
netdev = { path = "vendor/netdev" }
```

In `[dev-dependencies]` (~line 206), after the `regex` entry:

```toml
# ZEB-626: direct dev-dep so the guard test can assert the vendored patch
# is active (resolved to vendor/netdev by the [patch.crates-io] entry).
netdev = "0.45.0"
```

- [ ] **Step 7: Verify the patch resolves to the vendored path**

Run (from `src-tauri/`): `cargo tree -i netdev 2>&1 | head -5`
Expected first line: `netdev v0.45.0 (/Users/zeblith/work/zeblithic/harmony-client/src-tauri/vendor/netdev)` — and cargo prints NO "patch ... was not used" warning.

- [ ] **Step 8: Add the guard test to `src-tauri/src/iroh_endpoint.rs`**

Inside the existing `#[cfg(test)] mod tests` block (near the other endpoint tests, after `alpn_constants_are_correct`):

> **Superseded during review (see final-review I-1 + Qodo round 1):** the shipped guard has two
> layers — a non-cfg'd `const` block asserting the vendored crate's `ZEBLITHIC_ZEB_626_PATCH`
> marker (an unpatched netdev fails to COMPILE the test target, all platforms), plus the macOS
> behavioral test below scoped to `Wireless80211` interfaces only (wired links legitimately get a
> `transmit_speed` from netdev's unix SIOCGIFXMEDIA path — an all-interfaces assertion false-fails
> on docked Macs).

```rust
    const _: () = assert!(
        netdev::ZEBLITHIC_ZEB_626_PATCH,
        "unpatched netdev in the graph (ZEB-626) — refresh vendor/netdev per its README"
    );

    #[test]
    #[cfg(target_os = "macos")]
    fn vendored_netdev_never_computes_transmit_speed_on_macos() {
        use netdev::prelude::InterfaceType;
        for iface in netdev::interface::get_interfaces() {
            if iface.if_type != InterfaceType::Wireless80211 {
                continue;
            }
            assert!(
                iface.transmit_speed.is_none(),
                "wireless interface {} has transmit_speed {:?} — unpatched netdev \
                 (CoreWLAN query) back in the graph (ZEB-626)",
                iface.name,
                iface.transmit_speed
            );
        }
    }
```

- [ ] **Step 9: Run the guard test + the timing probe (the red→green moment)**

Run (from `src-tauri/`, rebuilds netwatch/iroh/harmony-app against the patch, ~7-12min):
`caffeinate -dims cargo nextest run --features test-fixtures -E 'test(=iroh_endpoint::tests::vendored_netdev_never_computes_transmit_speed_on_macos) or test(=iroh_endpoint::tests::iroh_endpoint_inits_with_ephemeral_secret)'`

Expected: both PASS, and `iroh_endpoint_inits_with_ephemeral_secret` reports **single-digit seconds** (pre-patch baseline measured 2026-07-04: 66.0s; no-bind control 0.011s). Record the measured time as **T1_SECS** for Task 2's doc rewrite. If it is still >30s: STOP, sample the process again (`sample <pid> 5`), do not proceed to Task 2.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/vendor/netdev src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/iroh_endpoint.rs
git commit -m "ZEB-626: vendor netdev 0.45.0 with CoreWLAN wifi-rate query removed

First Endpoint::bind() per process stalled ~66s on macOS in netdev's
synchronous CoreWLAN->wifid XPC (via netwatch interface enumeration).
Nothing consumes transmit_speed; the vendored copy sets it to None.
[patch.crates-io] substitution graph-wide + macOS guard test."
```

### Task 1b: hermetic DNS resolver (second stall, discovered by Task 1's probe)

Task 1's probe left a fixed ~22s/process residue (66s → 22.1s, identical for Minimal and N0
shapes). Sample: `Builder::bind` → `iroh_dns::dns::DnsResolver::new` → `HickoryResolver::
system_config` → `hickory_resolver::system_conf::apple::read_system_conf` →
`SCDynamicStoreCreateWithOptions` — a second synchronous SystemConfiguration XPC (configd this
time), stalled for unentitled processes. iroh builds the system resolver eagerly at bind unless
`Builder::dns_resolver(DnsResolver)` is supplied; `DnsResolver::with_nameserver(SocketAddr)`
(iroh-dns 1.0.1 dns.rs:351) builds without reading system conf. Hermetic tests never resolve
names (loopback dials by addr, relays disabled), so they get a resolver pointed at an
unanswering loopback port.

**Files:** `src-tauri/src/iroh_endpoint.rs` (helper + `*_inner` seam + warm_up + 3 N0 unit
tests), the 5 other src hermetic builder sites (`tunnel_task.rs`, `zenoh_iroh_transport.rs`,
`reachability_publisher.rs`, `lib.rs`, `zenoh_iroh_link.rs` ×2), the 10 integration builder
sites (`pkarr_iroh_redeem_full`, `zeb_373_dynamic_dial`, `referral_catalog_roundtrip` ×2,
`network_health_two_endpoint` ×2, `friend_token_roundtrip`, `iroh_zenoh_registration`,
`community_open_join_cross_wan`, `community_misc/community_reachability_two_engine`).

- [ ] **Step 1:** Add to `iroh_endpoint.rs` (near `warm_up_iroh_global_init`, pub under
  `#[cfg(any(test, feature = "test-fixtures"))]`):

```rust
pub fn hermetic_dns_resolver() -> iroh::dns::DnsResolver {
    iroh::dns::DnsResolver::with_nameserver(std::net::SocketAddr::from(([127, 0, 0, 1], 1)))
}
```

- [ ] **Step 2:** Split `new_with_secret_and_relays` into an `*_inner(secret, relays,
  dns_resolver: Option<DnsResolver>)` (prod wrapper passes `None`; builder chains
  `.dns_resolver(r)` when `Some`) + a `#[cfg(any(test, feature = "test-fixtures"))]`
  `new_with_secret_and_relays_hermetic_dns` passing `Some(hermetic_dns_resolver())`. Switch the
  3 N0-path unit tests (`iroh_endpoint_inits_with_ephemeral_secret`,
  `custom_relay_list_overrides_default_map`, `apply_relay_urls_diffs_insert_and_remove`) to the
  hermetic-dns variant (they test relay-map logic, not DNS). Production behavior unchanged.
- [ ] **Step 3:** Add `.dns_resolver(hermetic_dns_resolver())` (or the
  `harmony_app::iroh_endpoint::hermetic_dns_resolver()` path from integration tests) to the
  warm_up builder and every hermetic `Endpoint::builder(presets::Minimal)` site listed above.
- [ ] **Step 4:** Re-run the Task 1 Step 9 probe pair + the referral direct-run. Expected: both
  drop from ~22s to low single digits; that measured value becomes **T1_SECS**.
- [ ] **Step 5:** Commit (`ZEB-626: hermetic DNS resolver — skip eager system-conf read at test bind`).
- [ ] **Step 6:** Append the iroh_dns eager-`read_system_conf` finding to the ZEB-632 dossier
  (second would-be-upstream ask: build the system resolver lazily / skip when unused; prod macOS
  boot keeps this ~22s until upstream changes it — only tests are fixed by this task).

### Task 2: De-containment + timeout revert + warm_up doc correction

**Files:**
- Modify: `src-tauri/.config/nextest.toml` (delete lines 27-41)
- Modify: `src-tauri/tests/referral_catalog_roundtrip_integration.rs` (~lines 60-66)
- Modify: `src-tauri/src/iroh_endpoint.rs` (doc block above `warm_up_iroh_global_init`, ~lines 481-506)

**Interfaces:**
- Consumes: Task 1's patched graph + measured `T1_SECS`.
- Produces: uncapped nextest profile; `OUTER_TIMEOUT` back to 90s; corrected helper docs.

- [ ] **Step 1: Remove the ZEB-619 containment from `src-tauri/.config/nextest.toml`**

Delete this entire block (lines 27-41; lines 1-25, the historical quarantine note ending "this file is kept as the home for any future nextest configuration.", stay):

```toml
# ZEB-619 (iroh 0.98.2 -> 1.0.1): iroh 1.0 moved endpoint drain off the
# awaited close() path onto Drop, so every hermetic test process pays a
# ~57s background teardown (netwatch blocking socket-close + per-endpoint
# netmon monitor) AFTER its test body finishes. Under nextest's default
# full-CPU parallelism those drains contend and serialize into a
# ~330-360s-per-test plateau (54min suite, one outer-timeout failure).
# Capping concurrency for endpoint-heavy tests keeps the drains from
# stampeding; the ~57s/process floor itself is upstream behavior — a
# follow-up ticket tracks cutting it (endpoint pool or an iroh knob).
[test-groups]
iroh-endpoint = { max-threads = 4 }

[[profile.default.overrides]]
filter = 'test(iroh) | test(zenoh) | test(tunnel) | test(reachability) | test(dial) | test(liveness) | binary(~iroh) | binary(~zenoh) | binary(~referral) | binary(~network_health) | binary(~friend_token) | binary(~open_join) | binary(~api_server) | binary(~profile_isolation)'
test-group = 'iroh-endpoint'
```

- [ ] **Step 2: Revert the referral OUTER_TIMEOUT**

`src-tauri/tests/referral_catalog_roundtrip_integration.rs` (~line 61), before:
```rust
// ZEB-619: 90s -> 180s. iroh 1.0's Drop-based endpoint drain adds ~57s of
// per-process teardown that contends across parallel test processes; under
// a full-suite run this test's real-QUIC body exceeded 90s (it passes in
// isolation at ~57s total). 180s keeps ~3x headroom over the isolated cost
// without weakening any assertion, per this file's header rule.
const OUTER_TIMEOUT: Duration = Duration::from_secs(180);
```
after:
```rust
// ZEB-626: back to the pre-ZEB-619 90s. The "drain teardown" that forced
// 180s was actually netdev's CoreWLAN wifid XPC stall at first bind
// (~66s/process), removed by the vendored netdev patch (vendor/netdev).
const OUTER_TIMEOUT: Duration = Duration::from_secs(90);
```

- [ ] **Step 3: Rewrite the `warm_up_iroh_global_init` doc block**

In `src-tauri/src/iroh_endpoint.rs`, replace the doc comment block above `pub async fn warm_up_iroh_global_init` (the block at ~lines 481-506 describing "~10s on CI, ~30s on some macOS hosts (~76s under heavy local parallelism)" process-global init) with (substitute `T1_SECS` from Task 1 Step 9):

```rust
/// Warm up residual first-`bind()` initialization in this process ahead of
/// tests that assert tight timeouts.
///
/// History (ZEB-619 -> ZEB-626): the first `iroh::Endpoint::bind()` in a
/// process used to stall ~60s on macOS (~66s/process measured 2026-07-04).
/// That was neither iroh teardown nor a process-global iroh init: netwatch's
/// interface enumeration (the `netdev` crate) queried each wireless
/// interface's transmit rate via CoreWLAN, whose synchronous XPC call into
/// macOS's `wifid` stalls for unentitled processes. The vendored netdev
/// patch (vendor/netdev, ZEB-626) removes that query; post-patch the same
/// single-endpoint test completes in ~T1_SECSs. This helper stays: it still
/// serializes the small remaining first-bind cost (crypto-provider init,
/// netmon route-socket setup) ahead of asserted timeouts, and it marks the
/// measurement pattern — if first-bind cost regresses, sample the process
/// mid-stall (ZEB-626 diagnosis) before widening any timeout.
```

Keep the function body and signature untouched.

- [ ] **Step 4: Plateau check — the formerly-throttled set at default parallelism**

Run (from `src-tauri/`, ~expect minutes not tens of minutes):
`caffeinate -dims cargo nextest run --features test-fixtures -E 'test(iroh) | test(zenoh) | test(tunnel) | test(reachability) | test(dial) | test(liveness) | binary(~iroh) | binary(~zenoh) | binary(~referral) | binary(~network_health) | binary(~friend_token) | binary(~open_join) | binary(~api_server) | binary(~profile_isolation)' 2>&1 | tail -15`

Expected: all tests PASS at full parallelism; per-test times near isolated times (no ~330s plateau entries); `referral_catalog_roundtrip` passes within its restored 90s outer timeout. If a plateau reappears: STOP, capture the slowest test names + a process sample, and reassess before Task 3.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/.config/nextest.toml src-tauri/tests/referral_catalog_roundtrip_integration.rs src-tauri/src/iroh_endpoint.rs
git commit -m "ZEB-626: retire the iroh-endpoint nextest cap + 180s referral timeout

Containment for the misdiagnosed 'Drop-drain' (actually the now-patched
netdev CoreWLAN stall). Formerly-throttled set re-validated at default
parallelism; warm_up doc corrected with the measured mechanism."
```

### Task 3: Full gates

**Files:** none modified (verification only; ledger append).

**Interfaces:**
- Consumes: Tasks 1-2 committed.
- Produces: green branch ready for final review + PR.

- [ ] **Step 1: fmt** — `cargo fmt --all -- --check` (from `src-tauri/`). Expected: no output, exit 0.
- [ ] **Step 2: clippy (CI-authoritative form)** — `caffeinate -dims cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Expected: clean. (Local clippy is older than CI's rust 1.94 — CI remains the final word.)
- [ ] **Step 3: full nextest sweep** — `caffeinate -dims cargo nextest run --workspace --all-targets --features test-fixtures 2>&1 | tail -5`. Expected: **4055+ tests, 0 failures** (4054 pre-existing + the new guard test; count may drift with main). Note the total wall-time in the ledger — pre-patch full sweeps ran ~50-70min under the cap; expect a large drop.
- [ ] **Step 4: frontend parity gates (untouched code, cheap confirmation)** — from repo root: `npx tsc --noEmit` then `npx vitest run 2>&1 | tail -3`. Expected: clean / 3076+ pass.
- [ ] **Step 5: ledger** — append results (probe times T1_SECS + referral, plateau-check wall, sweep wall) to `.superpowers/sdd/progress.md`.

---

## Plan self-review (done at write time)

- **Spec coverage:** §2.1 vendored crate → Task 1 Steps 1-5; §2.2 wiring + dev-dep → Task 1 Step 6; §2.4 guard test → Task 1 Step 8; §2.3 de-containment + revert → Task 2 Steps 1-2; §2.5 warm_up docs → Task 2 Step 3; §3 verification ladder → Task 1 Step 9 (micro), Task 2 Step 4 (plateau), Task 3 (gates; CI scoreboard lands with the PR). §3.5 prod-boot smoke is spec-optional and deliberately omitted (probes cover the mechanism; fleet node untouchable).
- **Placeholders:** `T1_SECS` is not a placeholder — it is produced by Task 1 Step 9 and substituted in Task 2 Step 3.
- **Type consistency:** guard test uses `netdev::interface::get_interfaces()` — the exact call form netwatch uses (`netwatch/src/interfaces.rs:259`); `transmit_speed` is `Option<u64>`.
