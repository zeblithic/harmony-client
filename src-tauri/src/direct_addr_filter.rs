//! ZEB-391: filter the iroh direct-address set before it is advertised in a
//! [`ReachabilityAnnouncePayload`](crate::reachability_record::ReachabilityAnnouncePayload).
//!
//! iroh's interface-probed snapshot (`IrohEndpoint::direct_addresses()`) includes
//! addresses that are never peer-routable:
//!
//! * **Stale / wrong-subnet** addresses no live interface holds (observed: two
//!   hosts both advertising `192.168.1.216` — a dial lands on the wrong host, whose
//!   `EndpointId` key ≠ the expected one → `authentication failed`).
//! * **Virtual-switch / container** adapter addresses that *are* live but internal
//!   (observed: WSL/Hyper-V `vEthernet` `172.23.48.1`, Docker bridges, etc.).
//!
//! We keep only addresses that are (1) routable in shape (not loopback /
//! link-local / unspecified) and (2) assigned to a currently-live, non-virtual
//! local interface. The home relay stays as the cross-NAT fallback, so an emptied
//! direct set still dials.

use std::net::{IpAddr, SocketAddr};

/// One live local interface address: the assigned IP + the interface name.
#[derive(Debug, Clone)]
pub struct LocalIface {
    pub ip: IpAddr,
    pub name: String,
}

/// Interface-name substrings (matched case-insensitively) that mark a virtual
/// switch / container / bridge adapter whose addresses are never peer-routable.
/// Conservative on purpose: physical (`en0`/`eth0`/`wlan0`/`Wi-Fi`/`Ethernet`) and
/// VPN/overlay (`tun`/`utun`/`tailscale`/`zerotier`) interfaces are intentionally
/// left in — VPN handling is out of scope (ZEB-391 design).
const VIRTUAL_IFACE_PATTERNS: &[&str] = &[
    "docker",    // Docker default bridge (docker0)
    "veth",      // container veth pairs
    "br-",       // Linux / Docker user-defined bridges
    "virbr",     // libvirt bridges
    "vmnet",     // VMware
    "vboxnet",   // VirtualBox
    "vethernet", // Windows WSL / Hyper-V vSwitch ("vEthernet (WSL)")
];

