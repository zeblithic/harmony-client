//! ZEB-321 Phase 1 Task 5: `zenoh_link::LinkUnicastTrait` impl over an Iroh
//! QUIC bidi stream pair.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.2.
//!
//! ## Design intent
//!
//! Zenoh's transport plugin surface (`zenoh-link-commons`) defines
//! [`LinkUnicastTrait`] — a per-link send/recv interface that the upper
//! Zenoh transport stack calls into. By wrapping a paired
//! `(SendStream, RecvStream)` from an iroh QUIC bidi stream in this
//! trait we make iroh look like any other Zenoh transport (TCP, UDP,
//! QUIC over raw sockets, …). All existing CRDT sync code keeps working
//! unchanged once the matching `LinkManagerUnicastTrait` (Task 6) is
//! plugged in.
//!
//! ## API adaptations from the plan draft
//!
//! The plan draft was written against an unverified zenoh-link surface.
//! What's actually pinned (`zenoh = "1"`, resolving to `zenoh-link
//! 1.9.0` in `Cargo.lock` after PR #162 bumped the family) differs from
//! the plan in three places:
//!
//! - `LinkUnicastTrait` does **not** include `is_local` (the plan had
//!   it). Removed.
//! - [`LinkAuthId`] has no `None` variant — the closest fit for a
//!   QUIC-backed link is `LinkAuthId::Quic(None)` (i.e. QUIC with no
//!   per-link CN identity beyond the Iroh `EndpointId` already in the
//!   locator).
//! - The 1.9.0 trait surface threads an `Option<Priority>` through
//!   `read`/`write`/`read_exact`/`write_all`. We don't surface any
//!   priority semantics from iroh QUIC (per-stream priority isn't a
//!   thing in iroh's bidi stream API), so the parameter is accepted
//!   and ignored — `supports_priorities()` defaults to `false` from
//!   the trait, and we don't override it.
//! - `ZResult` and the `zerror!` macro are not re-exported from
//!   `zenoh-link`; we pull them from `zenoh-result` (already a
//!   transitive dep — verified in `Cargo.lock`).
//!
//! We import these via the existing `zenoh = "1"` direct dep + the
//! `zenoh-link` / `zenoh-result` transitive deps already in
//! `Cargo.lock` — no new top-level dep needed.

use std::sync::Arc;

use async_trait::async_trait;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::sync::Mutex;
use zenoh_link::{LinkAuthId, LinkUnicastTrait, Locator};
use zenoh_protocol::{core::Priority, transport::BatchSize};
use zenoh_result::{zerror, ZResult};

/// One end of a Zenoh-over-Iroh link: a paired QUIC bidi stream wrapped
/// so the upper Zenoh transport stack sees it as a generic
/// [`LinkUnicastTrait`]. The two halves are independently
/// `Mutex`-guarded so a concurrent reader and writer never serialize
/// behind a single lock.
pub struct IrohZenohLink {
    send: Arc<Mutex<SendStream>>,
    recv: Arc<Mutex<RecvStream>>,
    src: Locator,
    dst: Locator,
}

impl IrohZenohLink {
    /// Build a link from an already-opened iroh QUIC bidi pair plus the
    /// locators identifying our end (`src`) and the peer's end (`dst`).
    ///
    /// Construction is sync because the actual stream open happens at
    /// the call site (the link manager calls `connect` / `accept_bi`
    /// then hands the streams over).
    pub fn new(send: SendStream, recv: RecvStream, src: Locator, dst: Locator) -> Self {
        Self {
            send: Arc::new(Mutex::new(send)),
            recv: Arc::new(Mutex::new(recv)),
            src,
            dst,
        }
    }
}

#[async_trait]
impl LinkUnicastTrait for IrohZenohLink {
    async fn write(&self, buffer: &[u8], _priority: Option<Priority>) -> ZResult<usize> {
        let mut s = self.send.lock().await;
        s.write(buffer)
            .await
            .map_err(|e| zerror!("iroh write: {e}").into())
    }

    async fn write_all(&self, buffer: &[u8], _priority: Option<Priority>) -> ZResult<()> {
        let mut s = self.send.lock().await;
        s.write_all(buffer)
            .await
            .map_err(|e| zerror!("iroh write_all: {e}").into())
    }

    async fn read(&self, buffer: &mut [u8], _priority: Option<Priority>) -> ZResult<usize> {
        let mut r = self.recv.lock().await;
        // `RecvStream::read` returns `Ok(None)` at clean stream EOF —
        // map that to a read error so the caller sees a terminating
        // event instead of a phantom zero-byte read (which Zenoh
        // would interpret as "try again", busy-looping).
        match r.read(buffer).await {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Err(zerror!("iroh stream EOF").into()),
            Err(e) => Err(zerror!("iroh read: {e}").into()),
        }
    }

