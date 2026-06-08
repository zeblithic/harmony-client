# ZEB-391 — Filter advertised iroh direct addresses to live, routable interfaces

**Status:** Approved 2026-06-08
**Issue:** [ZEB-391](https://linear.app/zeblith/issue/ZEB-391) (Medium, harmony-client)
**Context:** Surfaced during the ZEB-390 / ZEB-330 Koya↔Ildwyn first-contact test.
Part of the ZEB-321 cross-WAN connectivity umbrella.

## Problem

Each node publishes a `ReachabilityAnnouncePayload` whose `direct_addresses` come
straight from iroh's interface-probed snapshot
(`IrohEndpoint::direct_addresses()` → `self.inner.addr().ip_addrs()`). That set
includes **non-routable / stale** addresses, so a dialing peer can land on the
wrong host — iroh authenticates on the `EndpointId` cert, the wrong host's key ≠
the expected `EndpointId`, and the dial fails `authentication failed`, wasting the
attempt. Two distinct failure modes observed:

1. **Stale / wrong-subnet (worst).** Ildwyn's only live IPv4 was `192.168.86.44`,
   but its record also advertised `192.168.1.216` — and **Koya advertised the same
   `192.168.1.216`** (different port). Two hosts can't share one address, so a dial
   to it hits the wrong host → `EndpointId` mismatch → fail. This address is **not
   assigned to any live interface** on the advertising host.
2. **Virtual switch / container.** Ildwyn advertised `172.23.48.1` — a WSL/Hyper-V
   vSwitch adapter that **is** a live local interface but is never peer-routable. If
   the dialing peer's own network reuses that virtual range (common — WSL defaults
   overlap), the dial lands on the wrong host too.

These are interface-probed **local** addresses (iroh uses the home relay, not these,
for cross-NAT), so filtering them never drops a useful reflexive/public address.

## Goal

Filter the direct-address set **before** it goes into the published
`ReachabilityAnnouncePayload`, keeping only addresses on a currently-live,
peer-routable, non-virtual local interface. Keep the home relay as the cross-NAT
fallback so an emptied direct set still dials.

## Architecture

A new focused module `src-tauri/src/direct_addr_filter.rs` with a **pure** core
(testable) and a thin impure gather:

```rust
/// One local interface address: the assigned IP, the interface name, and whether
/// the interface is operationally up (`if_addrs::IfOperStatus::Up`).
pub struct LocalIface { pub ip: IpAddr, pub name: String, pub oper_up: bool }

/// PURE: keep only iroh direct addrs that are assigned to a currently-live
/// (operationally up), non-virtual, routable local interface. Order-preserving;
/// deduplication is not required (the publisher's set is already small).
pub fn routable_direct_addresses(
    iroh_addrs: &[SocketAddr],
    local_ifaces: &[LocalIface],
) -> Vec<SocketAddr>;

/// Impure gather: snapshot host interfaces via `if_addrs::get_if_addrs()` and
/// apply the pure filter. FAIL-OPEN: on enumeration error, log a warning and
/// return `iroh_addrs` unfiltered (no connectivity regression vs today).
pub fn gather_routable_direct_addrs(iroh_addrs: Vec<SocketAddr>) -> Vec<SocketAddr>;

/// Async wrapper for callers on a Tokio worker: offloads the blocking
/// `get_if_addrs()` syscall to `spawn_blocking` so a burst of reachability
/// republishes can't stall an executor worker. Returns an empty set (relay
/// fallback) on task panic / shutdown. The sync `gather_*` above is retained for
/// the sync `Fn() -> Vec<u8>` pkarr routing-blob builder, which cannot `.await`.
pub async fn gather_routable_direct_addrs_async(iroh_addrs: Vec<SocketAddr>) -> Vec<SocketAddr>;
```

`if-addrs` is already in `Cargo.lock` (transitive via `if-watch`); ZEB-391 adds it
as a **direct** dependency so it can be imported. `get_if_addrs()` returns the
currently-assigned addresses (each on an up interface), giving both the live-IP set
and the interface name in one call.

## Filter rules (the pure core)

For each `sa` in `iroh_addrs`, **keep** it iff ALL hold:

