//! ZEB-592: range-based set reconciliation (RBSR) for channel-log catch-up.
//!
//! Pure protocol logic — no I/O, no crypto, no Zenoh. The channel log is
//! treated as a set of elements totally ordered by [`ReconcileKey`]; two peers
//! exchange count-folded range [`RangeFingerprint`]s, recursively bisect the
//! ranges that disagree, and (at a small leaf) ship the responder's events
//! wholesale. The crypto seal, transport, and the real log-backed
//! [`RangeReconcileSource`] live in their owning modules; this module is unit-
//! testable in isolation against the [`SliceSource`] test double.

use serde::{Deserialize, Serialize};

/// Total-order identity of one event in the reconciliation set:
/// `(wall_ms, logical, device_id, element_hash)` — the full HLC order with the
/// 32-byte element hash as a deterministic tie-breaker for the (rare) case of
/// two devices sharing `(wall_ms, logical)`.
pub type ReconcileKey = (u64, u32, String, [u8; 32]);

/// The absolute minimum key (inclusive lower sentinel for the universe).
pub fn min_key() -> ReconcileKey {
    (0, 0, String::new(), [0u8; 32])
}

/// An upper sentinel strictly greater than any real key (exclusive upper bound
/// for the universe). `wall_ms = u64::MAX` already dominates; the rest are max
/// for completeness.
pub fn max_key() -> ReconcileKey {
    (u64::MAX, u32::MAX, "\u{10FFFF}".repeat(8), [0xFFu8; 32])
}

/// Below this many events in a mismatching range the responder ships the range
/// wholesale via [`RbsrMode::Have`] instead of bisecting further.
pub const LEAF_THRESHOLD: u64 = 16;

/// Hard cap on reconciliation rounds; on exceed the driver falls back to a full
/// reconcile (`since = None`) as the safety net.
pub const MAX_RBSR_ROUNDS: u32 = 32;

/// Wire/version byte for RBSR messages.
pub const RBSR_PROTOCOL_VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Range fingerprint
// ---------------------------------------------------------------------------

/// A count-folded modular-sum fingerprint of a set of element hashes.
///
/// `raw_sum` is the sum (mod 2^256) of the element hashes as little-endian
/// integers; folding the `count` into the finalized hash defeats hash-
/// cancellation forgery (a synthetic event that negates another's hash still
/// bumps the count and breaks the match). `raw_sum`/`count` are associative, so
/// a wide range's fingerprint aggregates from its sub-ranges' summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeFingerprint {
    pub raw_sum: [u8; 32],
    pub count: u64,
}

/// `acc = (acc + add) mod 2^256` over little-endian 256-bit integers.
fn add_mod_256(acc: &mut [u8; 32], add: &[u8; 32]) {
    let mut carry = 0u16;
    for i in 0..32 {
        let s = acc[i] as u16 + add[i] as u16 + carry;
        acc[i] = (s & 0xff) as u8;
        carry = s >> 8;
    }
    // any carry out of the top byte is the mod-2^256 wrap; discard it.
}

/// Unsigned LEB128 encoding of `n`.
fn leb128(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            break;
        } else {
            out.push(b | 0x80);
        }
    }
    out
}

impl RangeFingerprint {
    pub fn zero() -> Self {
        Self {
            raw_sum: [0u8; 32],
            count: 0,
        }
    }

    pub fn fold(&mut self, element_hash: &[u8; 32]) {
        add_mod_256(&mut self.raw_sum, element_hash);
        self.count += 1;
    }

    pub fn combine(&self, other: &Self) -> Self {
        let mut raw = self.raw_sum;
        add_mod_256(&mut raw, &other.raw_sum);
        Self {
            raw_sum: raw,
            count: self.count + other.count,
        }
    }

    pub fn finalize(&self) -> [u8; 16] {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(self.raw_sum);
        h.update(leb128(self.count));
        let d = h.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&d[..16]);
        out
    }
}

// ---------------------------------------------------------------------------
// Reconcile source
// ---------------------------------------------------------------------------

/// A view over a peer's event set in canonical [`ReconcileKey`] order. All
/// ranges are half-open `[lo, hi)`. The real implementation is `ChannelLog`;
/// [`SliceSource`] is the in-memory test double.
pub trait RangeReconcileSource {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint;
    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64;
    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey>;
}

/// In-memory [`RangeReconcileSource`] over a sorted `(key, element_hash)` list.
pub struct SliceSource {
    entries: Vec<(ReconcileKey, [u8; 32])>,
}

impl SliceSource {
    pub fn from_unsorted(mut entries: Vec<(ReconcileKey, [u8; 32])>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        Self { entries }
    }