    async fn read_exact(&self, buffer: &mut [u8], _priority: Option<Priority>) -> ZResult<()> {
        let mut r = self.recv.lock().await;
        r.read_exact(buffer)
            .await
            .map_err(|e| zerror!("iroh read_exact: {e}").into())
    }

    async fn close(&self) -> ZResult<()> {
        // `finish` marks the send side as gracefully closed (the peer
        // will see `Ok(None)` on its next read). It returns
        // `Err(ClosedStream)` if the stream was already closed —
        // idempotent close is the contract for `LinkUnicastTrait`, so
        // we swallow that error.
        let mut s = self.send.lock().await;
        let _ = s.finish();
        Ok(())
    }

    fn get_mtu(&self) -> BatchSize {
        // QUIC streams have no per-frame size limit — the underlying
        // transport handles flow control + segmentation. Advertise the
        // max `BatchSize` (u16::MAX) so Zenoh's batching layer doesn't
        // chunk smaller than necessary.
        BatchSize::MAX
    }

    fn get_src(&self) -> &Locator {
        &self.src
    }

    fn get_dst(&self) -> &Locator {
        &self.dst
    }

    fn is_reliable(&self) -> bool {
        // QUIC streams are reliable + ordered.
        true
    }

    fn is_streamed(&self) -> bool {
        // QUIC bidi streams are byte-streams (not datagrams).
        true
    }

    fn get_interface_names(&self) -> Vec<String> {
        // Iroh chooses the underlying transport (direct hole-punch vs
        // DERP relay) opaquely — there is no single OS interface to
        // report here.
        vec![]
    }

    fn get_auth_id(&self) -> &LinkAuthId {
        // QUIC-backed link with no peer-CN identity beyond the Iroh
        // `EndpointId` already encoded in the locator. Authentication
        // is handled one layer up by the harmony device-pairing
        // handshake (Task 7+).
        const QUIC_NONE: LinkAuthId = LinkAuthId::Quic(None);
        &QUIC_NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end round-trip over a real iroh QUIC connection on
    /// loopback. Built with `presets::Minimal` (crypto provider only —
    /// **no** Address Lookup, **no** pkarr publisher) and explicit
    /// loopback bind, so the test never touches the network. The
    /// dialer reaches the acceptor via an explicit `EndpointAddr`
    /// carrying loopback IP addresses, bypassing iroh's discovery
    /// layer entirely.
    ///
    /// The `presets::N0` preset wires up a pkarr publisher + DNS
    /// address-lookup service even when `RelayMode::Disabled` is
    /// applied on top — those make outbound DNS / pkarr queries on
    /// bind, which hang in offline / sandboxed environments. Mirror
    /// the in-tree `iroh_endpoint::tests` pattern *for the relay
    /// piece*, but downgrade the preset for full hermeticity.
    #[tokio::test]
    async fn paired_stream_roundtrip_via_loopback() {
        // ZEB-347: prime the one-time process-global iroh bind init before
        // the asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        // Hard wall-clock cap so a future regression in the QUIC
        // teardown sequence can't strand the suite indefinitely
        // (we previously hung on `incoming.closed().await` after the
        // data exchange had already completed — see fix below).
        // 30s budget: solo wall-clock is ~10s; under full-suite
        // nextest parallelism (~80 binaries linking + executing
        // concurrently) the iroh QUIC handshake observed up to
        // ~20s — Task 9's regression run flaked at 19.7s on the
        // original 15s cap. 30s gives 1.5x headroom while still
        // failing fast on a real deadlock regression.
        tokio::time::timeout(std::time::Duration::from_secs(30), inner())
            .await
            .expect("paired_stream_roundtrip must complete within 30s");
    }

    async fn inner() {
        use crate::iroh_endpoint::alpn;
        use iroh::endpoint::{presets, Endpoint, RelayMode};
        use iroh::{EndpointAddr, SecretKey, TransportAddr};
        use rand::RngCore;
        use std::net::Ipv4Addr;

        // Fresh ephemeral secrets for each endpoint — production uses
        // keychain-persisted keys, but for a one-shot round-trip
        // disposable identities are fine.
        let mut buf_a = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf_a);
        let mut buf_b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf_b);

        let key_a = SecretKey::from_bytes(&buf_a);
        let key_b = SecretKey::from_bytes(&buf_b);

