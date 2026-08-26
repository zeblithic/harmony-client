//! ZEB-812: spill-forwarding between a zenoh query-reply drain and a bounded
//! engine channel.
//!
//! The invariant this type enforces: **draining zenoh's reply stream must
//! never await application backpressure.** A reply-drain loop that calls
//! `subscriber_tx.send(bytes).await` inside its reply arm holds zenoh's reply
//! channel hostage while it waits: the engine consumer slows → the bounded
//! engine channel fills → the drain parks in `send().await` → nothing pulls
//! from zenoh's reply channel → zenoh's net thread parks in
//! `flume wait_send<Reply>` — and that single net thread services the entire
//! session, so one slow channel consumer wedges the node's whole zenoh
//! transport (the ZEB-803 stall). The parked await also starves the drain
//! loop's own closing-poll select arm, making `stop()` latency unbounded.
//!
//! The replacement contract, in two phases:
//!
//! 1. **While the zenoh stream is open** — [`ReplySpill::accept`] each drained
//!    payload. It buffers locally and forwards with `try_send` only; it never
//!    awaits. The buffer carries an explicit `max_pending` cap: one GET's
//!    reply volume is `limit × responders`, and with
//!    `ConsolidationMode::None` the responder count is NOT bounded by the
//!    protocol (a large community can answer a backfill GET from every
//!    member — the ZEB-803 measurement was one page × 3 peers). Overflow
//!    beyond the cap is **dropped and counted** ([`ReplySpill::dropped`]),
//!    never blocked on: every spill-backed path is an anti-entropy loop
//!    (RBSR / periodic full reconcile / next backfill round), so a dropped
//!    pre-verification packet is re-fetched later, while blocking is the
//!    ZEB-803 wedge itself. Callers surface the drop count loudly.
//! 2. **After the stream closes** — [`ReplySpill::flush`] delivers whatever
//!    the consumer hasn't absorbed yet. Blocking on the engine *here* is the
//!    natural request-level backpressure point (the driver won't take its
//!    next request until the page lands), and the closing flag stays live:
//!    it is checked at the top of every iteration AND while parked, because
//!    the send-permit acquisition is a cancel-safe `reserve()` select ARM,
//!    not an await buried inside an arm body. Shutdown never reports a
//!    completed flush (PR #559 review).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

/// Per-payload result of [`ReplySpill::accept`].
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub enum AcceptOutcome {
    /// Forwarded to the engine channel immediately, or buffered within the
    /// cap for the post-drain flush.
    Accepted,
    /// The spill is at `max_pending` and the channel is full: the payload
    /// was dropped (and counted). Safe by design on these paths — the next
    /// anti-entropy round re-fetches it; blocking instead would recreate
    /// the ZEB-803 wedge.
    DroppedFull,
    /// The engine receiver is gone (engine teardown); the caller should
    /// exit its drive loop (same semantics as a failed `send` before
    /// ZEB-812).
    ConsumerGone,
}

/// Terminal state of a [`ReplySpill::flush`].
#[derive(Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Every buffered payload was delivered to the engine channel.
    Flushed,
    /// The engine receiver is gone (engine teardown); remaining payloads
    /// were dropped with it. Callers should exit their drive loop.
    ConsumerGone,
    /// The closing flag was (or flipped) set while payloads remained;
    /// they were abandoned, matching the pre-existing no-report shutdown
    /// semantics of the drains this serves.
    ShutdownAbandoned,
}

/// FIFO spill buffer in front of a bounded `mpsc::Sender<Vec<u8>>`.
pub struct ReplySpill {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    buf: VecDeque<Vec<u8>>,
    max_pending: usize,
    dropped: usize,
    peak: usize,
}

impl ReplySpill {
    /// `max_pending` bounds the buffer (payload count, not bytes — the
    /// payloads on these paths are already individually size-capped by
    /// their call sites). Size it as a generous multiple of the page
    /// `limit` so only a genuine reply storm (many simultaneous
    /// responders) ever trips it.
    pub fn new(tx: tokio::sync::mpsc::Sender<Vec<u8>>, max_pending: usize) -> Self {
        Self {
            tx,
            buf: VecDeque::new(),
            max_pending,
            dropped: 0,
            peak: 0,
        }
    }

