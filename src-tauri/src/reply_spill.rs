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
//!    awaits. Memory stays bounded by construction: these drains process one
//!    GET at a time, and one GET's reply count is capped by the request's
//!    clamped `limit`, so the spill can never hold more than one page.
//! 2. **After the stream closes** — [`ReplySpill::flush`] delivers whatever
//!    the consumer hasn't absorbed yet. Blocking on the engine *here* is the
//!    natural request-level backpressure point (the driver won't take its
//!    next request until the page lands), and the closing flag stays live
//!    because the send-permit acquisition is a select ARM (cancel-safe
//!    `reserve()`), not an await buried inside an arm body.
//!
//! Spill is not drop: nothing is discarded except on the two paths that
//! already abandon work today — consumer teardown and node shutdown.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

/// Terminal state of a [`ReplySpill::flush`].
#[derive(Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Every buffered payload was delivered to the engine channel.
    Flushed,
    /// The engine receiver is gone (engine teardown); remaining payloads
    /// were dropped with it. Callers should exit their drive loop.
    ConsumerGone,
    /// The closing flag flipped while the consumer was still wedged;
    /// remaining payloads were abandoned, matching the pre-existing
    /// no-report shutdown semantics of the drains this serves.
    ShutdownAbandoned,
}

/// FIFO spill buffer in front of a bounded `mpsc::Sender<Vec<u8>>`.
pub struct ReplySpill {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    buf: VecDeque<Vec<u8>>,
}

impl ReplySpill {
    pub fn new(tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            buf: VecDeque::new(),
        }
    }

    /// Accept one drained reply payload: buffer it, then forward as much of
    /// the buffer as fits RIGHT NOW (`try_send` only — never awaits, so the
    /// caller's reply-drain loop never blocks on the engine).
    ///
    /// Returns `false` when the engine receiver is gone; the caller should
    /// exit its drive loop (same semantics as a failed `send` before
    /// ZEB-812).
    #[must_use]
    pub fn accept(&mut self, bytes: Vec<u8>) -> bool {
        self.buf.push_back(bytes);
        self.try_flush()
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

    /// Post-drain delivery of everything still buffered, in order. Call this
    /// ONLY after the zenoh reply stream has closed — blocking on the engine
    /// here is harmless to the transport and is the intended request-level
    /// backpressure. The `closing` flag is polled every 500ms even while the
    /// consumer is wedged: `reserve()` is a cancel-safe select arm, so no
    /// payload is lost when the poll arm wins.
    pub async fn flush(mut self, closing: &AtomicBool) -> FlushOutcome {
        while !self.buf.is_empty() {
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
        let mut spill = ReplySpill::new(tx);

        // 10 accepts against a 2-slot channel with no consumer: 2 forwarded,
        // 8 spilled, zero awaits (this test would deadlock otherwise —
        // single-threaded runtime, no consumer task).
        for i in 0..10u8 {
            assert!(spill.accept(vec![i]));
        }
        assert_eq!(spill.pending(), 8);
        assert_eq!(rx.recv().await.unwrap(), vec![0]);
        assert_eq!(rx.recv().await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn accept_reports_consumer_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        drop(rx);
        let mut spill = ReplySpill::new(tx);
        assert!(!spill.accept(vec![7]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_delivers_everything_in_order_to_a_slow_consumer() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let mut spill = ReplySpill::new(tx);
        for i in 0..16u8 {
            assert!(spill.accept(vec![i]));
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
        let mut spill = ReplySpill::new(tx);
        assert!(spill.accept(vec![1]));
        assert!(spill.accept(vec![2])); // spilled: channel is full
        drop(rx);
        let closing = AtomicBool::new(false);
        assert_eq!(spill.flush(&closing).await, FlushOutcome::ConsumerGone);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_abandons_on_closing_while_consumer_wedged() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let mut spill = ReplySpill::new(tx);
        assert!(spill.accept(vec![1]));
        assert!(spill.accept(vec![2])); // wedged: full channel, live receiver, no consumer
        let closing = AtomicBool::new(true); // shutdown already requested
        let start = std::time::Instant::now();
        assert_eq!(spill.flush(&closing).await, FlushOutcome::ShutdownAbandoned);
        // One 500ms poll tick, give or take scheduling — the budget is far
        // below any regression that would matter (an unbounded wedge).
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "closing poll did not fire promptly"
        );
    }
}
