//! ZEB-473 (DM-over-iroh, Move 1a): inbound PQ tunnel acceptor.
//!
//! Implements [`IrohHandshakeDispatcher`] for the `harmony/tunnel/v1` ALPN. The
//! accept loop in `zenoh_iroh_transport` routes a negotiated tunnel connection
//! here; we spawn the responder driver, which completes the PQ handshake,
//! registers the live session into the [`TunnelManager`] (so our outbound DMs to
//! this peer reuse the bidirectional tunnel), and feeds inbound `FrameTag::Dm`
//! payloads onto the ingest seam.

use std::sync::Arc;

use async_trait::async_trait;
use harmony_identity::PqPrivateIdentity;
use iroh::endpoint::Connection;
use tokio::sync::mpsc;

use crate::iroh_invite_acceptor::IrohHandshakeDispatcher;
use crate::tunnel_manager::{InboundDm, TunnelManager};

/// Inbound `harmony/tunnel/v1` acceptor. Holds the handles the responder driver
/// needs: our own PQ private identity (the responder self-input), the manager to
/// register the live session into, and the ingest channel for received DMs.
pub struct IrohTunnelAcceptor {
    local_pq: Arc<PqPrivateIdentity>,
    mgr: Arc<TunnelManager>,
    ingest_tx: mpsc::Sender<InboundDm>,
}

impl IrohTunnelAcceptor {
    pub fn new(
        local_pq: Arc<PqPrivateIdentity>,
        mgr: Arc<TunnelManager>,
        ingest_tx: mpsc::Sender<InboundDm>,
    ) -> Self {
        Self {
            local_pq,
            mgr,
            ingest_tx,
        }
    }
}

#[async_trait]
impl IrohHandshakeDispatcher for IrohTunnelAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        // Spawn so a slow/hung peer can't block the accept loop (the responder
        // driver owns the connection for the tunnel's whole lifetime).
        let local_pq = Arc::clone(&self.local_pq);
        let mgr = Arc::clone(&self.mgr);
        let ingest_tx = self.ingest_tx.clone();
        tokio::spawn(async move {
            crate::tunnel_task::run_tunnel_responder(conn, local_pq, mgr, ingest_tx).await;
        });
    }
}