/// True if `name` looks like a virtual-switch / container interface.
pub fn is_virtual_iface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_IFACE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// True if `ip` is a shape we'd never advertise (loopback / unspecified /
/// link-local). IPv4 link-local is `169.254.0.0/16`; IPv6 link-local is `fe80::/10`
/// (the std `is_unicast_link_local` helper is nightly-only, so we mask the top 10
/// bits ourselves).
fn is_unroutable_shape(ip: IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// PURE core: keep only iroh direct addrs assigned to a currently-live,
/// non-virtual, routable local interface. Order-preserving.
pub fn routable_direct_addresses(
    iroh_addrs: &[SocketAddr],
    local_ifaces: &[LocalIface],
) -> Vec<SocketAddr> {
    iroh_addrs
        .iter()
        .copied()
        .filter(|sa| {
            let ip = sa.ip();
            if is_unroutable_shape(ip) {
                return false;
            }
            local_ifaces
                .iter()
                .any(|iface| iface.ip == ip && !is_virtual_iface(&iface.name))
        })
        .collect()
}

/// Snapshot host interfaces via `if_addrs::get_if_addrs()` and apply the pure
/// filter. **Fail-open:** on enumeration error, log a warning and return
/// `iroh_addrs` unfiltered — preserving pre-ZEB-391 behavior rather than silently
/// killing direct reachability when interface enumeration is unavailable.
pub fn gather_routable_direct_addrs(iroh_addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let ifaces: Vec<LocalIface> = match if_addrs::get_if_addrs() {
        Ok(ifs) => ifs
            .into_iter()
            .map(|i| LocalIface {
                ip: i.ip(),
                name: i.name,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                "if_addrs::get_if_addrs failed ({e}); advertising unfiltered iroh direct addrs"
            );
            return iroh_addrs;
        }
    };
    routable_direct_addresses(&iroh_addrs, &ifaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }
    fn iface(ip: &str, name: &str) -> LocalIface {
        LocalIface {
            ip: ip.parse().unwrap(),
            name: name.to_string(),
        }
    }

    #[test]
    fn drops_stale_wrong_subnet_address() {
        // Host is on 192.168.86.44; iroh also advertised a stale 192.168.1.216 that
        // no live interface holds (the worst case — two hosts both claimed it).
        let ifaces = vec![iface("192.168.86.44", "en0")];
        let got = routable_direct_addresses(
            &[sa("192.168.1.216", 5000), sa("192.168.86.44", 5000)],
            &ifaces,
        );
        assert_eq!(got, vec![sa("192.168.86.44", 5000)]);
    }

    #[test]
    fn drops_live_but_virtual_wsl_vswitch() {
        // 172.23.48.1 IS a live interface (vEthernet WSL) but never peer-routable.
        let ifaces = vec![
            iface("192.168.86.44", "Wi-Fi"),
            iface("172.23.48.1", "vEthernet (WSL)"),
        ];
        let got = routable_direct_addresses(
            &[sa("172.23.48.1", 5000), sa("192.168.86.44", 5000)],
            &ifaces,
        );
        assert_eq!(got, vec![sa("192.168.86.44", 5000)]);
    }

    #[test]
    fn drops_docker_and_bridge_keeps_physical() {
        let ifaces = vec![
            iface("172.17.0.1", "docker0"),
            iface("192.168.1.5", "br-abc123"),
            iface("10.0.0.7", "eth0"),
        ];
        let got = routable_direct_addresses(
            &[sa("172.17.0.1", 1), sa("192.168.1.5", 1), sa("10.0.0.7", 1)],
            &ifaces,
        );
        assert_eq!(got, vec![sa("10.0.0.7", 1)]);
    }

    #[test]
    fn keeps_physical_and_ipv6_unique_local() {
        let ifaces = vec![
            iface("192.168.86.44", "en0"),
            iface("fd00::1", "eth0"), // IPv6 unique-local — valid on a LAN
        ];
        let got = routable_direct_addresses(&[sa("192.168.86.44", 1), sa("fd00::1", 1)], &ifaces);
        assert_eq!(got, vec![sa("192.168.86.44", 1), sa("fd00::1", 1)]);
    }

    #[test]
    fn drops_unroutable_shapes_regardless_of_interface() {
        // The stateless shape check drops these before any interface scan.
        let ifaces = vec![
            iface("127.0.0.1", "lo0"),
            iface("169.254.1.1", "en0"),
            iface("fe80::1", "en0"),
        ];
        let got = routable_direct_addresses(
            &[
                sa("127.0.0.1", 1),
                sa("169.254.1.1", 1),
                sa("fe80::1", 1),
                sa("0.0.0.0", 1),
            ],
            &ifaces,
        );
        assert!(got.is_empty());
    }

    #[test]
    fn all_filtered_yields_empty_for_relay_fallback() {
        // Nothing matches a live non-virtual interface → empty vec (peers fall back
        // to the home relay). Must be a clean empty, not a panic.
        let ifaces = vec![iface("172.23.48.1", "vEthernet (WSL)")];
        let got =
            routable_direct_addresses(&[sa("172.23.48.1", 9), sa("192.168.1.216", 9)], &ifaces);
        assert!(got.is_empty());
    }

    #[test]
    fn is_virtual_iface_table() {
        for v in [
            "docker0",
            "veth1234",
            "br-abc",
            "virbr0",
            "vmnet1",
            "vboxnet0",
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
        ] {
            assert!(is_virtual_iface(v), "{v} should be virtual");
        }
        for p in [
            "en0",
            "eth0",
            "wlan0",
            "Wi-Fi",
            "Ethernet",
            "utun3",
            "tun0",
            "tailscale0",
        ] {
            assert!(!is_virtual_iface(p), "{p} should NOT be virtual");
        }
    }
}
