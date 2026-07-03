//! ZEB-621: address-delta fan-out hub.
//!
//! The reachability publisher fires its `publish_fn` on every publish tick —
//! the idle backstop, a forced republish, and (since Task 3) within ~2s of any
//! iroh address change fed by `watch_addr_stream`. Most of those ticks carry the
//! SAME self-address; only a subset represent an ACTUAL change to this device's
//! reachability (home relay flap or a direct-address set change).
//!
//! `AddrChangeFanout` sits inside `publish_fn` and converts "publish tick" into
//! "self-address actually changed": it snapshots `(home_relay, direct_addrs)`
//! and, on a delta versus the last observation, fires two install-once hooks:
//!
//!   1. `pkarr_republish` — re-registers this device's pkarr identity + per-
//!      community routing slots immediately (a re-register forces an immediate
//!      republish of a freshly-built blob), instead of waiting the ~3.5-day
//!      epoch schedule for the record to naturally refresh under the new
//!      address.
//!   2. `supervisor_sweep` — kicks the reconnect supervisor to re-arm every
//!      known non-connected peer, so a local address change prompts an immediate
//!      reconnection sweep rather than waiting on the next idle cycle.
//!
//! The FIRST observation (the boot publish) records the snapshot and does NOT
//! fire: the boot paths already register every pkarr slot and seed the
//! supervisor, so firing on boot would be redundant work. Only genuine
//! post-boot changes fan out.
//!
//! `observe` is sync and non-blocking (it locks a `Mutex` only to compare and
//! swap the snapshot, releasing it before invoking any hook). Both hooks are
//! plain `Fn()` closures because their targets are all sync: the pkarr
//! re-register wrapper spawns its own async work, and `SupervisorHandle::
//! kick_sweep` is a non-async atomic store + notify.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

/// A sync, `Send + Sync` side-effect fired on a real address change.
type Hook = Box<dyn Fn() + Send + Sync>;

/// The self-address fingerprint compared across publish ticks. Two ticks are
/// "the same address" iff the home relay and the (order-insensitive) direct-
/// address set are equal.
#[derive(PartialEq, Eq)]
struct AddrSnapshot {
    home_relay: Option<String>,
    direct_addrs: BTreeSet<SocketAddr>,
}

/// Delta-gated fan-out hub. Constructed once per node boot, cloned into
/// `publish_fn` (which calls [`AddrChangeFanout::observe`]) and into the two
/// hook-install sites.
pub struct AddrChangeFanout {
    last: Mutex<Option<AddrSnapshot>>,
    pkarr_republish: OnceLock<Hook>,
    supervisor_sweep: OnceLock<Hook>,
}

impl AddrChangeFanout {
    /// Build a fresh hub with no snapshot recorded and no hooks installed.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last: Mutex::new(None),
            pkarr_republish: OnceLock::new(),
            supervisor_sweep: OnceLock::new(),
        })
    }

    /// Install the pkarr slot re-register hook (install-once; later calls are
    /// ignored). Fired on a real address change.
    pub fn set_pkarr_republish(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.pkarr_republish.set(f);
    }

    /// Install the supervisor sweep hook (install-once; later calls are
    /// ignored). Fired on a real address change.
    pub fn set_supervisor_sweep(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.supervisor_sweep.set(f);
    }

    /// Record `(home_relay, direct_addrs)` and compare against the previous
    /// observation. Returns `true` (and fires both installed hooks exactly
    /// once) iff this is a CHANGE relative to a prior snapshot. The very first
    /// observation records the boot snapshot and returns `false` without
    /// firing. Missing hooks are skipped gracefully (no panic).
    ///
    /// The comparison/swap is done under the `Mutex`; the guard is released
    /// BEFORE any hook runs, so the hub never holds its lock across a hook
    /// (keeping `observe` non-blocking with respect to the hook bodies).
    pub fn observe(&self, home_relay: Option<String>, direct_addrs: BTreeSet<SocketAddr>) -> bool {
        let next = AddrSnapshot {
            home_relay,
            direct_addrs,
        };
        let changed = {
            let mut guard = self.last.lock().unwrap_or_else(|p| p.into_inner());
            match guard.as_ref() {
                // First observation (boot publish): record, do not fire.
                None => {
                    *guard = Some(next);
                    false
                }
                // Unchanged self-address: a redundant publish tick, no fan-out.
                Some(prev) if *prev == next => false,
                // Real change: swap in the new snapshot and fan out.
                Some(_) => {
                    *guard = Some(next);
                    true
                }
            }
        };
        if changed {
            if let Some(f) = self.pkarr_republish.get() {
                f();
            }
            if let Some(f) = self.supervisor_sweep.get() {
                f();
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;

    fn addrs(list: &[&str]) -> BTreeSet<SocketAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// Wire counters into both hooks; return (fanout, pkarr_count, sweep_count).
    fn counted() -> (Arc<AddrChangeFanout>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let f = AddrChangeFanout::new();
        let pk = Arc::new(AtomicUsize::new(0));
        let sw = Arc::new(AtomicUsize::new(0));
        let pk2 = Arc::clone(&pk);
        let sw2 = Arc::clone(&sw);
        f.set_pkarr_republish(Box::new(move || {
            pk2.fetch_add(1, SeqCst);
        }));
        f.set_supervisor_sweep(Box::new(move || {
            sw2.fetch_add(1, SeqCst);
        }));
        (f, pk, sw)
    }

    #[test]
    fn first_observation_records_but_does_not_fire() {
        let (f, pk, sw) = counted();
        let fired = f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
        assert!(!fired);
        assert_eq!(pk.load(SeqCst), 0);
        assert_eq!(sw.load(SeqCst), 0);
    }

    #[test]
    fn relay_change_fires_both_hooks_once() {
        let (f, pk, sw) = counted();
        f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
        let fired = f.observe(Some("https://relay.b/".into()), addrs(&["10.0.0.1:4433"]));
        assert!(fired);
        assert_eq!(pk.load(SeqCst), 1);
        assert_eq!(sw.load(SeqCst), 1);
        // Same snapshot again: no re-fire.
        let fired = f.observe(Some("https://relay.b/".into()), addrs(&["10.0.0.1:4433"]));
        assert!(!fired);
        assert_eq!(pk.load(SeqCst), 1);
        assert_eq!(sw.load(SeqCst), 1);
    }

    #[test]
    fn direct_addr_set_change_fires() {
        let (f, pk, _sw) = counted();
        f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
        let fired = f.observe(
            Some("https://relay.a/".into()),
            addrs(&["10.0.0.1:4433", "192.168.1.5:4433"]),
        );
        assert!(fired);
        assert_eq!(pk.load(SeqCst), 1);
    }

    #[test]
    fn addr_set_equality_is_order_insensitive() {
        let (f, pk, _sw) = counted();
        f.observe(None, addrs(&["10.0.0.1:4433", "192.168.1.5:4433"]));
        // Same members, built in the opposite order → BTreeSet equality → no fire.
        let fired = f.observe(None, addrs(&["192.168.1.5:4433", "10.0.0.1:4433"]));
        assert!(!fired);
        assert_eq!(pk.load(SeqCst), 0);
    }

    #[test]
    fn change_with_no_hooks_installed_returns_true_without_panic() {
        let f = AddrChangeFanout::new();
        f.observe(Some("https://relay.a/".into()), addrs(&[]));
        assert!(f.observe(Some("https://relay.b/".into()), addrs(&[])));
    }
}
