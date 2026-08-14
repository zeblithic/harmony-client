//! SimNet CRDT plane — N `CommunitySyncEngine`s composed over a partitionable
//! [`SimBus`], sharing one in-memory content store (CAS).
//!
//! **Convergence note (load-bearing).** The engine's `publisher_tx`/
//! `subscriber_rx` plane has NO periodic anti-entropy. A publish that is
//! dropped downstream (a partition dropping bytes) leaves the sender neither
//! dirty nor retry-armed, so advancing virtual time after a heal re-delivers
//! nothing. Post-heal convergence is driven by an explicit
//! [`CommunitySyncEngine::flush_now`] on each node that mutated during the
//! partition — it re-ships the full state root, and since the CAS is shared
//! (never partitioned), one post-heal publish carries everything the lagging
//! side missed. See the module design doc for the full rationale.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};

use super::anomaly::Sample;
use super::bus::SimBus;
use super::partition::Partition;

use crate::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use crate::community_membership::{mint_test_owner, SignedMembershipEvent, TestOwner};
use crate::community_state_crdt::{CommunityState, InsertOutcome};
use crate::community_state_sync::{
    CommunityReplayTracker, CommunitySyncEngine, CommunitySyncEngineConfig, IdentityResolver,
    PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use crate::content_store::{CasOp, ContentStore, RuntimeContentStore};
use crate::hlc_adopt_floor::HlcAdoptFloor;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// Dummy resolver: verification is satisfied from each Join's EnrollmentCert,
/// so a constant pubkey is never the deciding verifier (matches the 2-node
/// fixtures, which pass `[0u8; 64]`).
struct SimIdentityResolver;

#[async_trait::async_trait]
impl IdentityResolver for SimIdentityResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        Some([0u8; 64])
    }
}

/// One shared in-memory CAS servicer for all engines. Returns the op sender;
/// each node wraps its own clone in a `RuntimeContentStore`.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_tx, mut cas_rx) = mpsc::channel::<CasOp>(256);
    tokio::spawn(async move {
        while let Some(op) = cas_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = cas.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    cas_tx
}

/// Order-independent u64 digest of a state's event-id set. Two logs with the
/// same event set (any insertion order) hash equal; different sets differ.
fn event_set_digest(state: &CommunityState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut acc: u64 = 0;
    for ev in state.events() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ev.id.hash(&mut h);
        acc ^= h.finish();
    }
    acc
}

/// One logical node: a `CommunitySyncEngine` + its shared `CommunityState` +
/// a stable identity + its partition tag.
pub(crate) struct SimCommunityNode {
    pub index: usize,
    pub owner: OwnerAddr,
    pub device_id: String,
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    pub state: Arc<Mutex<CommunityState>>,
    pub engine: CommunitySyncEngine,
    pub tag: [u8; 32],
    #[allow(dead_code)] // retained for HLC-derivation debugging / future seams.
    pub join_hlc: Hlc,
}

/// N `CommunitySyncEngine`s over one shared partition, bus, and CAS.
pub(crate) struct SimCommunity {
    pub community_id: SpaceId,
    pub admin_owner: OwnerAddr,
    partition: Partition,
    _bus: SimBus,                 // kept alive; drop aborts drainers.
    _cas_tx: mpsc::Sender<CasOp>, // kept alive; drop stops the servicer.
    _tmpdirs: Vec<tempfile::TempDir>,
    nodes: Vec<SimCommunityNode>,
}

/// Local identity bundle assembled during `build`.
struct Ident {
    identity: TestOwner,
    signing: Arc<ed25519_dalek::SigningKey>,
    device_id: String,
    tag: [u8; 32],
}

