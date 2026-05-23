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
//! 1.8.0` in `Cargo.lock` today, with `zenoh-link 1.9.0` available)
//! differs in three places:
//!
//! - `LinkUnicastTrait` does **not** include `is_local` (the plan had
//!   it). Removed.
//! - [`LinkAuthId`] has no `None` variant — the closest fit for a
//!   QUIC-backed link is `LinkAuthId::Quic(None)` (i.e. QUIC with no
//!   per-link CN identity beyond the Iroh `EndpointId` already in the
//!   locator). The 1.9.0 trait surface additionally threads an
//!   `Option<Priority>` through read/write — we pin 1.8.0 to avoid
//!   that churn; the priority axis is unused in Phase 1.
//! - `ZResult` and the `zerror!` macro are not re-exported from
//!   `zenoh-link`; we pull them from `zenoh-result` (already a
//!   transitive dep — verified in `Cargo.lock`).
//!
//! We import these via the existing `zenoh = "1"` direct dep + the
//! `zenoh-link` / `zenoh-result` transitive deps already in
//! `Cargo.lock` — no new top-level dep needed. Should the lockfile
//! ever resolve `zenoh-link` to a major-bumped release with the
//! `priority` parameter, we'd need to add an explicit pin and adapt
//! this file; for now relying on the existing transitive resolution
//! keeps the dep graph minimal.

use std::sync::Arc;

use async_trait::async_trait;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::sync::Mutex;
use zenoh_link::{LinkAuthId, LinkUnicastTrait, Locator};
use zenoh_protocol::transport::BatchSize;
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
    async fn write(&self, buffer: &[u8]) -> ZResult<usize> {
        let mut s = self.send.lock().await;
        s.write(buffer)
            .await
            .map_err(|e| zerror!("iroh write: {e}").into())
    }

    async fn write_all(&self, buffer: &[u8]) -> ZResult<()> {
        let mut s = self.send.lock().await;
        s.write_all(buffer)
            .await
            .map_err(|e| zerror!("iroh write_all: {e}").into())
    }

    async fn read(&self, buffer: &mut [u8]) -> ZResult<usize> {
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

    async fn read_exact(&self, buffer: &mut [u8]) -> ZResult<()> {
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

        // Accept side runs on ep_b in a spawned task. iroh's
        // open_bi/accept_bi contract requires the dialer to write
        // first (otherwise accept_bi waits forever) — we honor that
        // below.
        let ep_b_clone = ep_b.clone();
        let accept_task = tokio::spawn(async move {
            let incoming = ep_b_clone
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("connection established");
            let (mut send, mut recv) = incoming.accept_bi().await.expect("accept_bi");
            let mut buf = [0u8; 5];
            recv.read_exact(&mut buf).await.expect("server read");
            assert_eq!(&buf, b"hello");
            send.write_all(b"world").await.expect("server write");
            send.finish().expect("server finish");
            // Hold the connection open until the client side has had
            // a chance to drain its read — dropping `incoming` here
            // would close the connection prematurely.
            let _ = incoming.closed().await;
        });

        // Dial side: open the bidi stream, immediately write so the
        // accept_task can progress past accept_bi.
        let conn = ep_a
            .connect(ep_b_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        send.write_all(b"hello").await.expect("client write");
        send.finish().expect("client finish");
        let mut buf = [0u8; 5];
        recv.read_exact(&mut buf).await.expect("client read");
        assert_eq!(&buf, b"world");

        // Wrap the client side in `IrohZenohLink` and exercise the
        // trait surface to confirm the wrapper compiles + delegates.
        // We construct locators with the iroh protocol prefix and
        // the bs58-formatted EndpointId as the address — the
        // canonical form the LinkManager will use in Task 6.
        let src = Locator::new("iroh", ep_a_id.to_string(), "").expect("src locator");
        let dst = Locator::new("iroh", ep_b_id.to_string(), "").expect("dst locator");
        let link = IrohZenohLink::new(send, recv, src.clone(), dst.clone());

        assert_eq!(link.get_src(), &src);
        assert_eq!(link.get_dst(), &dst);
        assert_eq!(link.get_mtu(), BatchSize::MAX);
        assert!(link.is_reliable());
        assert!(link.is_streamed());
        assert!(link.get_interface_names().is_empty());
        assert!(matches!(link.get_auth_id(), LinkAuthId::Quic(None)));

        // Close is idempotent — calling twice must not error.
        link.close().await.expect("first close");
        link.close().await.expect("second close (idempotent)");

        // Wait for accept side to finish, then shut both endpoints
        // down cleanly. The accept_task awaits `closed()` so we must
        // drop the connection before joining; the connection drops
        // when `link` (which owns send/recv) goes out of scope at
        // the end of the test, so just close the endpoint.
        drop(link);
        let _ = accept_task.await;
        ep_a.close().await;
        ep_b.close().await;
    }
}
