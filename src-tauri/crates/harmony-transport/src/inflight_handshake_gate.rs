//! ZEB-866: a bounded *concurrency* limiter for in-flight inbound handshakes.
//!
//! Unlike the sliding-window *rate* limiters guarding the post-`accept_bi`
//! path (`open_join_admit`, `friend_intro::KeyedSlidingWindow`), this bounds
//! how many handshakes are *concurrently in flight*. A permit is acquired at
//! the `MultiplexHandshakeDispatcher` chokepoint — before delegating to an
//! inner acceptor, and therefore before that acceptor's first `accept_bi`
//! await — and held, via an RAII [`InFlightGuard`], across the whole
//! handshake. This caps handler-task occupancy against a peer that opens a
//! connection and withholds its bidirectional stream (slowloris) for up to the
//! 30 s `io_deadline`.
//!
//! Two tiers:
//! - **global** — total concurrent in-flight handshakes node-wide (caps a
//!   Sybil fan-out).
//! - **per-source** — concurrent in-flight from one `remote_id` (caps a
//!   single-source flood; by limiting any one source's share of the global
//!   pool it preserves ZEB-853 B7's anti-lockout property — no single source
//!   can monopolize the node-wide ceiling).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-source concurrent in-flight cap: how many handshakes a single
/// connection `remote_id` may have in flight at once. Well above honest use
/// (1-3 concurrent), it tightly bounds a single-source stalled flood and, by
/// capping any one source's share of the global pool, preserves the
/// anti-lockout property (ZEB-853 B7).
pub const HANDSHAKE_INFLIGHT_PER_SOURCE_MAX: usize = 8;

/// Node-wide concurrent in-flight cap: total handshakes in flight across all
/// sources. Sized ~10x a heavy legitimate join surge (honest handshakes hold a
/// slot < 1 s), so it never sheds honest load, yet caps a Sybil fan-out whose
/// uncapped worst case is (attacker connection rate) x 30 s `io_deadline`.
pub const HANDSHAKE_INFLIGHT_GLOBAL_MAX: usize = 1024;

/// A two-tier concurrency limiter over connection `remote_id`s. Shared
/// node-wide as an `Arc<InFlightHandshakeGate>`.
pub(crate) struct InFlightHandshakeGate {
    inner: Mutex<InFlightState>,
    per_source_max: usize,
    global_max: usize,
}

struct InFlightState {
    /// Total in-flight handshakes across all sources.
    global: usize,
    /// In-flight count per source. Self-bounding: every entry also counts
    /// toward `global`, and entries are removed at zero, so
    /// `per_source.len() <= global <= global_max`. No separate key cap needed
    /// (contrast `friend_intro::KeyedSlidingWindow`, whose rate entries linger
    /// after a request completes and therefore need `MAX_WINDOW_KEYS`).
    per_source: HashMap<[u8; 32], usize>,
}

impl InFlightHandshakeGate {
    /// The only constructor — both caps explicit. Production passes the
    /// constants (see `MultiplexHandshakeDispatcher::new`); tests pass tiny
    /// caps. Exposing only this (no no-arg `new`) avoids
    /// `clippy::new_without_default`.
    pub(crate) fn with_caps(per_source_max: usize, global_max: usize) -> Self {
        Self {
            inner: Mutex::new(InFlightState {
                global: 0,
                per_source: HashMap::new(),
            }),
            per_source_max,
            global_max,
        }
    }

    /// Reserve one in-flight slot for `source`. Returns an [`InFlightGuard`]
    /// that releases the slot on drop, or `None` if the global OR the
    /// per-source ceiling is already reached (→ caller sheds the connection).
    ///
    /// Checks the global ceiling first (Sybil fan-out), then the per-source
    /// ceiling (single-source flood). A zero cap admits nothing.
    pub(crate) fn try_acquire(self: &Arc<Self>, source: [u8; 32]) -> Option<InFlightGuard> {
        let mut st = self.inner.lock().unwrap();
        if st.global >= self.global_max {
            return None;
        }
        if st.per_source.get(&source).copied().unwrap_or(0) >= self.per_source_max {
            return None;
        }
        st.global += 1;
        *st.per_source.entry(source).or_insert(0) += 1;
        Some(InFlightGuard {
            gate: Arc::clone(self),
            source,
        })
    }