    /// Accept one drained reply payload: forward as much as fits RIGHT NOW
    /// (`try_send` only — never awaits, so the caller's reply-drain loop
    /// never blocks on the engine), buffering the remainder up to
    /// `max_pending`.
    pub fn accept(&mut self, bytes: Vec<u8>) -> AcceptOutcome {
        if self.buf.len() >= self.max_pending {
            // At the cap: record the high-water (== max_pending) so peak()
            // reflects that we hit the ceiling even on a run whose overflow
            // was later re-fetched. Then drop, then still try to make room
            // for the NEXT reply.
            self.peak = self.peak.max(self.buf.len());
            self.dropped = self.dropped.saturating_add(1);
            if !self.try_flush() {
                return AcceptOutcome::ConsumerGone;
            }
            return AcceptOutcome::DroppedFull;
        }
        self.buf.push_back(bytes);
        if !self.try_flush() {
            return AcceptOutcome::ConsumerGone;
        }
        // Sample AFTER try_flush, so peak reflects the backlog actually
        // RETAINED under backpressure — not the transient +1 from a push that
        // try_flush forwards in the same call. A run whose channel always had
        // capacity leaves buf empty every accept, so peak stays 0: the "spill
        // never exercised" signal peak()'s contract promises (sampling before
        // the flush made peak==0 unreachable after the first accept — Qodo,
        // PR #570). The overflow branch above is the one place we sample
        // pre-flush, because sitting AT max_pending on entry is a sustained
        // cap-full state (prior accepts' flushes couldn't drain it), a real
        // high-water rather than a transient.
        self.peak = self.peak.max(self.buf.len());
        AcceptOutcome::Accepted
    }

