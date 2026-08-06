//! ZEB-285 Phase 1 Task 6: `fork_community` Tauri IPC + snapshot builder.
//!
//! Lands the user-facing fork primitive. When invoked:
//! 1. Validates forker is Joined in the original community.
//! 2. Generates a fresh fork SpaceId.
//! 3. Builds a PreForkSnapshot from local view (§4.2 snapshot policy).
//! 4. Creates the fork's bootstrap state (forker self-Join + #general).
//! 5. Sets `CommunityState.forked_from = Some(original_id)` on the fork.
//! 6. Writes `pre_fork_snapshot.bin` to the fork's data dir (atomic rename).
//! 7. If `!silent`: mints + publishes Fork event in the original.
//! 8. If `also_leave`: mints + publishes Leave event in the original.
//! 9. Emits `community-forked` frontend event.

pub const SNAPSHOT_PER_CHANNEL_CAP: usize = 500;
pub const SNAPSHOT_TOTAL_CAP: usize = 5000;

/// §4.2 snapshot policy: per-channel cap + proportional total trim.
///
/// 1. Per-channel cap: for each channel, sort descending by HLC, take
///    top `SNAPSHOT_PER_CHANNEL_CAP` (500).
/// 2. If total > `SNAPSHOT_TOTAL_CAP` (5000): proportional trim —
///    `floor(slice_len × total_cap / total)` per channel; the largest-
///    slice channel absorbs the remainder so the total lands in
///    `[total_cap − channel_count, total_cap]`.
/// 3. Sort each retained slice back to HLC ascending for output.
///
/// Accepts a map from `ChannelId` → raw event vec. Returns the trimmed
/// snapshot map.
pub fn build_snapshot(
    raw: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        Vec<crate::community_channel_log::SignedChannelEvent>,
    >,
) -> std::collections::BTreeMap<
    crate::community_membership::ChannelId,
    Vec<crate::community_channel_log::SignedChannelEvent>,
> {
    // Step 1: per-channel cap — sort descending by HLC, take top N=500,
    // re-sort ascending for intermediate use. We keep ascending order
    // throughout to simplify the proportional trim step, then Step 3
    // ensures final ascending order.
    let mut capped: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        Vec<crate::community_channel_log::SignedChannelEvent>,
    > = raw
        .into_iter()
        .filter_map(|(ch, mut events)| {
            if events.is_empty() {
                return None;
            }
            // Sort descending by HLC to pick the newest events.
            events.sort_by(|a, b| {
                let at_a = event_hlc(a);
                let at_b = event_hlc(b);
                hlc_cmp(at_b, at_a) // reversed for descending
            });
            events.truncate(SNAPSHOT_PER_CHANNEL_CAP);
            // Re-sort ascending for consistent ordering.
            events.sort_by(|a, b| hlc_cmp(event_hlc(a), event_hlc(b)));
            Some((ch, events))
        })
        .collect();

    // Step 2: proportional total trim if needed.
    let total: usize = capped.values().map(|v| v.len()).sum();
    if total > SNAPSHOT_TOTAL_CAP {
        // Compute floor(slice_len * TOTAL_CAP / total) for each channel.
        let keys: Vec<crate::community_membership::ChannelId> = capped.keys().copied().collect();
        let mut allocations: Vec<usize> = keys
            .iter()
            .map(|k| capped[k].len() * SNAPSHOT_TOTAL_CAP / total)
            .collect();

        // Distribute remainder across channels in order of descending available
        // capacity (slice_len - current_allocation), giving each at most its
        // remaining capacity until remainder reaches 0.
        // (Fix: PR #122 round-4 bot review — CodeRabbit inline.)
        let allocated_sum: usize = allocations.iter().sum();
        let mut remainder = SNAPSHOT_TOTAL_CAP.saturating_sub(allocated_sum);
        if remainder > 0 {
            let mut by_capacity: Vec<usize> = (0..keys.len()).collect();
            by_capacity.sort_by_key(|&i| {
                std::cmp::Reverse(capped[&keys[i]].len().saturating_sub(allocations[i]))
            });
            for &i in &by_capacity {
                if remainder == 0 {
                    break;
                }
                let available = capped[&keys[i]].len().saturating_sub(allocations[i]);
                let take = available.min(remainder);
                allocations[i] += take;
                remainder -= take;
            }
        }

        for (k, alloc) in keys.iter().zip(allocations) {
            let events = capped.get_mut(k).expect("key present");
            // Keep the newest `alloc` events (already ascending; take from the end).
            let len = events.len();
            if alloc < len {
                events.drain(..len - alloc);
            }
        }
    }

    // Step 3: ensure final HLC-ascending sort per channel (guaranteed
    // by Step 1's re-sort, but explicitly enforced for correctness).
    for events in capped.values_mut() {
        events.sort_by(|a, b| hlc_cmp(event_hlc(a), event_hlc(b)));
    }

    capped
}

/// Extract the HLC timestamp from a `SignedChannelEvent`.
fn event_hlc(
    event: &crate::community_channel_log::SignedChannelEvent,
) -> &crate::owner_state_types::Hlc {
    event.at()
}

/// Compare two HLCs: wall_ms first, then logical, then device_id
/// (lexicographic — stable but arbitrary among same-wall-same-logical).
fn hlc_cmp(
    a: &crate::owner_state_types::Hlc,
    b: &crate::owner_state_types::Hlc,
) -> std::cmp::Ordering {
    a.wall_ms
        .cmp(&b.wall_ms)
        .then(a.logical.cmp(&b.logical))
        .then(a.device_id.cmp(&b.device_id))
}