1. **Routable IP shape.** `!ip.is_loopback() && !ip.is_unspecified()` and not
   link-local — IPv4 `169.254.0.0/16` (`Ipv4Addr::is_link_local`), IPv6 `fe80::/10`
   (`(seg0 & 0xffc0) == 0xfe80`). (IPv6 unique-local `fc00::/7` and IPv4 private
   ranges are **kept** — they're valid on a LAN.)
2. **Live + non-virtual interface.** Some `LocalIface` has `iface.ip == sa.ip()`,
   `iface.oper_up` (the interface is operationally up — `IfOperStatus::Up`), and
   `!is_virtual_iface(&iface.name)`. The `oper_up` gate rejects a stale address still
   listed on a physical interface that has gone down (cable pulled / Wi-Fi dropped)
   so peers don't dial an unreachable endpoint.

`is_virtual_iface(name)` matches on the **lowercased** name containing any of a
conservative pattern set:

```
"docker", "veth", "br-", "virbr", "vmnet", "vboxnet", "vethernet"
```

- `vethernet` covers Windows WSL/Hyper-V (`vEthernet (WSL)`, `vEthernet (Default Switch)`).
- `br-` / `virbr` cover Linux bridges / libvirt; `vmnet` VMware; `vboxnet` VirtualBox;
  `veth` container veth pairs; `docker` the default Docker bridge.
- Physical (`en0`, `eth0`, `wlan0`, `Wi-Fi`, `Ethernet`) and VPN/overlay
  (`tun`/`utun`/`tailscale`/`zerotier`) interfaces are **left alone** — VPN handling
  was explicitly out of scope per the approved design.

Rule 1 (stateless) is checked first so a loopback/link-local addr is dropped without
an interface scan. Rule 2 (the allowlist) is what catches the stale/wrong-subnet
`192.168.1.216`; the virtual-name check within it catches `172.23.48.1`.

## Integration

Both publish closures in `lib.rs` filter `ep.direct_addresses()` before building the
payload, but they run in **different execution contexts**:

- **Reachability republish (`~4317`) is async** (`PublishFn` → `Box::pin(async move {…})`),
  fired on `if-watch` network-change events. It uses the async wrapper so the blocking
  interface enumeration is offloaded to `spawn_blocking`:

  ```rust
  let direct_addrs =
      crate::direct_addr_filter::gather_routable_direct_addrs_async(ep.direct_addresses()).await;
  ```

- **pkarr routing `blob_builder` (`~4595`) is a sync `Arc<dyn Fn() -> Vec<u8>>`** (the
  routing-blob contract; already does sync CBOR + signing). It cannot `.await`, so it
  calls the sync `gather_routable_direct_addrs(...)` directly — the marginal
  `getifaddrs` cost is consistent with the closure's existing sync work.

No other change to payload construction; `home_relay_url` and the rest are untouched.

**`network_health.rs` is intentionally NOT filtered** (`:572`, `:1357`). Those feed
the connectivity **diagnostic** surface, where seeing iroh's raw address set —
including the ones we filter out — is more useful for debugging. The ticket scopes
the change to the *advertised* set (the reachability-announce publish path).

## Data flow & error handling

- Normal: iroh raw addrs → `get_if_addrs()` snapshot → pure filter → routable subset
  → payload. If the subset is **empty**, the payload's `direct_addresses` is empty
  and peers dial via `home_relay_url` (relay fallback). No special-casing needed.
- `get_if_addrs()` `Err`: log `warn!` and pass the **unfiltered** iroh set through
  (fail-open). This preserves today's behavior rather than silently killing direct
  reachability when interface enumeration is unavailable.

## Testing

Pure-function unit tests in `direct_addr_filter.rs` with synthetic `LocalIface`
fixtures (no real interface I/O):

- **Stale/wrong-subnet dropped:** `192.168.1.216:p` with ifaces `[en0→192.168.86.44]`
  → filtered out; `192.168.86.44:p` → kept.
- **Virtual dropped:** `172.23.48.1:p` with iface `[vEthernet (WSL)→172.23.48.1]` →
  filtered out even though it's "live".
- **Docker/bridge dropped:** `172.17.0.1:p` on `docker0` → out; `192.168.1.5:p` on
  `br-abc` → out.
- **Physical kept:** addr on `en0`/`eth0`/`Wi-Fi` → kept; IPv6 ULA on a physical
  iface → kept.
- **Down physical dropped:** addr on a non-virtual iface with `oper_up == false`
  (cable pulled / Wi-Fi dropped) → filtered out; an up sibling → kept.
- **Shape drops:** loopback (`127.0.0.1`), link-local (`169.254.x`, `fe80::x`),
  unspecified (`0.0.0.0`) → out regardless of interface.
- **All-filtered → empty** (asserts the relay-fallback precondition: an empty vec,
  not a panic).
- **`is_virtual_iface`** unit table: each pattern + a few physical/VPN negatives.

(The impure `gather_*` fail-open path is covered by the pure core + a smoke that it
returns *something* for the loopback-only test host; not asserting exact host IPs.)

Gates: `cargo fmt`, `cargo clippy --all-targets --features test-fixtures -D warnings`,
`cargo nextest run -p harmony-app --lib --features test-fixtures`, MSRV.

## Out of scope

- **VPN / overlay interface filtering** (tun/utun/tailscale/zerotier) — deliberately
  left in per the approved design; revisit only if they cause observed dial waste.
- **Filtering the `network_health` diagnostic display** — raw set is intentional there.
- **De-duplication / ranking of direct addresses** — the set is small; not needed.
- **iroh-level address-source configuration** — we filter post-hoc on the publish
  path rather than reaching into iroh's probing.
