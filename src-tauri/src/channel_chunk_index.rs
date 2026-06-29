//! ZEB-592 Slice 2: an in-memory content-defined-chunk fingerprint index that
//! makes [`crate::channel_rbsr::RangeReconcileSource::range_fingerprint`]
//! disk-bounded.
//!
//! The channel log is not stored in canonical order on disk, so computing a
//! range fingerprint naively re-reads every segment. This index caches, per
//! content-defined chunk, the associative `(raw_sum, count)` summary
//! ([`ChunkSummary`]). A range fingerprint aggregates the summaries of the
//! chunks fully inside the range and reads only the (≤2) partial boundary
//! chunks' events from disk — so per-query disk is O(chunk), not O(history).
//!
//! Chunk boundaries are content-defined (a function of each element's hash, not
//! its position), which makes them **insert-stable**: appending one event
//! re-chunks only the single chunk it lands in. Boundaries are never sent on
//! the wire (only fingerprint *results* are), so they need not be identical
//! across peers — this is a purely local acceleration structure.

use crate::channel_rbsr::{RangeFingerprint, ReconcileKey};

/// Reads a chunk's events for its inclusive `[first, last]` span — supplied by
/// the caller (the log reads segments/tail; tests read an in-memory vec).
type ChunkEventReader<'a> = dyn FnMut(&ReconcileKey, &ReconcileKey) -> Vec<(ReconcileKey, [u8; 32])> + 'a;

/// Average chunk size ≈ `2^CHUNK_MASK_BITS` events. A boundary falls on an
/// element whose hash's low `CHUNK_MASK_BITS` bits are zero.
pub const CHUNK_MASK_BITS: u32 = 8;

/// True when `element_hash` ends a content-defined chunk.
fn is_boundary(element_hash: &[u8; 32]) -> bool {
    let v = u64::from_le_bytes(element_hash[..8].try_into().unwrap());
    (v & ((1u64 << CHUNK_MASK_BITS) - 1)) == 0
}

/// Associative summary of one chunk's events in canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkSummary {
    pub first: ReconcileKey,
    pub last: ReconcileKey,
    pub count: u64,
    pub raw_sum: [u8; 32],
}

/// A sorted list of [`ChunkSummary`]s partitioning the event set in canonical
/// order.
#[derive(Debug, Default)]
pub struct ChunkIndex {
    chunks: Vec<ChunkSummary>,
}

impl ChunkIndex {
    pub fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    /// Build from events pre-sorted ascending by [`ReconcileKey`].
    pub fn build_from_sorted(entries: &[(ReconcileKey, [u8; 32])]) -> Self {
        let mut chunks = Vec::new();
        let mut acc = RangeFingerprint::zero();
        let mut first: Option<ReconcileKey> = None;
        let mut last: Option<ReconcileKey> = None;
        for (key, hash) in entries {
            if first.is_none() {
                first = Some(key.clone());
            }
            acc.fold(hash);
            last = Some(key.clone());
            if is_boundary(hash) {
                chunks.push(ChunkSummary {
                    first: first.take().unwrap(),
                    last: last.take().unwrap(),
                    count: acc.count,
                    raw_sum: acc.raw_sum,
                });
                acc = RangeFingerprint::zero();
            }
        }
        if let Some(f) = first {
            chunks.push(ChunkSummary {
                first: f,
                last: last.unwrap(),
                count: acc.count,
                raw_sum: acc.raw_sum,
            });
        }
        Self { chunks }
    }

    /// Chunk upper bounds, for parity assertions in tests.
    pub fn boundaries(&self) -> Vec<ReconcileKey> {
        self.chunks.iter().map(|c| c.last.clone()).collect()
    }

    /// Fingerprint over the half-open canonical range `[lo, hi)`. Fully-
    /// contained chunks aggregate from their cached summaries; the (≤2) partial
    /// boundary chunks are folded from their events via `boundary_events`, which
    /// returns a chunk's events given its inclusive `[first, last]` span.
    pub fn range_fingerprint(
        &self,
        lo: &ReconcileKey,
        hi: &ReconcileKey,
        boundary_events: &mut ChunkEventReader<'_>,
    ) -> RangeFingerprint {
        let mut agg = RangeFingerprint::zero();
        for c in &self.chunks {
            if &c.last < lo || &c.first >= hi {
                continue; // chunk entirely outside [lo, hi)
            }
            if lo <= &c.first && &c.last < hi {
                agg = agg.combine(&RangeFingerprint {
                    raw_sum: c.raw_sum,
                    count: c.count,
                });
            } else {
                for (k, h) in boundary_events(&c.first, &c.last) {
                    if &k >= lo && &k < hi {
                        agg.fold(&h);
                    }
                }
            }
        }
        agg
    }