    /// Test-only snapshot of `(global count, distinct in-flight sources)`.
    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let st = self.inner.lock().unwrap();
        (st.global, st.per_source.len())
    }
}

/// RAII slot reservation. Dropping it releases one global + one per-source
/// slot for its `source`, removing the per-source entry when it reaches zero
/// (keeping the map bounded to sources with a live in-flight handshake).
pub(crate) struct InFlightGuard {
    gate: Arc<InFlightHandshakeGate>,
    source: [u8; 32],
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // std::sync::Mutex (not tokio): Drop cannot be async, and this
        // critical section is O(1) with no await — a sync lock is correct and
        // necessary, and is never held across an await (no async stall).
        let mut st = self.gate.inner.lock().unwrap();
        st.global = st.global.saturating_sub(1);
        if let Some(c) = st.per_source.get_mut(&self.source) {
            *c -= 1;
            if *c == 0 {
                st.per_source.remove(&self.source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct 32-byte source key from a small tag.
    fn src(n: u8) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0] = n;
        s
    }

    #[test]
    fn per_source_cap_sheds_beyond_max_and_recovers_on_drop() {
        let gate = Arc::new(InFlightHandshakeGate::with_caps(2, 100));
        let a = src(1);
        let g1 = gate.try_acquire(a).expect("1st admits");
        let _g2 = gate.try_acquire(a).expect("2nd admits (at per-source cap)");
        assert!(
            gate.try_acquire(a).is_none(),
            "3rd from same source is shed"
        );
        drop(g1);
        assert!(
            gate.try_acquire(a).is_some(),
            "a slot frees for the same source once a guard drops"
        );
    }

    #[test]
    fn global_cap_sheds_new_source_under_its_per_source_cap() {
        // Per-source cap high (8) so only the GLOBAL cap (3) can bite.
        let gate = Arc::new(InFlightHandshakeGate::with_caps(8, 3));
        let _g1 = gate.try_acquire(src(1)).expect("global 1/3");
        let _g2 = gate.try_acquire(src(2)).expect("global 2/3");
        let _g3 = gate.try_acquire(src(3)).expect("global 3/3");
        // src(4) is brand-new (0 in-flight, under its per-source cap) yet the
        // node-wide ceiling is full → shed. This is the Sybil-fan-out bound.
        assert!(
            gate.try_acquire(src(4)).is_none(),
            "4th distinct source shed by the global ceiling despite being under per-source cap"
        );
    }

    #[test]
    fn dropping_all_guards_zeroes_global_and_empties_map() {
        let gate = Arc::new(InFlightHandshakeGate::with_caps(8, 100));
        let guards: Vec<_> = (0..5).map(|n| gate.try_acquire(src(n)).unwrap()).collect();
        let extra = gate.try_acquire(src(0)).unwrap(); // stack a 2nd on src(0)
        assert_eq!(
            gate.counts(),
            (6, 5),
            "6 in flight across 5 distinct sources"
        );
        drop(extra);
        drop(guards);
        assert_eq!(
            gate.counts(),
            (0, 0),
            "all slots released; the per-source map self-bounds back to empty"
        );
    }

    #[test]
    fn one_source_at_cap_does_not_lock_out_others() {
        // Anti-lockout (ZEB-853 B7): source A saturating its per-source budget
        // must not prevent source B from acquiring.
        let gate = Arc::new(InFlightHandshakeGate::with_caps(2, 100));
        let a = src(1);
        let _a1 = gate.try_acquire(a).expect("A 1/2");
        let _a2 = gate.try_acquire(a).expect("A 2/2 (at per-source cap)");
        assert!(gate.try_acquire(a).is_none(), "A is shed beyond its cap");
        assert!(
            gate.try_acquire(src(2)).is_some(),
            "B still admits — one source does not lock out others"
        );
    }

    #[test]
    fn zero_caps_admit_nothing() {
        let g_global0 = Arc::new(InFlightHandshakeGate::with_caps(8, 0));
        assert!(
            g_global0.try_acquire(src(1)).is_none(),
            "global cap 0 admits nothing"
        );
        let g_source0 = Arc::new(InFlightHandshakeGate::with_caps(0, 100));
        assert!(
            g_source0.try_acquire(src(1)).is_none(),
            "per-source cap 0 admits nothing"
        );
    }
}