        // `presets::Minimal` sets only the crypto provider — no
        // address-lookup service, no relays. We also call
        // `clear_ip_transports()` so the default 0.0.0.0 / [::] binds
        // are dropped, then add an explicit loopback IPv4 bind.
        // Result: neither endpoint touches anything but loopback.
        // (Pattern mirrors iroh's own `test_bind_addr_clear` test.)
        let ep_a = Endpoint::builder(presets::Minimal)
            .secret_key(key_a)
            .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr ep_a")
            .bind()
            .await
            .expect("bind ep_a");
        let ep_b = Endpoint::builder(presets::Minimal)
            .secret_key(key_b)
            .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr ep_b")
            .bind()
            .await
            .expect("bind ep_b");

        // Build ep_b's full EndpointAddr from its bound sockets —
        // with no address-lookup service running, `ep_b.addr()` would
        // not contain direct IPs by itself. We construct it manually
        // from the known loopback sockets.
        let ep_a_id = ep_a.id();
        let ep_b_id = ep_b.id();
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b_id,
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Accept side runs on ep_b in a spawned task. Server wraps its
        // half in IrohZenohLink and exercises the async trait methods
        // there too — that way the actual byte transport is going
        // *through* the wrapper on both sides, not just around it.
        let ep_b_clone = ep_b.clone();
        let server_src = Locator::new("iroh", ep_b_id.to_string(), "").expect("server src");
        let server_dst = Locator::new("iroh", ep_a_id.to_string(), "").expect("server dst");
        let accept_task = tokio::spawn(async move {
            let incoming = ep_b_clone
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("connection established");
            let (send, recv) = incoming.accept_bi().await.expect("accept_bi");
            let server_link = IrohZenohLink::new(send, recv, server_src, server_dst);
            let mut buf = [0u8; 5];
            server_link
                .read_exact(&mut buf, None)
                .await
                .expect("server link read");
            assert_eq!(&buf, b"hello");
            server_link
                .write_all(b"world", None)
                .await
                .expect("server link write");
            server_link.close().await.expect("server link close");
            // Wait for the client to drive the connection close. This
            // is the symmetric counterpart to the client's explicit
            // `conn.close()` below — it gives the QUIC layer time to
            // flush the "world" reply to the client before the
            // connection is torn down. (Dropping `incoming` here
            // immediately would race: server-side teardown can wipe
            // in-flight bytes, causing the client's read to fail with
            // "connection lost". See ZEB-321 Task 5 fix: 6-hour stall
            // followed by 11s race failure, 2026-05-22.)
            let _ = incoming.closed().await;
        });

        // Dial side: open the bidi stream, wrap in IrohZenohLink, and
        // round-trip the data through the wrapper's async trait methods.
        let conn = ep_a
            .connect(ep_b_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("connect");
        let (send, recv) = conn.open_bi().await.expect("open_bi");
        let client_src = Locator::new("iroh", ep_a_id.to_string(), "").expect("client src");
        let client_dst = Locator::new("iroh", ep_b_id.to_string(), "").expect("client dst");
        let link = IrohZenohLink::new(send, recv, client_src.clone(), client_dst.clone());

        link.write_all(b"hello", None)
            .await
            .expect("client link write");
        let mut buf = [0u8; 5];
        link.read_exact(&mut buf, None)
            .await
            .expect("client link read");
        assert_eq!(&buf, b"world");

        // Sync trait surface checks.
        assert_eq!(link.get_src(), &client_src);
        assert_eq!(link.get_dst(), &client_dst);
        assert_eq!(link.get_mtu(), BatchSize::MAX);
        assert!(link.is_reliable());
        assert!(link.is_streamed());
        assert!(link.get_interface_names().is_empty());
        assert!(matches!(link.get_auth_id(), LinkAuthId::Quic(None)));

        // Close is idempotent — calling twice must not error.
        link.close().await.expect("first close");
        link.close().await.expect("second close (idempotent)");

        // Drive the connection close from the client side. The server's
        // `incoming.closed().await` then returns naturally; without this
        // explicit close the server would either (a) hang forever
        // waiting for a peer close that never comes, or (b) close its
        // own end too early and race the client's read. The `0u32` is
        // an application close code; the byte string is a human-
        // readable reason transmitted in the QUIC CONNECTION_CLOSE.
        drop(link);
        conn.close(0u32.into(), b"test-done");
        conn.closed().await;

        // Server returns once its `incoming.closed()` future resolves.
        accept_task.await.expect("server task panic");
        ep_a.close().await;
        ep_b.close().await;
    }
}