impl SimCommunity {
    /// Build N nodes (index 1..=n): node 1 mints an OPEN community + its
    /// bootstrap Join; nodes 2..=n redeem an open invite. Every node then
    /// insert-locals every OTHER node's bootstrap Join (O(N^2) cross-seed for
    /// the membership-at-HLC gate). Returns with all nodes holding all N Joins
    /// (baseline convergence achieved with NO bus traffic).
    pub(crate) async fn build(n: u8) -> Self {
        assert!((2..=12).contains(&n), "SimCommunity supports 2..=12 nodes");
        let resolver: Arc<dyn IdentityResolver> = Arc::new(SimIdentityResolver);
        let cas_tx = spawn_shared_cas();

        // Per-node identities (seed = index; stay in 1..=12 to avoid the
        // seed ^ 0xFF key-material collision documented on mint_test_owner).
        let idents: Vec<Ident> = (1..=n)
            .map(|i| {
                let identity = mint_test_owner(i);
                let signing = Arc::new(identity.device_key.clone());
                Ident {
                    identity,
                    signing,
                    device_id: format!("n{i}-dev"),
                    tag: [i; 32],
                }
            })
            .collect();

        // Node 1 mints the OPEN community + bootstrap Join.
        let admin = &idents[0];
        let admin_owner = admin.identity.owner;
        let minted_admin = crate::mint_community_creation(
            "SimCommunity",
            false,
            admin_owner,
            &admin.signing,
            &admin.identity.cert,
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: admin.device_id.clone(),
            },
        )
        .expect("mint create");
        let community_id = minted_admin.community_id;
        let membership_key = minted_admin.membership_key.clone();

        // Bootstrap Join per node: admin's is `minted_admin.bootstrap_join`;
        // each other node redeems an open invite.
        let mut bootstrap_joins: Vec<SignedMembershipEvent> = Vec::with_capacity(n as usize);
        bootstrap_joins.push(minted_admin.bootstrap_join.clone());
        for (offset, id) in idents.iter().enumerate().skip(1) {
            let invite = CommunityInvitePayload {
                inviter_signer_certs: Vec::new(),
                community_id,
                epoch_snapshot: InviteEpochSnapshot {
                    epoch: 0,
                    sealed_epoch_key: membership_key.as_bytes().to_vec(),
                    sealed_epoch_keys: Vec::new(),
                    state_snapshot: MaterializedCommunityState::default(),
                },
                admin_addr: admin_owner,
                community_name: "SimCommunity".into(),
                is_invite_only: false,
                expires_at: None,
                invite_token: None,
                admin_bootstrap: None,
                admin_identity_pub: None,
                forked_from: None,
                pre_fork_snapshot: None,
                inviter_enrollment: None,
                untargeted_decrypt_key: None,
            };
            let minted = crate::mint_redemption(
                &invite,
                id.identity.owner,
                &id.signing,
                &id.identity.cert,
                Hlc {
                    wall_ms: 200_000 + offset as u64,
                    logical: 0,
                    device_id: id.device_id.clone(),
                },
            )
            .expect("mint redeem");
            bootstrap_joins.push(minted.bootstrap_join.clone());
        }

        // Per-node channels: engine.publisher_tx = out_tx; bus drains out_rx.
        //                    bus delivers to in_tx; engine.subscriber_rx = in_rx.
        let mut out_txs = Vec::new();
        let mut out_rxs = Vec::new();
        let mut in_txs = Vec::new();
        let mut in_rxs = Vec::new();
        for _ in 0..n {
            let (o_tx, o_rx) = mpsc::channel::<Vec<u8>>(256);
            let (i_tx, i_rx) = mpsc::channel::<Vec<u8>>(256);
            out_txs.push(o_tx);
            out_rxs.push(o_rx);
            in_txs.push(i_tx);
            in_rxs.push(i_rx);
        }