/// Caller-supplied options for `fork_community`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkCommunityOpts {
    /// Display name for the new fork community.
    pub name: String,
    /// ZEB-649: the forker's stated why — mandatory at this layer (no serde
    /// default; the dialog always sends it). Validated non-empty and
    /// ≤ MAX_MODERATION_REASON_CHARS before any state change. The wire
    /// event carries it as `Option` purely for decode-compat with
    /// pre-ZEB-649 events.
    pub reason: String,
    /// If `true`, do NOT mint/publish a Fork event in the original.
    /// The fork is "silent" — original members won't see a fork annotation.
    /// The reason is still required and still lands on the fork's own
    /// lineage state: silent means "don't notify the parent", not
    /// "the fork has no why".
    #[serde(default)]
    pub silent: bool,
    /// If `true`, additionally mint/publish a Leave event in the original
    /// after creating the fork.
    #[serde(default)]
    pub also_leave: bool,
}

/// Result returned to the frontend on success.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkCommunityResult {
    /// Hex-encoded SpaceId of the newly created fork community.
    pub fork_space_id: String,
    /// True iff the Fork event was published into the original community.
    pub visible: bool,
    /// Total message count captured in the PreForkSnapshot (after trim).
    pub snapshot_message_count: usize,
}

/// Pure helper: mint a signed Fork event referencing `fork_space_id` into
/// the **original** community's event log.
///
/// Mirrors `mint_kick_event` / `mint_leave_event` shape: pure / sync /
/// no I/O. Caller pre-reserves `hlc`.
pub fn mint_fork_event(
    original_community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    fork_space_id: crate::owner_state_types::SpaceId,
    reason: String,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    // ZEB-649 (CodeRabbit PR #434): validate at the function boundary, not
    // just the IPC gate — mint_fork_event is pub, and a future caller must
    // not be able to mint an empty/oversize reason that verify_event on
    // every replica would then reject.
    let reason = validate_fork_reason(&reason)?;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id: original_community_id,
        kind: MembershipEventKind::Fork {
            fork_space_id,
            // ZEB-649: always Some for newly minted events; the Option is a
            // decode-compat shape, not an invitation to mint without one.
            reason: Some(reason),
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign fork: {e}"))
}

/// ZEB-649: pure validation for the mandatory fork reason. Trims, rejects
/// empty, and enforces the same codepoint cap `verify_event` applies on
/// inbound events (defense-in-depth twin). Returns the trimmed reason.
pub fn validate_fork_reason(raw: &str) -> Result<String, String> {
    let reason = raw.trim().to_string();
    if reason.is_empty() {
        return Err("fork reason must not be empty".to_string());
    }
    if reason.chars().count() > crate::community_membership::MAX_MODERATION_REASON_CHARS {
        return Err(format!(
            "fork reason exceeds {} characters",
            crate::community_membership::MAX_MODERATION_REASON_CHARS
        ));
    }
    Ok(reason)
}

/// Tauri IPC: fork a community the local node is currently Joined in.
///
/// Creates a new fork community (own SpaceId, bootstrap Join + #general),
/// writes a `PreForkSnapshot` to disk, and optionally publishes Fork/Leave
/// events into the original community. Returns `ForkCommunityResult` on
/// success.
///
/// Power threshold: any Joined member (power ≥ 0) — enforced structurally
/// by requiring Joined membership status, not by engine's verify_event
/// (Fork carries no power guard; the Fork event itself is gated at insertion
/// time by verify_event's Joined-actor check).
#[tauri::command]
pub async fn fork_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
    community_id: String,
    opts: ForkCommunityOpts,
) -> Result<ForkCommunityResult, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let original_id = crate::owner_state_types::SpaceId(id_bytes);

    // ZEB-649: reason is mandatory — validate before ANY state change so a
    // rejected fork leaves nothing behind.
    let reason = validate_fork_reason(&opts.reason)?;

    // Snapshot all needed handles from NodeState under a single std lock scope.
    let (
        crdt_state,
        hlc_tracker,
        adopt_floor,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        transport_epoch_rx,
        channel_log_registry,
        dm_outbox,
        sync_engine,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.hlc_adopt_floor.clone(),
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            // ZEB-434 D6: pass-through Option (no ok_or) — a missing
            // receiver degrades to a non-re-arming fetch driver rather
            // than failing the IPC.
            g.transport_epoch_rx.clone(),
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            // ZEB-709 (audit A1): the owner-state engine — the fork's Space
            // commit below must fence its flush or the forked community's
            // owner-state row evaporates on a crash (its twin
            // create_community has fenced since ZEB-393). Option, not ok_or:
            // engineless test states must not fail the fork.
            g.sync_engine.clone(),
            g.generation,
        )
    }; // std lock released here.

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Step 1: validate forker is Joined in the original. Read materialized
    // state from the original community's engine.
    let original_engine = community_registry
        .engine_arc(&original_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for original community {} — not currently joined",
                hex::encode(original_id.0)
            )
        })?;

    let (
        original_name,
        original_events_vec,
        materialized,
        original_parent_lineage,
        original_forked_at_wall_ms,
        original_fork_reason,
    ) = {
        let state_arc = original_engine.state();
        let state_g = state_arc.lock().await;
        let admin_addr = original_engine.admin_addr();
        let mat = state_g.materialize_now(admin_addr);
        let member_info = mat.members.get(&self_owner).ok_or_else(|| {
            "fork_community: caller is not a member of the original community".to_string()
        })?;
        if !matches!(
            member_info.status,
            crate::community_membership::MemberStatus::Joined
        ) {
            return Err(
                "fork_community: caller is not currently Joined in the original community"
                    .to_string(),
            );
        }
        // Collect membership events for the snapshot.
        let events: Vec<crate::community_membership::SignedMembershipEvent> =
            state_g.events().cloned().collect();
        // ZEB-287 Task 4: capture forker's lineage + forked_at_wall_ms so we
        // can extend the chain into the new fork's PreForkSnapshot.
        let forker_parent_lineage = state_g.parent_lineage.clone();
        let forker_forked_at_wall_ms = state_g.forked_at_wall_ms;
        // ZEB-649: the original's OWN fork_reason rides on its lineage
        // entry (why IT split from its parent), keeping reasons stacked
        // down the chain.
        let forker_fork_reason = state_g.fork_reason.clone();
        // Derive original community name from owner-state crdt.
        let name = {
            let crdt_g = crdt_state.lock().await;
            crdt_g
                .spaces
                .get(&original_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| hex::encode(original_id.0))
        };
        (
            name,
            events,
            mat,
            forker_parent_lineage,
            forker_forked_at_wall_ms,
            forker_fork_reason,
        )
    };

    // Step 2a: Start building the signer address set from membership events.
    // CORRECTNESS: build the signer set from membership_events, NOT from
    // materialized.members. The bootstrap admin may have no explicit Join
    // event and therefore no entry in materialized.members, but they DO sign
    // other events (e.g., Invite, Kick, SetPower). Omitting their pubkey here
    // would cause verify_snapshot_event to return UnknownSigner for any event
    // they authored. (Bot review finding: PR #122 CodeRabbit / Qodo.)
    let mut signer_addrs = std::collections::BTreeSet::new();
    for event in &original_events_vec {
        signer_addrs.insert(event.actor);
        // Also include any countersigner (invite-only Join vouchers).
        if let Some(ref cs) = event.countersig {
            signer_addrs.insert(cs.signer);
        }
    }

    // Step 3: Build the BoundedChannelLogSnapshot from local channel engines.
    // Collect raw events per channel (up to a generous cap to avoid OOM;
    // build_snapshot applies the §4.2 policy).
    let mut raw_channel_events: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        Vec<crate::community_channel_log::SignedChannelEvent>,
    > = std::collections::BTreeMap::new();

    for (channel_id, ch_info) in &materialized.channels {
        if ch_info.deleted_at.is_some() {
            continue;
        }
        if let Some(engine) = channel_log_registry.engine(&original_id, channel_id).await {
            // ZEB-536: the pre-fork snapshot is message-only for v1. Pull Post
            // events via the Post-only accessor, which pages by POSTS RETURNED
            // — a long reaction run cannot exhaust the pull budget and strand
            // later posts (CodeRabbit PR #314). Cap at SNAPSHOT_TOTAL_CAP * 2
            // posts (build_snapshot trims further per the §4.2 policy).
            // Best-effort snapshot: one channel's read failure must not abort
            // the whole fork, but it must not be silent either (CodeAnt PR
            // #314) — surface it so an incomplete snapshot is diagnosable
            // rather than a silent history drop.
            let events = match engine.list_post_events(None, SNAPSHOT_TOTAL_CAP * 2).await {
                Ok(evs) => evs,
                Err(e) => {
                    tracing::warn!(
                        channel_id = ?channel_id,
                        error = %e,
                        "fork snapshot: channel log read failed; omitting its history from the snapshot"
                    );
                    Vec::new()
                }
            };
            if !events.is_empty() {
                raw_channel_events.insert(*channel_id, events);
            }
        }
    }

    // Step 2b: Extend the signer set with channel-log event authors.
    // Channel-log events are signed by their `author` field (OwnerAddr), which
    // may differ from membership-event actors. Without their pubkeys in
    // identity_pubs, verify_snapshot_event would return UnknownSigner on those
    // events. Channel-log events (Post and React) are author-signed, not
    // countersigned; the snapshot is Post-only by the filter above.
    // (Fix: PR #122 round-2 bot review — CodeRabbit Major.)
    for log_events in raw_channel_events.values() {
        for event in log_events {
            signer_addrs.insert(*event.author());
        }
    }

    // Step 2c: Resolve identity_pubs for all snapshot signers.
    let resolver = community_registry.identity_resolver();
    let mut identity_pubs: std::collections::BTreeMap<
        crate::owner_state_types::OwnerAddr,
        [u8; 64],
    > = std::collections::BTreeMap::new();
    for addr in &signer_addrs {
        if let Some(pub64) = resolver.resolve(addr).await {
            identity_pubs.insert(*addr, pub64);
        } else {
            tracing::warn!(
                signer = %hex::encode(addr.0),
                original_id = %hex::encode(original_id.0),
                "fork_community: cannot resolve identity_pub for snapshot signer; \
                 snapshot events from this actor will fail verification at display time"
            );
        }
    }

    let trimmed_channel_events = build_snapshot(raw_channel_events);
    let snapshot_message_count: usize = trimmed_channel_events.values().map(|v| v.len()).sum();

    let channel_log_snapshot = crate::community_invite::BoundedChannelLogSnapshot {
        per_channel: trimmed_channel_events,
    };

    // Reserve HLC for the fork creation (used as `forked_at` timestamp).
    let fork_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
        &hlc_tracker,
        &adopt_floor,
        &device_id,
        wall_now_ms,
    )
    .await;

    // Step 4: Mint the fork community (bootstrap Join + #general).
    // Uses create_community_inner with is_invite_only=false (fork can set
    // its own access policy post-creation; Task 8 may extend this).
    //
    // ZEB-339: the fork's bootstrap Join + #general ChannelCreate are
    // community-membership events — sign with the enrolled device key (#2).
    // The bootstrap Join is identity-introducing → carries this device's own
    // EnrollmentCert; the ChannelCreate is steady-state → no cert.
    let (signing_key, enrollment_cert) = {
        let outbox_g = dm_outbox.lock().await;
        (
            std::sync::Arc::clone(&outbox_g.community_signing_key),
            outbox_g.enrollment_cert.clone(),
        )
    };

    // Mint the fork community — this internally generates the fork SpaceId.
    // We need the fork SpaceId BEFORE calling create_community_inner so we can
    // set forked_from. We use mint_community_creation then call the inner steps
    // manually, mirroring create_community_inner's own structure.
    //
    // Since create_community_inner is the authoritative path for all the
    // locking, channel-log transactions, and RAII guards, we call it directly
    // and then patch forked_from in owner-state afterward.
    //
    // HOWEVER: we need the fork SpaceId BEFORE create_community_inner to:
    //   a) Set forked_from on the new community's CommunityState.
    //   b) Write the PreForkSnapshot to the fork's data dir.
    //   c) Mint the Fork event for the original (referencing fork_space_id).
    //
    // Strategy: mint the community struct first (which generates the id), then
    // call create_community_inner with the same name but let it re-generate a
    // NEW id (that id is what gets persisted). Then we patch owner-state.
    //
    // BETTER strategy: call mint_community_creation ourselves, then replicate
    // the create_community_inner steps using the pre-minted result. This is
    // what we do below.

    let fork_hlc_for_create = crate::dm_outbox::reserve_next_hlc_for_device(
        &hlc_tracker,
        &adopt_floor,
        &device_id,
        wall_now_ms,
    )
    .await;

    let minted = crate::mint_community_creation(
        &opts.name,
        false, // is_invite_only = false (fork starts open; Task 8 can extend)
        self_owner,
        signing_key.as_ref(),
        &enrollment_cert,
        fork_hlc_for_create,
    )?;

    let fork_space_id = minted.community_id;
    let fork_space_id_hex = hex::encode(fork_space_id.0);

    // ZEB-287 Task 4 / spec §3.4: build the ancestor chain for the new fork
    // via the shared `build_parent_lineage` helper. The helper extends the
    // forker's lineage with a new entry for the forker community itself,
    // then applies the 16-deep cap. (R1-4: shared helper so tests + prod
    // exercise the same code path.)
    let new_parent_lineage = crate::community_invite::build_parent_lineage(
        &original_parent_lineage,
        original_id,
        &original_name,
        original_forked_at_wall_ms,
        original_fork_reason,
    );

    // Step 5: Build PreForkSnapshot (with correct fork_space_id already known).
    let pre_fork_snapshot = crate::community_invite::PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: original_name.clone(),
        membership_events: original_events_vec,
        channel_log: channel_log_snapshot,
        identity_pubs,
        forked_at: fork_hlc.clone(),
        // R1-1b: clone — we also mutate the new fork's CommunityState
        // below with this same chain.
        parent_lineage: new_parent_lineage.clone(),
        // ZEB-649: THIS fork's why — joiners mirror it into their
        // CommunityState.fork_reason at redeem-time.
        fork_reason: Some(reason.clone()),
    };

    // Step 6 (pre-spawn): write pre_fork_snapshot.bin to the fork's data dir.
    // The directory is `identity_dir/communities/{fork_space_id_hex}/`.
    // We write it BEFORE create_community_inner creates the engine, but
    // the create_community_inner path does `create_dir_all` on the directory
    // when saving crdt.cbor. We do `create_dir_all` ourselves here so the
    // file write doesn't fail if the directory doesn't exist yet.
    {
        let identity_dir = crate::owner_commands::resolve_identity_dir()
            .map_err(|e| format!("fork_community: cannot resolve identity_dir: {e}"))?;
        let fork_dir = identity_dir.join("communities").join(&fork_space_id_hex);
        std::fs::create_dir_all(&fork_dir)
            .map_err(|e| format!("fork_community: create fork dir: {e}"))?;

        // Serialize snapshot to canonical CBOR.
        let snapshot_bytes = crate::owner_state_crypto::canonical_cbor_encode(&pre_fork_snapshot)
            .map_err(|e| format!("fork_community: encode snapshot: {e}"))?;

        // Atomic write via tempfile + rename.
        let snapshot_path = fork_dir.join("pre_fork_snapshot.bin");
        let tmp_path = fork_dir.join("pre_fork_snapshot.bin.tmp");
        std::fs::write(&tmp_path, &snapshot_bytes)
            .map_err(|e| format!("fork_community: write snapshot tmp: {e}"))?;
        std::fs::rename(&tmp_path, &snapshot_path)
            .map_err(|e| format!("fork_community: rename snapshot: {e}"))?;
    }

    // Now spawn the fork community engine + persist owner-state space.
    // We replicate the relevant steps of create_community_inner:
    //   - channel-log transaction guard
    //   - community-sync spawn guard
    //   - channel pair setup
    //   - engine spawn + bootstrap Join insert + default #general insert
    //   - generation fence
    //   - owner-state apply_space (with forked_from set)
    //   - commit guards

    let channel_log_tx = channel_log_registry.begin_transaction(fork_space_id);
    let mut community_sync_guard = community_registry.begin_spawn_guard(fork_space_id);

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    // ZEB-434: queryable-serve + root-fetch pairs (engine halves into
    // the spawn, adapter halves into the CommunityAdapterRequest).
    let (root_serve_tx, root_serve_rx) =
        tokio::sync::mpsc::channel::<crate::community_state_sync::RootServeRequest>(8);
    let (fetch_request_tx, fetch_request_rx) =
        tokio::sync::mpsc::channel::<crate::event_loop::CommunityRootFetchRequest>(4);

    let engine_arc = community_registry
        .spawn_engine_with_guard(
            &mut community_sync_guard,
            fork_space_id,
            minted.membership_key.clone(),
            self_owner,
            false, // is_invite_only
            pub_tx,
            sub_rx,
            pub_rx,
            sub_tx,
            community_adapter_tx,
            crate::community_state_sync::CatchUpChannels {
                root_serve_rx: Some(root_serve_rx),
                fetch_request_tx: Some(fetch_request_tx),
                transport_epoch_rx,
            },
            root_serve_tx,
            fetch_request_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine_with_guard: {e}"))?;

    // Helper: best-effort cleanup of disk artifacts written before engine spawn.
    // Called on early-return paths between snapshot write and guard commit.
    let snapshot_path_for_cleanup = crate::owner_commands::resolve_identity_dir()
        .map(|d| {
            d.join("communities")
                .join(&fork_space_id_hex)
                .join("pre_fork_snapshot.bin")
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("<identity_dir unavailable>"));
    let cleanup_disk_artifacts = || {
        let parent_dir = snapshot_path_for_cleanup.parent();
        if let Err(e) = std::fs::remove_file(&snapshot_path_for_cleanup) {
            tracing::warn!(
                error = ?e,
                path = %snapshot_path_for_cleanup.display(),
                "ZEB-285 fork_community: failed to remove orphaned pre_fork_snapshot.bin during rollback"
            );
        }
        if let Some(parent) = parent_dir {
            if let Err(e) = std::fs::remove_dir(parent) {
                tracing::debug!(
                    error = ?e,
                    path = %parent.display(),
                    "ZEB-285 fork_community: didn't remove orphaned community dir during rollback (likely non-empty or in use)"
                );
            }
        }
    };

    // Insert bootstrap Join.
    let bootstrap_outcome = engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .map_err(|e| {
            cleanup_disk_artifacts();
            format!("engine.insert_local_event (bootstrap_join): {e}")
        })?;
    if !matches!(
        bootstrap_outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        cleanup_disk_artifacts();
        return Err(format!(
            "fork bootstrap Join not inserted (got {bootstrap_outcome:?})"
        ));
    }

    // Insert default #general channel.
    let default_channel_id: crate::community_membership::ChannelId = {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        crate::community_membership::ChannelId(buf)
    };
    let default_channel_event_id: crate::community_membership::EventId = {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    };
    let default_channel_at = crate::dm_outbox::reserve_next_hlc_for_device(
        &hlc_tracker,
        &adopt_floor,
        &device_id,
        wall_now_ms,
    )
    .await;
    let default_channel_payload = crate::community_membership::EventPayload {
        id: default_channel_event_id,
        community_id: fork_space_id,
        kind: crate::community_membership::MembershipEventKind::ChannelCreate {
            channel_id: default_channel_id,
            name: "general".to_string(),
            write_power: 0,
            // ZEB-349: fork default channel is always Text. (Voice channels are
            // member-created via create_channel; a later slice wires the real
            // value through the IPC.)
            kind: crate::community_membership::ChannelKind::Text,
        },
        actor: self_owner,
        at: default_channel_at,
    };
    let default_channel_signed =
        crate::community_membership::sign_event(&default_channel_payload, signing_key.as_ref())
            .map_err(|e| format!("sign fork default-channel ChannelCreate: {e}"))?;

    let default_channel_outcome = engine_arc
        .insert_local_event(default_channel_signed)
        .await
        .map_err(|e| {
            cleanup_disk_artifacts();
            format!("engine.insert_local_event (fork default channel): {e}")
        })?;
    if !matches!(
        default_channel_outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        cleanup_disk_artifacts();
        return Err(format!(
            "fork default-channel ChannelCreate not inserted (got {default_channel_outcome:?})"
        ));
    }

    // Set forked_from + Phase 2 lineage fields on the fork's CommunityState
    // via the engine. (R1-1b: spec §3.5 mandates persisting parent_lineage +
    // forked_at_wall_ms for both fork-redeem AND local-fork-create paths so
    // a subsequent fork of THIS new community reads the correct ancestry.)
    {
        let state_arc = engine_arc.state();
        let mut state_g = state_arc.lock().await;
        state_g.forked_from = Some(original_id);
        state_g.forked_at_wall_ms = Some(fork_hlc.wall_ms);
        state_g.parent_lineage = new_parent_lineage;
        // ZEB-649: the fork's own why, for its lineage/divider UI and for
        // the lineage entry it hands to ITS future forks.
        state_g.fork_reason = Some(reason.clone());
    }

    // Generation fence (mirrors create_community_inner).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            cleanup_disk_artifacts();
            return Err(format!(
                "ZEB-285 fork_community: node generation changed after engine spawn and snapshot \
                 write; engine rolled back by guard; best-effort cleanup of disk artifacts at {} \
                 attempted; original community untouched",
                snapshot_path_for_cleanup.display()
            ));
        }
        if g.community_registry.is_none() {
            cleanup_disk_artifacts();
            return Err(format!(
                "ZEB-285 fork_community: community_registry was torn down after engine spawn and \
                 snapshot write; engine rolled back by guard; best-effort cleanup of disk \
                 artifacts at {} attempted; original community untouched",
                snapshot_path_for_cleanup.display()
            ));
        }
    }

    // The fork's Space row is the minted space as-is.
    // Note: Space does not carry forked_from directly — that lives in
    // CommunityState. The Space row is committed to owner-state as-is;
    // the forked_from lineage is in the community engine's CommunityState
    // (set above) and in the PreForkSnapshot on disk.
    let fork_space = minted.space.clone();

    // Commit owner-state (LAST persistent step per ZEB-258).
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(fork_space);
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            return Err(format!("apply_space rejected fork community: {outcome:?}"));
        }
    }

    // ZEB-709 (audit A1): durable-on-commit for the fork's owner-state Space
    // row — the community engine's snapshot is already fenced above, but
    // without THIS fence the owner-state row (nav entry, members,
    // content_key) rides an unarmed debounce and a crash forgets the fork
    // from nav while its engine state exists on disk. Mirrors
    // create_community's ZEB-393 fence exactly.
    if let Some(engine) = sync_engine.as_ref() {
        crate::fence_owner_state_flush(
            engine,
            crate::OWNER_STATE_FENCE_TIMEOUT,
            "fork_community",
            &fork_space_id_hex,
        )
        .await;
    }

    // Release RAII guards.
    community_sync_guard.commit();

    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            fork_space_id = %fork_space_id_hex,
            error = %e,
            "fork_community: channel_log_registry commit failed after durable fork create; \
             pending channel-log spawns will be re-attempted via reconcile_from_state at next start_node"
        );
    }

    // Step 7: if !silent, mint + publish Fork event in the original.
    // Per spec §5.2: transport-level failures here must warn-and-continue with
    // visible=false; the fork is already durably on disk.
    let mut visible = !opts.silent;
    if visible {
        let fork_event_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
            &hlc_tracker,
            &adopt_floor,
            &device_id,
            wall_now_ms,
        )
        .await;
        // RELIABILITY: wrap mint_fork_event in a match so a signing failure
        // after the fork is durably committed does not propagate via `?` and
        // surface as an IPC error (which would mislead the caller into thinking
        // the fork itself failed). Warn and set visible=false instead, then
        // continue to the also_leave + frontend-emit steps.
        // (Fix: PR #122 round-2 bot review — early return was wrong.)
        let maybe_fork_event = {
            let outbox_g = dm_outbox.lock().await;
            // ZEB-339: Fork is a steady-state community-membership event in the
            // original community — enrolled device key (#2), no cert.
            match mint_fork_event(
                original_id,
                self_owner,
                fork_space_id,
                reason.clone(),
                outbox_g.community_signing_key.as_ref(),
                fork_event_hlc,
            ) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        original_id = %hex::encode(original_id.0),
                        fork_space_id = %fork_space_id_hex,
                        "ZEB-285 fork_community: mint_fork_event failed after fork commit; \
                         skipping Fork event publish"
                    );
                    visible = false;
                    None
                }
            }
        };

        if let Some(fork_event) = maybe_fork_event {
            // Generation + registry fence before inserting Fork event into the
            // original engine. Extract verdict under the std lock, drop the guard,
            // then act on the verdict. Pattern mirrors kick_from_community's
            // post-mint fence (lib.rs:11826-11842).
            enum ForkPublishFence {
                Proceed,
                GenerationChanged,
                RegistryGone,
            }
            let fork_publish_fence = {
                let g = state_lock
                    .lock()
                    .map_err(|e| format!("NodeState poisoned: {e}"))?;
                if g.generation != snapshot_generation {
                    ForkPublishFence::GenerationChanged
                } else if g.community_registry.is_none() {
                    ForkPublishFence::RegistryGone
                } else {
                    ForkPublishFence::Proceed
                }
            }; // std lock guard dropped here.

            match fork_publish_fence {
                ForkPublishFence::GenerationChanged => {
                    tracing::warn!(
                        original_id = %hex::encode(original_id.0),
                        fork_space_id = %fork_space_id_hex,
                        "ZEB-285 fork_community: node generation changed before Fork event publish; skipping"
                    );
                    visible = false;
                }
                ForkPublishFence::RegistryGone => {
                    tracing::warn!(
                        original_id = %hex::encode(original_id.0),
                        fork_space_id = %fork_space_id_hex,
                        "ZEB-285 fork_community: community_registry detached before Fork event publish; skipping"
                    );
                    visible = false;
                }
                ForkPublishFence::Proceed => {
                    match original_engine.insert_local_event(fork_event).await {
                        Ok(outcome) => {
                            if !matches!(
                                outcome,
                                crate::community_state_crdt::InsertOutcome::Inserted
                                    | crate::community_state_crdt::InsertOutcome::AlreadyKnown
                            ) {
                                tracing::warn!(
                                    original_id = %hex::encode(original_id.0),
                                    "fork_community: Fork event rejected by original engine: {outcome:?}"
                                );
                                // Per spec §5.2: a Rejected outcome means the Fork event did not
                                // land in the original's log — treat as not-visible so the caller
                                // knows the announce did not take effect. (Fix: PR #122 bot review.)
                                visible = false;
                            }
                        }
                        Err(e) => {
                            // Per spec §5.2: transport/IO failure publishing Fork event must not
                            // tear down the fork (already on disk). Log + set visible=false so the
                            // caller knows the announce did not land; forker can retry from settings.
                            tracing::warn!(
                                error = ?e,
                                original_id = %hex::encode(original_id.0),
                                fork_space_id = %fork_space_id_hex,
                                "ZEB-285: failed to publish Fork event to original; fork is still on disk locally"
                            );
                            visible = false;
                        }
                    }
                }
            }
        } // end if let Some(fork_event) = maybe_fork_event
    }

    // Step 8: if also_leave, mint + publish Leave event in the original.
    if opts.also_leave {
        // Generation + registry fence before inserting Leave event — mirrors
        // Step 7's Fork-event fence. If generation changed or registry is gone,
        // the fork is already durably committed; Leave failure is a degraded
        // outcome but not fatal — log and skip rather than returning Err.
        let leave_generation_ok = {
            match state_lock.lock() {
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        original_id = %hex::encode(original_id.0),
                        "ZEB-285 fork_community: NodeState poisoned before Leave fence; skipping Leave publish"
                    );
                    false
                }
                Ok(g) => {
                    if g.generation != snapshot_generation {
                        tracing::warn!(
                            original_id = %hex::encode(original_id.0),
                            fork_space_id = %fork_space_id_hex,
                            "ZEB-285 fork_community: node generation changed before Leave event publish; skipping"
                        );
                        false
                    } else if g.community_registry.is_none() {
                        tracing::warn!(
                            original_id = %hex::encode(original_id.0),
                            fork_space_id = %fork_space_id_hex,
                            "ZEB-285 fork_community: community_registry detached before Leave event publish; skipping"
                        );
                        false
                    } else {
                        true
                    }
                }
            }
        }; // std lock guard dropped here.

        if leave_generation_ok {
            let leave_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
                &hlc_tracker,
                &adopt_floor,
                &device_id,
                wall_now_ms,
            )
            .await;
            // RELIABILITY: wrap mint_leave_event in a match so a signing failure
            // does not propagate via `?` after the fork is durably committed.
            // Leave is a best-effort courtesy event; warn and skip on failure.
            // Use Option-binding (not early return) so the frontend-event-emit
            // in Step 9 still runs. (Fix: PR #122 round-2 bot review.)
            let maybe_leave_event = {
                let outbox_g = dm_outbox.lock().await;
                // ZEB-339: Leave is a steady-state community-membership event —
                // enrolled device key (#2), no cert.
                match crate::mint_leave_event(
                    original_id,
                    self_owner,
                    outbox_g.community_signing_key.as_ref(),
                    leave_hlc,
                ) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            original_id = %hex::encode(original_id.0),
                            "ZEB-285 fork_community: mint_leave_event failed; skipping Leave publish"
                        );
                        None
                    }
                }
            };
            if let Some(leave_event) = maybe_leave_event {
                match original_engine.insert_local_event(leave_event).await {
                    Ok(outcome) => {
                        if matches!(
                            outcome,
                            crate::community_state_crdt::InsertOutcome::Rejected(_)
                        ) {
                            tracing::warn!(
                                original_id = %hex::encode(original_id.0),
                                "fork_community: also_leave Leave event rejected by original engine: {outcome:?}"
                            );
                        }
                    }
                    Err(e) => {
                        // Per spec §5.2: transport/IO failure publishing Leave event must not tear down
                        // the fork (already on disk). Log + continue; original community membership
                        // remains unchanged locally. Forker can retry leave from settings.
                        tracing::warn!(
                            error = ?e,
                            original_id = %hex::encode(original_id.0),
                            "ZEB-285: failed to publish Leave event; fork still on disk, \
                             original community membership unchanged locally"
                        );
                    }
                }
            } // end if let Some(leave_event) = maybe_leave_event
        } // end if leave_generation_ok
    }

    // Step 9: emit `community-forked` frontend event (non-fatal if it fails).
    {
        use tauri::Emitter;
        #[derive(serde::Serialize)]
        struct CommunityForkedPayload<'a> {
            original_community_id: &'a str,
            fork_space_id: &'a str,
            name: &'a str,
            visible: bool,
            snapshot_message_count: usize,
        }
        let payload = CommunityForkedPayload {
            original_community_id: &community_id,
            fork_space_id: &fork_space_id_hex,
            name: &opts.name,
            visible,
            snapshot_message_count,
        };
        if let Err(e) = app.emit("community-forked", &payload) {
            tracing::warn!(error = %e, "fork_community: community-forked emit failed");
        }
    }

    Ok(ForkCommunityResult {
        fork_space_id: fork_space_id_hex,
        visible,
        snapshot_message_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::SignedChannelEvent;
    use crate::community_membership::ChannelId;
    use crate::owner_state_types::Hlc;
    use std::collections::BTreeMap;

    /// Build a dummy SignedChannelEvent::Post with the given HLC wall_ms.
    /// All other fields are zero/empty but structurally valid.
    fn make_event(wall_ms: u64, logical: u32) -> SignedChannelEvent {
        SignedChannelEvent::Post {
            at: Hlc {
                wall_ms,
                logical,
                device_id: "test".to_string(),
            },
            author: crate::owner_state_types::OwnerAddr([0u8; 16]),
            body: String::new(),
            channel_id: ChannelId([0u8; 16]),
            community_id: crate::owner_state_types::SpaceId([0u8; 16]),
            id: crate::community_channel_log::MessageId([0u8; 16]),
            content_kind: 0,
            mentions: None,
            attachments: None,
            reply_to: None,
            sig: [0u8; 64],
        }
    }

    // ── ZEB-649: fork reason validation + mint threading ─────────────────

    #[test]
    fn validate_fork_reason_trims_and_accepts() {
        assert_eq!(
            validate_fork_reason("  Treasury split  "),
            Ok("Treasury split".to_string())
        );
        let at_cap = "a".repeat(crate::community_membership::MAX_MODERATION_REASON_CHARS);
        assert_eq!(validate_fork_reason(&at_cap), Ok(at_cap.clone()));
    }

    #[test]
    fn validate_fork_reason_rejects_empty_and_oversize() {
        assert!(validate_fork_reason("").is_err());
        assert!(
            validate_fork_reason("   \n\t ").is_err(),
            "whitespace-only is empty"
        );
        let oversized = "a".repeat(crate::community_membership::MAX_MODERATION_REASON_CHARS + 1);
        assert!(validate_fork_reason(&oversized).is_err());
    }

    #[test]
    fn mint_fork_event_carries_reason_as_some() {
        use crate::community_membership::MembershipEventKind;
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let event = mint_fork_event(
            crate::owner_state_types::SpaceId([0xc0; 16]),
            crate::owner_state_types::OwnerAddr([0xaa; 16]),
            crate::owner_state_types::SpaceId([0xfa; 16]),
            "Treasury split".to_string(),
            &signing_key,
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        )
        .expect("mint");

        match &event.kind {
            MembershipEventKind::Fork {
                fork_space_id,
                reason,
            } => {
                assert_eq!(
                    *fork_space_id,
                    crate::owner_state_types::SpaceId([0xfa; 16])
                );
                assert_eq!(reason.as_deref(), Some("Treasury split"));
            }
            other => panic!("expected Fork, got {other:?}"),
        }
    }

    /// ZEB-649 (CodeRabbit PR #434): mint_fork_event validates at its own
    /// boundary — a pub-fn caller skipping the IPC gate cannot mint an
    /// event every replica's verify_event would reject.
    #[test]
    fn mint_fork_event_rejects_empty_reason() {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let result = mint_fork_event(
            crate::owner_state_types::SpaceId([0xc0; 16]),
            crate::owner_state_types::OwnerAddr([0xaa; 16]),
            crate::owner_state_types::SpaceId([0xfa; 16]),
            "   ".to_string(),
            &signing_key,
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn build_snapshot_applies_per_channel_cap() {
        // 600 messages in one channel → trim to 500 (newest 500 kept).
        let ch = ChannelId([1u8; 16]);
        let events: Vec<SignedChannelEvent> = (0u64..600).map(|i| make_event(i, 0)).collect();

        let mut raw: BTreeMap<ChannelId, Vec<SignedChannelEvent>> = BTreeMap::new();
        raw.insert(ch, events);

        let result = build_snapshot(raw);
        let retained = result.get(&ch).expect("channel present");

        assert_eq!(
            retained.len(),
            SNAPSHOT_PER_CHANNEL_CAP,
            "per-channel cap must trim 600 → 500"
        );

        // Verify the newest 500 were kept (wall_ms 100..599, ascending).
        let first_wall = retained[0].at().wall_ms;
        let last_wall = retained[retained.len() - 1].at().wall_ms;
        assert_eq!(
            first_wall, 100,
            "oldest retained event should be wall_ms=100 (newest 500 of 600)"
        );
        assert_eq!(
            last_wall, 599,
            "newest retained event should be wall_ms=599"
        );
    }

    #[test]
    fn build_snapshot_applies_total_cap_proportionally() {
        // Two channels of 4000 each (already under per-channel cap of 500 each
        // after cap step — wait, 4000 > 500, so each gets capped to 500 first,
        // total = 1000, which is < 5000). We need channels that individually
        // pass the 500 cap but together exceed 5000.
        //
        // To actually exercise the proportional trim: need total > 5000.
        // With SNAPSHOT_PER_CHANNEL_CAP = 500 and 11 channels of 500 = 5500 > 5000.
        // Use 2 channels each holding 2500 events BUT per-channel cap trims each
        // to 500 first. So we'd need to use > SNAPSHOT_PER_CHANNEL_CAP channels.
        //
        // Actually the test description says "two channels of 4000 each → trim to
        // 2500 + 2500 total". With per-channel cap=500, each would get capped to
        // 500 → total=1000 < 5000 → no proportional trim needed. The test description
        // in the spec is using SNAPSHOT_PER_CHANNEL_CAP=2500, SNAPSHOT_TOTAL_CAP=5000
        // as hypothetical values. We need to test with actual constants.
        //
        // With the real constants (PER=500, TOTAL=5000), to trigger proportional trim
        // we need more than 10 channels each with 500 events. Use 11 channels × 500 =
        // 5500 > 5000 → proportional trim to 5000.
        //
        // But the task spec says "two channels of 4000 each". Let's interpret this
        // as: override the test to work correctly with real constants by using enough
        // channels to exceed SNAPSHOT_TOTAL_CAP.
        //
        // Test: 12 channels × 500 events each = 6000 total > 5000 → proportional trim.
        // Proportional allocation per channel: 500 × 5000 / 6000 = 416 (floor).
        // Allocated sum = 12 × 416 = 4992. Remainder = 5000 - 4992 = 8 → largest channel.
        // All channels are same size, so first max_by_key wins (channel 0 or last depending on impl).
        // Total after trim: 4992 + 8 = 5000.

        let n_channels = 12usize;
        let events_per_channel = SNAPSHOT_PER_CHANNEL_CAP; // 500 each, exactly at cap
        let mut raw: BTreeMap<ChannelId, Vec<SignedChannelEvent>> = BTreeMap::new();
        for ch_idx in 0..n_channels {
            let mut ch_bytes = [0u8; 16];
            ch_bytes[0] = ch_idx as u8;
            let ch = ChannelId(ch_bytes);
            let events: Vec<SignedChannelEvent> = (0u64..events_per_channel as u64)
                .map(|i| make_event(i, 0))
                .collect();
            raw.insert(ch, events);
        }

        let result = build_snapshot(raw);
        let total: usize = result.values().map(|v| v.len()).sum();

        assert!(
            total <= SNAPSHOT_TOTAL_CAP,
            "total {total} must not exceed SNAPSHOT_TOTAL_CAP={SNAPSHOT_TOTAL_CAP}"
        );
        // Allow for the floor-rounding slack: total ≥ SNAPSHOT_TOTAL_CAP - channel_count.
        assert!(
            total >= SNAPSHOT_TOTAL_CAP.saturating_sub(n_channels),
            "total {total} should be close to SNAPSHOT_TOTAL_CAP={SNAPSHOT_TOTAL_CAP} \
             (within {n_channels})"
        );
    }
}