    fn slice(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> &[(ReconcileKey, [u8; 32])] {
        let s = self.entries.partition_point(|x| &x.0 < lo);
        let e = self.entries.partition_point(|x| &x.0 < hi);
        &self.entries[s..e]
    }
}

impl RangeReconcileSource for SliceSource {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint {
        let mut f = RangeFingerprint::zero();
        for (_, h) in self.slice(lo, hi) {
            f.fold(h);
        }
        f
    }

    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64 {
        self.slice(lo, hi).len() as u64
    }

    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey> {
        self.slice(lo, hi).iter().map(|x| x.0.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// What one range in an RBSR message asserts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RbsrMode {
    /// Ranges agree; nothing to do (responder → requester).
    #[serde(rename = "s")]
    Skip,
    /// "My view of this range hashes to X" (request → responder, and bisected
    /// sub-ranges responder → requester).
    #[serde(rename = "f")]
    Fingerprint([u8; 16]),
    /// The responder's element keys in this (small, leaf) range — the requester
    /// fetches the events it lacks (responder → requester).
    #[serde(rename = "h")]
    Have(Vec<ReconcileKey>),
}

/// One range `[prev_upper, upper)` of a message; ranges partition the universe
/// in ascending order, the first implicitly starting at [`min_key`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbsrRange {
    #[serde(rename = "u")]
    pub upper: ReconcileKey,
    #[serde(rename = "m")]
    pub mode: RbsrMode,
}

/// A full RBSR round message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbsrMessage {
    #[serde(rename = "v")]
    pub version: u8,
    #[serde(rename = "r")]
    pub ranges: Vec<RbsrRange>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RbsrError {
    Decode,
}

pub fn encode_message(m: &RbsrMessage) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::into_writer(m, &mut v).expect("RbsrMessage cbor encode into Vec is infallible");
    v
}

pub fn decode_message(bytes: &[u8]) -> Result<RbsrMessage, RbsrError> {
    ciborium::from_reader(bytes).map_err(|_| RbsrError::Decode)
}

// ---------------------------------------------------------------------------
// Protocol state machine (pull-only)
// ---------------------------------------------------------------------------

/// Append a range to a message under construction, coalescing adjacent `Skip`
/// spans into one. Both `respond` and `process_reply` emit a **full ordered
/// partition** of the universe so the receiver's positional lower-bound chain
/// (`lo` advancing by each range's `upper`) stays aligned; coalescing keeps the
/// resolved prefix/suffix from bloating the message, bounding it to
/// O(diff · log n).
fn push_range(out: &mut Vec<RbsrRange>, upper: ReconcileKey, mode: RbsrMode) {
    if mode == RbsrMode::Skip {
        if let Some(last) = out.last_mut() {
            if last.mode == RbsrMode::Skip {
                last.upper = upper;
                return;
            }
        }
    }
    out.push(RbsrRange { upper, mode });
}

/// Round 0: one `Fingerprint` over the whole universe `[min_key, max_key)`.
pub fn initial_request(source: &impl RangeReconcileSource) -> RbsrMessage {
    RbsrMessage {
        version: RBSR_PROTOCOL_VERSION,
        ranges: vec![RbsrRange {
            upper: max_key(),
            mode: RbsrMode::Fingerprint(source.range_fingerprint(&min_key(), &max_key()).finalize()),
        }],
    }
}

/// Responder half. For each incoming `Fingerprint` range: `Skip` if it matches
/// the responder's own fingerprint, else bisect (above the leaf threshold) or
/// ship the responder's keys wholesale via `Have`. Already-resolved ranges
/// (`Skip`/`Have` in the request) are echoed as `Skip` to preserve the
/// partition.
pub fn respond(request: &RbsrMessage, source: &impl RangeReconcileSource) -> RbsrMessage {
    let mut out: Vec<RbsrRange> = Vec::new();
    let mut lo = min_key();
    for range in &request.ranges {
        let hi = range.upper.clone();
        match &range.mode {
            RbsrMode::Fingerprint(their_fp) => {
                let mine = source.range_fingerprint(&lo, &hi).finalize();
                if &mine == their_fp {
                    push_range(&mut out, hi.clone(), RbsrMode::Skip);
                } else if source.range_count(&lo, &hi) <= LEAF_THRESHOLD {
                    push_range(&mut out, hi.clone(), RbsrMode::Have(source.keys_in_range(&lo, &hi)));
                } else {
                    let keys = source.keys_in_range(&lo, &hi);
                    let mid = keys[keys.len() / 2].clone();
                    push_range(
                        &mut out,
                        mid.clone(),
                        RbsrMode::Fingerprint(source.range_fingerprint(&lo, &mid).finalize()),
                    );
                    push_range(
                        &mut out,
                        hi.clone(),
                        RbsrMode::Fingerprint(source.range_fingerprint(&mid, &hi).finalize()),
                    );
                }
            }
            RbsrMode::Skip | RbsrMode::Have(_) => {
                push_range(&mut out, hi.clone(), RbsrMode::Skip);
            }
        }
        lo = hi;
    }
    RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: out }
}