        // Build engines.
        let mut tmpdirs = Vec::new();
        let mut nodes = Vec::new();
        let mut in_rxs_iter = in_rxs.into_iter();
        let mut out_txs_iter = out_txs.into_iter();
        for (idx0, id) in idents.iter().enumerate() {
            let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
            let tracker = Arc::new(Mutex::new(CommunityReplayTracker::new((
                id.identity.owner,
                id.device_id.clone(),
            ))));
            let tmp = tempfile::tempdir().expect("tmp");
            let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
                cas_tx.clone(),
                Duration::from_secs(2),
            ));
            let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
                adopt_floor: HlcAdoptFloor::new(),
                community_id,
                membership_key: membership_key.clone(),
                admin_addr: admin_owner,
                is_invite_only: false,
                device_id: id.device_id.clone(),
                self_owner: id.identity.owner,
                signing_key: Arc::clone(&id.signing),
                state: Arc::clone(&state),
                tracker,
                content_store: cs,
                publisher_tx: out_txs_iter.next().unwrap(),
                subscriber_rx: in_rxs_iter.next().unwrap(),
                paths: PersistPaths {
                    crdt: tmp.path().join("crdt.cbor"),
                    replay: tmp.path().join("replay.cbor"),
                },
                debounce_ms: DEFAULT_DEBOUNCE_MS,
                identity_resolver: Some(Arc::clone(&resolver)),
                error_tx: None,
                delta_tx: None,
                pending_redemptions: None,
                crdt_state: None,
                admin_identity_pub: None,
                nav_emitter: None,
                root_serve_rx: None,
            });
            tmpdirs.push(tmp);
            nodes.push(SimCommunityNode {
                index: idx0 + 1,
                owner: id.identity.owner,
                device_id: id.device_id.clone(),
                signing_key: Arc::clone(&id.signing),
                state,
                engine,
                tag: id.tag,
                join_hlc: bootstrap_joins[idx0].at.clone(),
            });
        }

        // O(N^2) cross-seed: every node insert-locals EVERY bootstrap Join
        // (including its own; duplicate self-Joins are AlreadyKnown no-ops).
        // Satisfies the membership-at-HLC gate for every future publisher.
        for node in &nodes {
            for join in &bootstrap_joins {
                let outcome = node
                    .engine
                    .insert_local_event(join.clone())
                    .await
                    .expect("cross-seed insert");
                assert!(matches!(
                    outcome,
                    InsertOutcome::Inserted | InsertOutcome::AlreadyKnown
                ));
            }
        }

        // Assemble the bus.
        let partition = Partition::fully_connected();
        let tags: Vec<[u8; 32]> = nodes.iter().map(|nd| nd.tag).collect();
        let bus = SimBus::spawn(out_rxs, in_txs, tags, partition.clone());

        Self {
            community_id,
            admin_owner,
            partition,
            _bus: bus,
            _cas_tx: cas_tx,
            _tmpdirs: tmpdirs,
            nodes,
        }
    }

    pub(crate) fn node(&self, index: usize) -> &SimCommunityNode {
        self.nodes
            .iter()
            .find(|nd| nd.index == index)
            .expect("node index exists")
    }

    /// Partition by 1-based seed-number groups.
    pub(crate) fn split(&self, groups: Vec<Vec<usize>>) {
        let id_groups: Vec<Vec<[u8; 32]>> = groups
            .iter()
            .map(|g| g.iter().map(|i| self.node(*i).tag).collect())
            .collect();
        self.partition.set_split(id_groups);
    }

    pub(crate) fn heal(&self) {
        self.partition.heal();
    }

    /// Advance virtual time, letting engine debounce timers, bus drainers, and
    /// the CAS servicer run. `sleep` (not `advance`) so `start_paused`
    /// auto-advances through each armed timer.
    pub(crate) async fn advance(&self, d: Duration) {
        tokio::time::sleep(d).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    pub(crate) async fn counts(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.nodes.len());
        for nd in &self.nodes {
            v.push(nd.state.lock().await.event_count());
        }
        v
    }

    /// Per-node `(count, digest)` snapshot for the anomaly trajectory.
    pub(crate) async fn sample(&self) -> Vec<Sample> {
        let mut v = Vec::with_capacity(self.nodes.len());
        for nd in &self.nodes {
            let s = nd.state.lock().await;
            v.push(Sample {
                count: s.event_count(),
                digest: event_set_digest(&s),
            });
        }
        v
    }

    /// True iff every node's `CommunityState` is pairwise-equal (the exact
    /// event-set + metadata oracle). Compares two live locked guards — never
    /// clones (`CommunityState` is deliberately not `Clone`).
    pub(crate) async fn all_states_equal(&self) -> bool {
        for k in 1..self.nodes.len() {
            let a = self.nodes[0].state.lock().await;
            let b = self.nodes[k].state.lock().await;
            if *a != *b {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod community_tests {
    use super::*;
    use crate::community_membership::{materialize, MemberStatus};

    #[tokio::test(start_paused = true)]
    async fn baseline_all_nodes_hold_all_joins() {
        let c = SimCommunity::build(4).await;
        // Each node insert-locals its own Join + the other 3 -> 4 events each,
        // with NO bus traffic (cross-seed is direct insert).
        assert_eq!(
            c.counts().await,
            vec![4, 4, 4, 4],
            "each node holds all 4 bootstrap Joins"
        );
        assert!(
            c.all_states_equal().await,
            "all nodes must share an identical baseline CommunityState"
        );
    }

    /// Hand-build a monotone HLC for a mutation: the author has exactly one
    /// prior event (its Join, wall <= 200_000 + idx), so any wall >= 300_000
    /// sorts strictly after it. No reserve ceremony needed.
    fn mutation_hlc(device_id: &str, wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.to_string(),
        }
    }

    async fn poll_counts_eq(c: &SimCommunity, target: usize, rounds: u32) -> bool {
        for _ in 0..rounds {
            if c.counts().await.iter().all(|&x| x == target) {
                return true;
            }
            c.advance(Duration::from_millis(100)).await;
        }
        c.counts().await.iter().all(|&x| x == target)
    }

    #[tokio::test(start_paused = true)]
    async fn membership_partition_heal_reconverges() {
        let c = SimCommunity::build(6).await;
        let mut trajectory: Vec<Vec<Sample>> = vec![c.sample().await]; // baseline
        assert_eq!(
            c.counts().await,
            vec![6; 6],
            "baseline: all 6 Joins everywhere"
        );
        assert!(c.all_states_equal().await, "baseline states equal");

        // Partition {1,2,3} | {4,5,6}.
        c.split(vec![vec![1, 2, 3], vec![4, 5, 6]]);

        // Island A: admin (node 1) kicks node 2.
        let kick = {
            let n1 = c.node(1);
            crate::mint_kick_event(
                c.community_id,
                n1.owner,
                c.node(2).owner,
                Some("sim-kick".into()),
                &n1.signing_key,
                mutation_hlc(&n1.device_id, 300_000),
            )
            .expect("mint kick")
        };
        assert!(matches!(
            c.node(1)
                .engine
                .insert_local_event(kick)
                .await
                .expect("insert kick"),
            InsertOutcome::Inserted
        ));

        // Island B: node 4 self-leaves.
        let leave = {
            let n4 = c.node(4);
            crate::mint_leave_event(
                c.community_id,
                n4.owner,
                &n4.signing_key,
                mutation_hlc(&n4.device_id, 400_000),
            )
            .expect("mint leave")
        };
        assert!(matches!(
            c.node(4)
                .engine
                .insert_local_event(leave)
                .await
                .expect("insert leave"),
            InsertOutcome::Inserted
        ));

        // Force intra-island publishes and let same-island peers merge.
        c.node(1).engine.flush_now().await.expect("flush n1");
        c.node(4).engine.flush_now().await.expect("flush n4");
        assert!(
            poll_counts_eq(&c, 7, 50).await,
            "each island should reach 7 events (baseline + its own mutation): {:?}",
            c.counts().await
        );
        trajectory.push(c.sample().await); // partitioned phase

        // Divergence proof: island A holds the kick, island B holds the leave.
        assert!(
            !c.all_states_equal().await,
            "islands must diverge under partition (A has kick, B has leave)"
        );

        // Heal + REQUIRED post-heal republish from each mutator (the pub/sub
        // plane has no anti-entropy — advancing time alone re-delivers nothing).
        c.heal();
        c.node(1).engine.flush_now().await.expect("reflush n1");
        c.node(4).engine.flush_now().await.expect("reflush n4");

        assert!(
            poll_counts_eq(&c, 8, 50).await,
            "all nodes should reconverge to 8 events after heal: {:?}",
            c.counts().await
        );
        trajectory.push(c.sample().await); // healed phase

        // Global convergence: identical CommunityState, both mutations applied.
        assert!(
            c.all_states_equal().await,
            "all nodes converge to identical state"
        );
        let events: Vec<_> = {
            let s = c.node(6).state.lock().await;
            s.events().cloned().collect()
        };
        let mat = materialize(&events, c.admin_owner);
        assert_eq!(
            mat.members.get(&c.node(2).owner).map(|m| m.status),
            Some(MemberStatus::Banned),
            "node 2 kicked -> Banned everywhere"
        );
        assert_eq!(
            mat.members.get(&c.node(4).owner).map(|m| m.status),
            Some(MemberStatus::Left),
            "node 4 left -> Left everywhere"
        );

        // The anomaly analyzer sees a clean terminal state.
        let anomalies = super::super::anomaly::analyze(&trajectory);
        assert!(
            anomalies.is_empty(),
            "no terminal anomalies expected, got {anomalies:?}"
        );
    }
}