    /// Forward buffered payloads until the channel is full or the buffer is
    /// empty. Returns `false` on a closed channel.
    fn try_flush(&mut self) -> bool {
        while let Some(bytes) = self.buf.pop_front() {
            match self.tx.try_send(bytes) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(bytes)) => {
                    self.buf.push_front(bytes);
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        true
    }

    /// Number of payloads still waiting in the spill.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Payloads dropped at the cap so far. Callers surface a non-zero
    /// count loudly after the drain — a drop is recoverable (anti-entropy
    /// re-fetches) but should never be invisible.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// High-water mark of buffered payloads (`pending()` at its largest)
    /// over this spill's lifetime. ZEB-816: `dropped() == 0` alone is
    /// over-determined — it collapses "comfortable headroom" (peak well
    /// under `max_pending`), "near-miss" (peak approaching it), and "the
    /// spill was never exercised at all" (peak 0 — `try_send` always had
    /// capacity, so a clean run proves nothing about the ZEB-812 stall
    /// path). `peak()` separates them, so the cap can be sized from field
    /// data rather than from the estimate in its doc comment. Sampled in
    /// `accept()`; `flush()` only drains, so it never raises the peak.
    pub fn peak(&self) -> usize {
        self.peak
    }

    /// Post-drain delivery of everything still buffered, in order. Call this
    /// ONLY after the zenoh reply stream has closed — blocking on the engine
    /// here is harmless to the transport and is the intended request-level
    /// backpressure. The `closing` flag is honored at the top of every
    /// iteration (a shutdown never reports `Flushed`, even when the
    /// consumer has capacity — PR #559 review) and polled every 500ms while
    /// parked: `reserve()` is a cancel-safe select arm, so no payload is
    /// lost when the poll arm wins.
    pub async fn flush(mut self, closing: &AtomicBool) -> FlushOutcome {
        while !self.buf.is_empty() {
            if closing.load(Ordering::SeqCst) {
                return FlushOutcome::ShutdownAbandoned;
            }
            tokio::select! {
                biased;
                permit = self.tx.reserve() => {
                    match permit {
                        Ok(permit) => {
                            permit.send(self.buf.pop_front().expect("buf checked non-empty"));
                        }
                        Err(_) => return FlushOutcome::ConsumerGone,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    if closing.load(Ordering::SeqCst) {
                        return FlushOutcome::ShutdownAbandoned;
                    }
                }
            }
        }
        FlushOutcome::Flushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn accept_forwards_while_capacity_and_spills_on_full_without_blocking() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let mut spill = ReplySpill::new(tx, 64);

        // 10 accepts against a 2-slot channel with no consumer: 2 forwarded,
        // 8 spilled, zero awaits (this test would deadlock otherwise —
        // single-threaded runtime, no consumer task).
        for i in 0..10u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted);
        }
        assert_eq!(spill.pending(), 8);
        assert_eq!(spill.dropped(), 0);
        assert_eq!(rx.recv().await.unwrap(), vec![0]);
        assert_eq!(rx.recv().await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn accept_drops_and_counts_beyond_the_cap() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 3);

        // 1 forwarded + 3 buffered = at cap; the next two drop.
        for i in 0..4u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted, "i={i}");
        }
        assert_eq!(spill.accept(vec![4]), AcceptOutcome::DroppedFull);
        assert_eq!(spill.accept(vec![5]), AcceptOutcome::DroppedFull);
        assert_eq!(spill.pending(), 3);
        assert_eq!(spill.dropped(), 2);

        // Everything that was ACCEPTED still arrives, in order, once the
        // consumer drains — drops only ever discard the overflow.
        let closing = AtomicBool::new(false);
        let consumer = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(pkt) = rx.recv().await {
                got.push(pkt[0]);
            }
            got
        });
        assert_eq!(spill.flush(&closing).await, FlushOutcome::Flushed);
        assert_eq!(consumer.await.unwrap(), vec![0, 1, 2, 3]);
    }

    /// ZEB-816: peak tracks the high-water buffer depth, so a clean run
    /// (dropped == 0) can be told apart from one that never buffered.
    #[tokio::test]
    async fn peak_tracks_high_water_of_buffer_depth() {
        // 1-slot channel, no consumer: the first accept forwards into the
        // channel, every later one buffers, so pending() (and peak) climb.
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 64);
        assert_eq!(spill.peak(), 0, "nothing accepted yet");
        for i in 0..5u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted);
        }
        // 1 forwarded + 4 buffered → peak buffer depth 4, no drops.
        assert_eq!(spill.pending(), 4);
        assert_eq!(spill.peak(), 4);
        assert_eq!(spill.dropped(), 0);
    }

    /// ZEB-816: on overflow the peak pins to the cap and holds — a dropped
    /// run reads as "peaked at the ceiling", not as ambiguous headroom.
    #[tokio::test]
    async fn peak_reaches_cap_on_overflow_and_holds() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 3);
        // 1 forwarded + 3 buffered = at cap; peak == 3.
        for i in 0..4u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted, "i={i}");
        }
        assert_eq!(spill.peak(), 3);
        // Overflow drops, and peak stays pinned at the cap (does not grow).
        assert_eq!(spill.accept(vec![9]), AcceptOutcome::DroppedFull);
        assert_eq!(spill.peak(), 3);
        assert_eq!(spill.dropped(), 1);
    }

    /// ZEB-816 (Qodo, PR #570): when `try_send` has capacity for every
    /// payload, nothing is ever retained under backpressure, so `peak()`
    /// stays 0 — the "spill path never exercised" signal the contract
    /// promises. Sampling the buffer *before* `try_flush` (the pre-fix
    /// ordering) reported peak 1 here after the very first accept, making the
    /// zero signal unreachable and defeating the whole point of the metric.
    #[tokio::test]
    async fn peak_stays_zero_when_channel_always_has_capacity() {
        // Channel roomier than the payload count and never full: every accept
        // forwards immediately, buf returns to empty each time.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let mut spill = ReplySpill::new(tx, 64);
        for i in 0..5u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted);
        }
        assert_eq!(spill.pending(), 0, "nothing retained under backpressure");
        assert_eq!(spill.peak(), 0, "spill never had to buffer");
        assert_eq!(spill.dropped(), 0);
        // The payloads did flow through, in order.
        for i in 0..5u8 {
            assert_eq!(rx.recv().await.unwrap(), vec![i]);
        }
    }

    #[tokio::test]
    async fn accept_reports_consumer_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        drop(rx);
        let mut spill = ReplySpill::new(tx, 64);
        assert_eq!(spill.accept(vec![7]), AcceptOutcome::ConsumerGone);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_delivers_everything_in_order_to_a_slow_consumer() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let mut spill = ReplySpill::new(tx, 64);
        for i in 0..16u8 {
            assert_eq!(spill.accept(vec![i]), AcceptOutcome::Accepted);
        }
        let closing = AtomicBool::new(false);

        let consumer = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(pkt) = rx.recv().await {
                got.push(pkt[0]);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            got
        });

        assert_eq!(spill.flush(&closing).await, FlushOutcome::Flushed);
        // Dropping the spill dropped the sender; the consumer loop ends.
        let got = consumer.await.unwrap();
        assert_eq!(got, (0..16u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn flush_reports_consumer_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 64);
        assert_eq!(spill.accept(vec![1]), AcceptOutcome::Accepted);
        assert_eq!(spill.accept(vec![2]), AcceptOutcome::Accepted); // spilled: full
        drop(rx);
        let closing = AtomicBool::new(false);
        assert_eq!(spill.flush(&closing).await, FlushOutcome::ConsumerGone);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_abandons_on_closing_while_consumer_wedged() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 64);
        assert_eq!(spill.accept(vec![1]), AcceptOutcome::Accepted);
        assert_eq!(spill.accept(vec![2]), AcceptOutcome::Accepted); // wedged: full, no consumer
        let closing = AtomicBool::new(true); // shutdown already requested
        let start = std::time::Instant::now();
        assert_eq!(spill.flush(&closing).await, FlushOutcome::ShutdownAbandoned);
        // With the top-of-loop check this returns without even one poll
        // tick; the generous budget guards only against an unbounded wedge.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "closing poll did not fire promptly"
        );
    }

    /// PR #559 review (CodeRabbit): shutdown must never report `Flushed`,
    /// even when the consumer has freed capacity and the biased `reserve()`
    /// arm would win every iteration.
    #[tokio::test]
    async fn flush_abandons_on_preset_closing_even_with_capacity_available() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx, 64);
        assert_eq!(spill.accept(vec![1]), AcceptOutcome::Accepted); // forwarded
        assert_eq!(spill.accept(vec![2]), AcceptOutcome::Accepted); // buffered
        assert_eq!(rx.recv().await.unwrap(), vec![1]); // capacity now free

        let closing = AtomicBool::new(true);
        assert_eq!(spill.flush(&closing).await, FlushOutcome::ShutdownAbandoned);
        // The buffered payload was abandoned, not delivered post-shutdown.
        assert!(rx.try_recv().is_err(), "no delivery after closing");
    }
}