    /// Insert one element, re-chunking only the single chunk it lands in (read
    /// via `chunk_events`, which returns a chunk's events for its inclusive
    /// `[first, last]` span). Insert-stable boundaries keep this local and keep
    /// the result identical to [`Self::build_from_sorted`] over the new set.
    pub fn insert(
        &mut self,
        key: ReconcileKey,
        hash: [u8; 32],
        chunk_events: impl Fn(&ReconcileKey, &ReconcileKey) -> Vec<(ReconcileKey, [u8; 32])>,
    ) {
        if self.chunks.is_empty() {
            *self = ChunkIndex::build_from_sorted(&[(key, hash)]);
            return;
        }
        let mut idx = self.chunks.partition_point(|c| c.last < key);
        if idx == self.chunks.len() {
            idx -= 1; // key sorts after every chunk → extend the last chunk
        }
        let c = &self.chunks[idx];
        let mut window = chunk_events(&c.first, &c.last);
        window.push((key, hash));
        window.sort_by(|a, b| a.0.cmp(&b.0));
        window.dedup_by(|a, b| a.0 == b.0);
        let rebuilt = ChunkIndex::build_from_sorted(&window);
        self.chunks.splice(idx..idx + 1, rebuilt.chunks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_rbsr::{max_key, min_key, RangeReconcileSource, ReconcileKey, SliceSource};

    fn entry(w: u64, hb: u8) -> (ReconcileKey, [u8; 32]) {
        let mut h = [0u8; 32];
        h[0] = hb;
        h[1] = w as u8;
        ((w, 0, "d".into(), h), h)
    }

    fn sorted(n: u64) -> Vec<(ReconcileKey, [u8; 32])> {
        let mut v: Vec<_> = (0..n).map(|i| entry(i, (i % 251) as u8)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn chunk_boundaries_are_content_defined() {
        let s = sorted(2000);
        let a = ChunkIndex::build_from_sorted(&s);
        // boundaries fall exactly on the boundary-hash elements regardless of
        // how the (already-sorted) input was produced.
        let mut s2 = s.clone();
        s2.reverse();
        s2.sort_by(|x, y| x.0.cmp(&y.0));
        let b = ChunkIndex::build_from_sorted(&s2);
        assert_eq!(a.boundaries(), b.boundaries());
        assert!(a.chunks.len() > 1, "expected multiple chunks, got {}", a.chunks.len());
    }

    #[test]
    fn range_fingerprint_matches_naive_over_same_set() {
        let s = sorted(2000);
        let idx = ChunkIndex::build_from_sorted(&s);
        let naive = SliceSource::from_unsorted(s.clone());
        let mut lookup = |lo: &ReconcileKey, hi: &ReconcileKey| -> Vec<(ReconcileKey, [u8; 32])> {
            s.iter().filter(|(k, _)| k >= lo && k <= hi).cloned().collect()
        };
        let lo = s[300].0.clone();
        let hi = s[1700].0.clone();
        assert_eq!(
            idx.range_fingerprint(&lo, &hi, &mut lookup).finalize(),
            naive.range_fingerprint(&lo, &hi).finalize()
        );
        assert_eq!(
            idx.range_fingerprint(&min_key(), &max_key(), &mut lookup).finalize(),
            naive.range_fingerprint(&min_key(), &max_key()).finalize()
        );
    }

    #[test]
    fn incremental_insert_equals_rebuild() {
        let mut base = sorted(1500);
        let mut pulled = Vec::new();
        for i in (0..base.len()).step_by(7).rev() {
            pulled.push(base.remove(i));
        }
        let mut idx = ChunkIndex::build_from_sorted(&base);
        let mut running = base.clone();
        for (k, h) in pulled.iter().cloned() {
            let snapshot = running.clone();
            idx.insert(k.clone(), h, |lo, hi| {
                snapshot.iter().filter(|(kk, _)| kk >= lo && kk <= hi).cloned().collect()
            });
            let pos = running.partition_point(|(kk, _)| kk < &k);
            running.insert(pos, (k, h));
        }

        let mut full = base.clone();
        full.extend(pulled);
        full.sort_by(|a, b| a.0.cmp(&b.0));
        full.dedup_by(|a, b| a.0 == b.0);
        let rebuilt = ChunkIndex::build_from_sorted(&full);

        assert_eq!(idx.boundaries(), rebuilt.boundaries(), "insert path must equal rebuild");
        let mut lk = |lo: &ReconcileKey, hi: &ReconcileKey| {
            full.iter().filter(|(k, _)| k >= lo && k <= hi).cloned().collect::<Vec<_>>()
        };
        assert_eq!(
            idx.range_fingerprint(&min_key(), &max_key(), &mut lk).finalize(),
            rebuilt.range_fingerprint(&min_key(), &max_key(), &mut lk).finalize()
        );
    }
}