/// Requester half. Ingest `Have` keys we lack, recompute our own fingerprint
/// over each still-`Fingerprint` sub-range, and emit the next full partition
/// (matched/resolved ranges → `Skip`). Returns `(keys_we_are_missing,
/// next_request)`; `next_request` is `None` once nothing mismatches (converged).
pub fn process_reply(
    reply: &RbsrMessage,
    source: &impl RangeReconcileSource,
) -> (Vec<ReconcileKey>, Option<RbsrMessage>) {
    let mut missing = Vec::new();
    let mut next: Vec<RbsrRange> = Vec::new();
    let mut any_mismatch = false;
    let mut lo = min_key();
    for range in &reply.ranges {
        let hi = range.upper.clone();
        match &range.mode {
            RbsrMode::Skip => push_range(&mut next, hi.clone(), RbsrMode::Skip),
            RbsrMode::Have(keys) => {
                let local: std::collections::HashSet<ReconcileKey> =
                    source.keys_in_range(&lo, &hi).into_iter().collect();
                for k in keys {
                    if !local.contains(k) {
                        missing.push(k.clone());
                    }
                }
                push_range(&mut next, hi.clone(), RbsrMode::Skip); // resolved after ingest
            }
            RbsrMode::Fingerprint(their_fp) => {
                let mine = source.range_fingerprint(&lo, &hi).finalize();
                if &mine == their_fp {
                    push_range(&mut next, hi.clone(), RbsrMode::Skip);
                } else {
                    any_mismatch = true;
                    push_range(&mut next, hi.clone(), RbsrMode::Fingerprint(mine));
                }
            }
        }
        lo = hi;
    }
    let next = if any_mismatch {
        Some(RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: next })
    } else {
        None
    };
    (missing, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> [u8; 32] {
        let mut x = [0u8; 32];
        x[0] = n;
        x
    }
    fn key(w: u64, hb: u8) -> ReconcileKey {
        (w, 0, "d".into(), h(hb))
    }

    // ---- Task 2: fingerprint ----

    #[test]
    fn fingerprint_is_order_independent_and_associative() {
        let hashes = [h(1), h(2), h(3), h(4)];
        let mut all = RangeFingerprint::zero();
        for x in &hashes {
            all.fold(x);
        }
        let mut rev = RangeFingerprint::zero();
        for x in hashes.iter().rev() {
            rev.fold(x);
        }
        assert_eq!(all.finalize(), rev.finalize(), "sum is commutative");

        let mut a = RangeFingerprint::zero();
        a.fold(&hashes[0]);
        a.fold(&hashes[1]);
        let mut b = RangeFingerprint::zero();
        b.fold(&hashes[2]);
        b.fold(&hashes[3]);
        assert_eq!(a.combine(&b).finalize(), all.finalize(), "combine == fold all");
        assert_eq!(a.combine(&b).count, 4);
    }

    #[test]
    fn count_fold_breaks_match_on_synthetic_cancellation() {
        let mut one = RangeFingerprint::zero();
        one.fold(&h(5));
        let mut two = RangeFingerprint::zero();
        two.fold(&h(2));
        two.fold(&h(3)); // 2 + 3 == 5 in the first byte → raw_sum collides
        assert_eq!(one.raw_sum, two.raw_sum, "raw sums collide by construction");
        assert_ne!(one.finalize(), two.finalize(), "count fold must distinguish them");
    }

    // ---- Task 3: source ----

    fn sample() -> SliceSource {
        SliceSource::from_unsorted(
            [key(10, 1), key(20, 2), key(30, 3), key(40, 4)]
                .into_iter()
                .map(|k| {
                    let hash = k.3;
                    (k, hash)
                })
                .collect(),
        )
    }

    #[test]
    fn range_count_and_keys_are_half_open() {
        let s = sample();
        assert_eq!(s.range_count(&key(10, 1), &key(40, 4)), 3, "[10,40) excludes 40");
        assert_eq!(
            s.keys_in_range(&key(10, 1), &key(30, 3)),
            vec![key(10, 1), key(20, 2)]
        );
        assert_eq!(s.range_count(&min_key(), &max_key()), 4, "whole universe");
    }

    #[test]
    fn range_fingerprint_matches_manual_fold() {
        let s = sample();
        let mut expect = RangeFingerprint::zero();
        expect.fold(&key(20, 2).3);
        expect.fold(&key(30, 3).3);
        assert_eq!(
            s.range_fingerprint(&key(20, 2), &key(40, 4)).finalize(),
            expect.finalize()
        );
    }

    // ---- Task 4: messages ----

    #[test]
    fn message_cbor_round_trips() {
        let m = RbsrMessage {
            version: RBSR_PROTOCOL_VERSION,
            ranges: vec![
                RbsrRange { upper: key(10, 1), mode: RbsrMode::Skip },
                RbsrRange { upper: key(20, 2), mode: RbsrMode::Fingerprint([7u8; 16]) },
                RbsrRange { upper: key(30, 3), mode: RbsrMode::Have(vec![key(21, 9), key(22, 9)]) },
            ],
        };
        let bytes = encode_message(&m);
        assert_eq!(decode_message(&bytes).unwrap(), m);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(decode_message(&[0xff, 0x00, 0x13]), Err(RbsrError::Decode));
    }

    // ---- Tasks 6 & 7: protocol ----

    fn src(ws: &[(u64, u8)]) -> SliceSource {
        SliceSource::from_unsorted(
            ws.iter()
                .map(|&(w, b)| {
                    let k = key(w, b);
                    let hash = k.3;
                    (k, hash)
                })
                .collect(),
        )
    }

    #[test]
    fn matching_whole_range_returns_skip() {
        let s = src(&[(10, 1), (20, 2), (30, 3)]);
        let reply = respond(&initial_request(&s), &s);
        assert_eq!(reply.ranges.len(), 1);
        assert_eq!(reply.ranges[0].mode, RbsrMode::Skip);
    }

    #[test]
    fn small_mismatch_returns_have_wholesale() {
        let responder = src(&[(10, 1), (20, 2), (30, 3)]);
        let requester = src(&[(10, 1), (30, 3)]);
        let reply = respond(&initial_request(&requester), &responder);
        let have: Vec<ReconcileKey> = reply
            .ranges
            .iter()
            .filter_map(|r| if let RbsrMode::Have(ks) = &r.mode { Some(ks.clone()) } else { None })
            .flatten()
            .collect();
        assert!(have.contains(&key(20, 2)), "responder ships its events wholesale at the leaf");
    }

    #[test]
    fn large_mismatch_bisects_into_fingerprints() {
        let resp: Vec<(u64, u8)> = (0..40u64).map(|i| (i * 10, i as u8)).collect();
        let responder = src(&resp);
        let mut req_set = resp.clone();
        req_set.remove(20);
        let requester = src(&req_set);
        let reply = respond(&initial_request(&requester), &responder);
        assert!(reply.ranges.len() >= 2, "bisected: {:?}", reply.ranges.len());
        assert!(reply
            .ranges
            .iter()
            .all(|r| matches!(r.mode, RbsrMode::Fingerprint(_) | RbsrMode::Skip)));
    }

    /// Drive requester ↔ responder to convergence; return (rounds, keys acquired).
    fn reconcile(req: &SliceSource, resp: &SliceSource) -> (u32, usize) {
        let mut request = initial_request(req);
        let mut acquired: Vec<ReconcileKey> = Vec::new();
        let mut rounds = 0u32;
        loop {
            rounds += 1;
            assert!(rounds <= MAX_RBSR_ROUNDS, "must converge within the round cap");
            let reply = respond(&request, resp);
            let (missing, next) = process_reply(&reply, req);
            acquired.extend(missing);
            match next {
                None => break,
                Some(n) => request = n,
            }
        }
        (rounds, acquired.len())
    }

    #[test]
    fn converges_and_transfers_only_the_gap() {
        let full: Vec<(u64, u8)> = (0..100u64).map(|i| (i, (i % 251) as u8)).collect();
        let resp = src(&full);
        let mut miss = full.clone();
        for idx in [70usize, 40, 5] {
            miss.remove(idx);
        }
        let req = src(&miss);
        let (rounds, have) = reconcile(&req, &resp);
        assert!(rounds <= MAX_RBSR_ROUNDS, "rounds {rounds}");
        assert!(have >= 3 && have < 40, "transferred ~gap not history: {have}");
    }

    #[test]
    fn identical_sets_converge_in_one_round_with_no_transfer() {
        let s = src(&[(1, 1), (2, 2), (3, 3)]);
        assert_eq!(reconcile(&s, &s), (1, 0));
    }

    #[test]
    fn requester_with_extra_events_still_converges_pull_only() {
        // requester holds events the responder lacks; pull-only → no transfer, converges.
        let responder = src(&[(10, 1), (30, 3)]);
        let requester = src(&[(10, 1), (20, 2), (30, 3), (40, 4)]);
        let (rounds, have) = reconcile(&requester, &responder);
        assert!(rounds <= MAX_RBSR_ROUNDS);
        assert_eq!(have, 0, "requester already holds everything the responder has");
    }
}
