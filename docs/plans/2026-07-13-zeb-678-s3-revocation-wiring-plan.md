# ZEB-678 S3 — Revocation wiring + honesty copy — Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax for tracking. Execute inline (executing-plans) — the tasks are tightly coupled around `revoke_device_inner`.

**Goal:** Make `revoke_device` actually cut off a revoked device's migrated vine feed by publishing a revoked `FeedAuthorityRecord`, and retire the now-false "feed publishing is not blocked yet" honesty copy.

**Architecture:** Both revoke kinds re-publish the target device's *stamped active binding* (its `feed_binding`, from the replicated fleet-net row) with the signed `RevocationCert` appended and `updated_at` bumped — no re-signing, because `n_sig` covers only the immutable identity binding (§3.1), not `revocation`/`updated_at`. Self-revoke publishes before its terminal engine-halt; master-revoke reads the sibling's replicated `feed_binding`. Followers already converge on this via the S1 sticky/first-write-wins cache + S2 dual-path ingest — S3 only produces the record.

**Tech Stack:** Rust (tauri backend), `feed_authority.rs` + `owner_commands.rs`; Svelte (frontend copy).

## Global Constraints

- Design source of truth: `docs/specs/2026-07-12-zeb-678-vine-follow-revocation-design.md` §6 (revocation flow), §7 (honesty copy), §8 (honesty ledger), §10 S3.
- **No `FILE_VERSION` bump** — `feed_binding` and all authority fields are additive; signatures are never persisted.
- **No re-signing of `n_sig`** — appending a `RevocationCert` + bumping `updated_at` keeps the original binding signature valid by construction (feed_authority.rs `authority_binding_bytes` covers only `domain ‖ feed_id ‖ owner_id ‖ device_id ‖ publisher_key`).
- Feed cut-off is **best-effort and non-fatal to the revoke**: a publish failure logs a warn and never fails `revoke_device` (the trust revocation + retire-announce still land). A device that never migrated a feed has **nothing to cut** (honest residual, §8).
- **First-write-wins binding + sticky-monotonic `revoked`** (already in S1 cache) make the cut-off permanent against a compromised device's fight-back — S3 must not add any path that could clear a revocation.
- Keychain untouched (the #2 key is already resident; no new keychain access → ZEB-428 `*_inner` seam rule is N/A here).
- Gates per task: `scripts/test-select --context task` iteratively (paste the `round=…/bucket=…` summary line into the task note), then `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, targeted `cargo nextest run --locked -E 'test(...)' --features test-fixtures`. Frontend task: `npm run test -- --run` (vitest) + `npm run check` (tsc/svelte-check). Full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` sweep + fmt + clippy before the PR opens.
- All `cargo`/`scripts` commands run from `/Users/zeblith/work/zeblithic/harmony-client/src-tauri`; `npm` from `/Users/zeblith/work/zeblithic/harmony-client`. Prefix every Bash call with an absolute `cd` (cwd drifts between calls).

## File Structure

- **Modify** `src-tauri/src/feed_authority.rs` — add `build_revoked_authority` (pure record transform) + unit tests. Cohesive with the existing authority builders/verifiers and their `quorum_fixtures` test harness.
- **Modify** `src-tauri/src/owner_commands.rs` — add `feed_binding_for_device` (pure scan) + `publish_feed_revocation` (async publish glue) helpers; snapshot `publish_tx`/`fleet_net_doc` in `revoke_device_inner`; insert the main-path cut-off after the trust flush; add the retry-arm republish; carry the target in `RevocationPlan::AlreadyRevoked`; tests via the existing `two_device_fixture` harness.
- **Modify** `harmony_owner::state::OwnerState` (git dep — check first) OR add a client-side accessor: retrieve the stored `RevocationCert` for a target (retry arm). Prefer an existing accessor; add a thin read-only one only if none exists.
- **Modify** `src/lib/components/RemoveDeviceDialog.svelte` — retire the "vine feeds … not blocked yet" copy.
- **Modify** `src/lib/components/__tests__/DevicesPanel.test.ts` — assert the new honesty copy.
- **Modify** `docs/specs/2026-07-11-zeb-668-device-management-design.md` §8 — mark the "existing feed publishing … not blocked yet" honesty row retired by ZEB-678.

---

### Task 1: `build_revoked_authority` — active binding → revoked record (feed_authority.rs)

**Files:**
- Modify: `src-tauri/src/feed_authority.rs` (new `pub fn` near the other builders, ~after `build_active_authority` at line 118; tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `FeedAuthorityRecord` (fields incl. `pub revocation_cbor_hex: Option<String>`, `pub updated_at: u64`, `pub feed_id: String`, `pub device_id: String`), `encode_revocation`, `verify_authority` (all already `pub`/`pub(crate)`), `harmony_owner::certs::RevocationCert` (`.target: [u8;16]`).
- Produces: `pub fn build_revoked_authority(active_binding_json: &str, revocation: &RevocationCert, now_ms: u64) -> Result<(String, String), String>` returning `(feed_id, canonical_json)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` (uses the existing `gen_identity`, `record_for`, `mint_quorum_world`, `WORLD_NOW` helpers already in the module, plus `RevocationReason` — import it):

```rust
#[test]
fn build_revoked_authority_appends_cert_and_still_verifies_as_revoked() {
    use harmony_owner::certs::RevocationReason;
    let world = mint_quorum_world(0xA0);
    let n = gen_identity();
    // The stamped active feed_binding: master-issued (empty signer bundle), no revocation.
    let active = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
    let active_json = serde_json::to_string(&active).unwrap();
    let expected_feed = active.feed_id.clone();

    let rev = RevocationCert::sign_master(
        &world.master_sk,
        world.master_bundle.clone(),
        world.a_cert.device_id,
        WORLD_NOW,
        RevocationReason::Lost,
    )
    .unwrap();
    let now_ms = (WORLD_NOW + 5) * 1000;

    let (feed_id, json) = build_revoked_authority(&active_json, &rev, now_ms).unwrap();
    assert_eq!(feed_id, expected_feed);

    let parsed: FeedAuthorityRecord = serde_json::from_str(&json).unwrap();
    assert!(parsed.revocation_cbor_hex.is_some(), "revocation appended");
    assert!(parsed.updated_at >= now_ms, "updated_at bumped forward");
    // n_sig untouched → the binding still verifies, now flagged revoked.
    let v = verify_authority(&parsed, WORLD_NOW).expect("revoked authority verifies");
    assert!(v.revoked, "device must be marked revoked");
    assert_eq!(v.device_id, world.a_cert.device_id);
}

#[test]
fn build_revoked_authority_rejects_target_that_is_not_the_feed_device() {
    use harmony_owner::certs::RevocationReason;
    let world = mint_quorum_world(0xA1);
    let n = gen_identity();
    let active = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
    let active_json = serde_json::to_string(&active).unwrap();
    // Revocation targets a DIFFERENT device (C), not the feed's device (A).
    let rev = RevocationCert::sign_master(
        &world.master_sk,
        world.master_bundle.clone(),
        world.c_quorum_cert.device_id,
        WORLD_NOW,
        RevocationReason::Lost,
    )
    .unwrap();
    let err = build_revoked_authority(&active_json, &rev, WORLD_NOW * 1000).unwrap_err();
    assert!(err.contains("target"), "target mismatch is rejected: {err}");
}

#[test]
fn build_revoked_authority_rejects_unparseable_binding() {
    use harmony_owner::certs::RevocationReason;
    let world = mint_quorum_world(0xA2);
    let rev = RevocationCert::sign_master(
        &world.master_sk,
        world.master_bundle.clone(),
        world.a_cert.device_id,
        WORLD_NOW,
        RevocationReason::Lost,
    )
    .unwrap();
    assert!(build_revoked_authority("not json", &rev, 1_000).is_err());
}
```

- [ ] **Step 2: Run tests to confirm they fail (function undefined)**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(build_revoked_authority)' --features test-fixtures`
Expected: compile error / FAIL — `build_revoked_authority` not found.

> **Note:** `RevocationCert::sign_master`'s exact arg types (esp. whether `RevocationReason` is passed by value or `&`) mirror the `mint_quorum_revocation` fixture in `enrollment_verify.rs` (~line 296–307). If the compiler rejects the signature above, match that fixture's call exactly.

- [ ] **Step 3: Implement `build_revoked_authority`**

```rust
/// ZEB-678 S3: turn a device's stamped *active* `FeedAuthorityRecord` (its
/// fleet-net `feed_binding`) into a *revoked* one by appending `revocation` and
/// bumping the LWW clock. No re-signing: `n_sig` covers only the immutable
/// binding (§3.1), so the original signature stays valid and every follower still
/// accepts the record — now flagged revoked. Returns `(feed_id, canonical_json)`
/// ready to publish to `harmony/vines/{feed_id}/authority`.
///
/// Rejects a `revocation` whose `target` is not the feed's `device_id`: such a
/// record is dropped by every follower at `verify_authority` step 3, so we never
/// emit it.
pub fn build_revoked_authority(
    active_binding_json: &str,
    revocation: &RevocationCert,
    now_ms: u64,
) -> Result<(String, String), String> {
    let mut rec: FeedAuthorityRecord = serde_json::from_str(active_binding_json)
        .map_err(|e| format!("feed_binding parse failed: {e}"))?;
    let target_hex = hex::encode(revocation.target);
    if target_hex != rec.device_id {
        return Err(format!(
            "revocation target {target_hex} does not match feed device_id {}",
            rec.device_id
        ));
    }
    rec.revocation_cbor_hex = Some(encode_revocation(revocation)?);
    rec.updated_at = now_ms.max(rec.updated_at.saturating_add(1));
    let json = serde_json::to_string(&rec).map_err(|e| format!("serialize failed: {e}"))?;
    Ok((rec.feed_id.clone(), json))
}
```

- [ ] **Step 4: Run tests to confirm they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(build_revoked_authority)' --features test-fixtures`
Expected: 3 passed.

- [ ] **Step 5: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/feed_authority.rs && git commit -m "feat(zeb-678-s3): build_revoked_authority — append RevocationCert to a stamped binding"
```

---

### Task 2: fleet-net scan + publish helpers (owner_commands.rs)

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (module-level helpers near the top, after the `use` block; unit test for the scan in `mod tests`)

**Interfaces:**
- Consumes: `crate::fleet_net::FleetNetDoc` (`.devices: BTreeMap<String, FleetNetRow>`), `FleetNetRow.feed_binding: Option<String>`, `crate::feed_authority::{FeedAuthorityRecord, build_revoked_authority}`, `crate::event_loop::PublishRequest { key_expr: String, payload: Vec<u8>, reply: oneshot::Sender<Result<(),String>> }`, `harmony_owner::certs::RevocationCert`.
- Produces:
  - `fn feed_binding_for_device(doc: &crate::fleet_net::FleetNetDoc, target_device_id_hex: &str) -> Option<String>`
  - `async fn publish_feed_revocation(publish_tx: &tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>, fleet_net_doc: &std::sync::Arc<tokio::sync::Mutex<crate::fleet_net::FleetNetDoc>>, revocation: &RevocationCert, now_ms: u64) -> Result<bool, String>` (Ok(true)=published, Ok(false)=nothing to cut)

- [ ] **Step 1: Write the failing test (scan helper)**

The pure scan is unit-testable without channels. Build a `FleetNetDoc` with rows; one row carries a `feed_binding` whose `device_id` matches the target. Add to `owner_commands.rs` `mod tests`:

```rust
#[test]
fn feed_binding_for_device_finds_matching_row_and_skips_others() {
    use crate::fleet_net::{FleetNetDoc, FleetNetRow, Hlc};
    // A parseable authority record whose device_id we control.
    let make_binding = |device_id_hex: &str| {
        serde_json::json!({
            "feedId": "feed-abcd",
            "ownerId": "00".repeat(16),
            "deviceId": device_id_hex,
            "publisherKey": "11".repeat(32),
            "nIdentityPub": "22".repeat(64),
            "enrollmentCborHex": "aa",
            "updatedAt": 1u64,
            "nSig": "33".repeat(64),
        })
        .to_string()
    };
    let row = |fb: Option<String>| FleetNetRow {
        iroh_endpoint_id: [0u8; 32],
        home_relay: String::new(),
        seen_at: Hlc::default(),
        feed_binding: fb,
    };
    let mut doc = FleetNetDoc::default();
    doc.devices.insert("sp1-a".into(), row(None)); // no feed_binding
    doc.devices
        .insert("sp1-b".into(), row(Some(make_binding("ab".repeat(8).as_str()))));
    doc.devices
        .insert("sp1-c".into(), row(Some("garbage".into()))); // unparseable → skipped

    let found = feed_binding_for_device(&doc, &"ab".repeat(8));
    assert!(found.is_some(), "matching device_id row found");
    assert!(feed_binding_for_device(&doc, &"cd".repeat(8)).is_none(), "no match → None");
}
```

> Confirm `Hlc: Default` and the `FleetNetRow` field set/visibility against fleet_net.rs:37–60 before running (the survey lists exactly these four fields; `Hlc` derives `Default` per its use as a zero clock — if not, construct it via its public constructor).

- [ ] **Step 2: Run to confirm failure**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(feed_binding_for_device)' --features test-fixtures`
Expected: compile error — `feed_binding_for_device` undefined.

- [ ] **Step 3: Implement both helpers**

```rust
/// ZEB-678 S3: find the stamped `feed_binding` for the device whose harmony-owner
/// `device_id` (16-byte, hex) equals `target_device_id_hex`, by scanning fleet-net
/// rows and parsing each row's authority record. `None` ⇒ that device never
/// migrated a feed (honest residual §8 — nothing to cut). Keyed on the *parsed*
/// `device_id`, not the row's SP1 key, so no owner↔SP1 id mapping is needed.
fn feed_binding_for_device(
    doc: &crate::fleet_net::FleetNetDoc,
    target_device_id_hex: &str,
) -> Option<String> {
    doc.devices.values().find_map(|row| {
        let fb = row.feed_binding.as_ref()?;
        let rec: crate::feed_authority::FeedAuthorityRecord = serde_json::from_str(fb).ok()?;
        (rec.device_id == target_device_id_hex).then(|| fb.clone())
    })
}

/// ZEB-678 S3: publish a revoked device's feed cut-off. Reads its stamped active
/// binding from the replicated fleet-net doc, appends `revocation`, and republishes
/// to `harmony/vines/{N}/authority`. Idempotent (sticky-revoked on the follower).
/// `Ok(true)` ⇒ published; `Ok(false)` ⇒ no migrated feed. Never fatal to revoke.
async fn publish_feed_revocation(
    publish_tx: &tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
    fleet_net_doc: &std::sync::Arc<tokio::sync::Mutex<crate::fleet_net::FleetNetDoc>>,
    revocation: &RevocationCert,
    now_ms: u64,
) -> Result<bool, String> {
    let target_hex = hex::encode(revocation.target);
    let feed_binding = {
        let doc = fleet_net_doc.lock().await;
        feed_binding_for_device(&doc, &target_hex)
    };
    let Some(fb) = feed_binding else {
        return Ok(false);
    };
    let (feed_id, rec_json) =
        crate::feed_authority::build_revoked_authority(&fb, revocation, now_ms)?;
    let key_expr = format!("harmony/vines/{feed_id}/authority");
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(crate::event_loop::PublishRequest {
            key_expr,
            payload: rec_json.into_bytes(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| "vine authority: event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "vine authority: event loop dropped publish".to_string())?
        .map_err(|e| format!("vine authority: publish rejected: {e}"))?;
    Ok(true)
}
```

`RevocationCert` is already imported (owner_commands.rs:14). Confirm `crate::event_loop::PublishRequest` field names match the survey (`key_expr`, `payload`, `reply`).

- [ ] **Step 4: Run to confirm pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(feed_binding_for_device)' --features test-fixtures`
Expected: 1 passed. (`publish_feed_revocation` is exercised by Task 3's integration tests; if clippy flags it dead-code before Task 3 lands, allow it in the same commit — it is wired in Task 3.)

- [ ] **Step 5: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/owner_commands.rs && git commit -m "feat(zeb-678-s3): fleet-net feed_binding scan + feed-revocation publish helpers"
```

---

### Task 3: wire the main-path feed cut-off into `revoke_device_inner`

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (`revoke_device_inner` guard snapshot ~791–815; cert clone before the `mutate_trust_state` closure ~896; insertion after the trust flush ~919; `now_ms` at fn top; tests in `mod tests`)

**Interfaces:**
- Consumes: the guard fields `g.publish_tx` (`Option<tokio::sync::mpsc::Sender<event_loop::PublishRequest>>`), `g.fleet_net_doc` (`Option<Arc<tokio::sync::Mutex<FleetNetDoc>>>`), `planned.cert`, `publish_feed_revocation` (Task 2).
- Produces: after a successful trust flush, both self- and master-revoke publish the feed cut-off (before the self terminal at ~1002 and before the master epoch-bump block at ~927).

- [ ] **Step 1: Write the failing tests**

Model the fixture on `revoke_device_inner_master_path_bumps_fleet_epoch` (owner_commands.rs:2277–2395) and `two_device_fixture` (1992–2029). Wire a `publish_tx`/`rx` pair and a `fleet_net_doc` holding a stamped `feed_binding` for the revoke target, drain the rx, and assert a revoked authority was published to the target feed's topic. Add:

```rust
/// Build a stamped active feed_binding JSON for `device`'s feed and insert it as a
/// fleet-net row so the revoke path can find + cut it. Returns the feed_id.
fn stamp_test_feed_binding(
    doc: &mut crate::fleet_net::FleetNetDoc,
    sp1_key: &str,
    device_cert: &harmony_owner::certs::EnrollmentCert,
    device_sk: &ed25519_dalek::SigningKey,
) -> String {
    let n = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
    // now_ms arg is the migration clock; value irrelevant to the test assertions.
    let rec = crate::feed_authority::build_active_authority(&n, device_sk, device_cert, 1_000)
        .expect("build active authority");
    let feed_id = rec.feed_id.clone();
    let json = serde_json::to_string(&rec).unwrap();
    doc.devices.insert(
        sp1_key.to_string(),
        crate::fleet_net::FleetNetRow {
            iroh_endpoint_id: [0u8; 32],
            home_relay: String::new(),
            seen_at: crate::fleet_net::Hlc::default(),
            feed_binding: Some(json),
        },
    );
    feed_id
}

#[tokio::test]
async fn revoke_device_inner_master_revoke_publishes_feed_cutoff() {
    std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
    let now = 1_700_000_000;
    let dir = tempfile::tempdir().unwrap();
    let (state, a_sk, seed, b_sk, b_vk_hex) = two_device_fixture(now);
    // B's enrollment cert (the sibling being revoked) — pull from the fixture state.
    let b_cert = state
        .enrollment_for_vk_hex(&b_vk_hex) // helper on OwnerState; if absent, look it up from the fixture
        .expect("b cert");
    save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None).unwrap();

    // fleet-net doc carrying B's stamped feed_binding.
    let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
    let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
    let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

    // publish channel + drain task.
    let (publish_tx, mut publish_rx) =
        tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
    let published: std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>> = Default::default();
    let published_c = published.clone();
    let drain = tokio::spawn(async move {
        while let Some(req) = publish_rx.recv().await {
            published_c.lock().unwrap().push((req.key_expr.clone(), req.payload.clone()));
            let _ = req.reply.send(Ok(()));
        }
    });

    let node = std::sync::Mutex::new(crate::NodeState {
        identity_dir: Some(dir.path().to_path_buf()),
        publish_tx: Some(publish_tx),
        fleet_net_doc: Some(fleet_net_doc),
        ..crate::NodeState::default()
    });

    revoke_device_inner(&node, || None, std::sync::Arc::new(|_| {}), b_vk_hex, "lost".into())
        .await
        .unwrap();

    drop(node); // close publish_tx so the drain task ends
    drain.await.unwrap();

    let pubs = published.lock().unwrap();
    let want_key = format!("harmony/vines/{feed_id}/authority");
    let hit = pubs.iter().find(|(k, _)| *k == want_key).expect("feed cut-off published");
    let rec: crate::feed_authority::FeedAuthorityRecord =
        serde_json::from_slice(&hit.1).unwrap();
    let v = crate::feed_authority::verify_authority(&rec, now).expect("cut-off verifies");
    assert!(v.revoked, "published authority marks B revoked");
}

#[tokio::test]
async fn revoke_device_inner_no_feed_binding_publishes_nothing() {
    std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
    let now = 1_700_000_000;
    let dir = tempfile::tempdir().unwrap();
    let (state, a_sk, seed, _b_sk, b_vk_hex) = two_device_fixture(now);
    save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None).unwrap();

    // Empty fleet-net doc → B never migrated → nothing to cut.
    let fleet_net_doc =
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::fleet_net::FleetNetDoc::default()));
    let (publish_tx, mut publish_rx) =
        tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_c = count.clone();
    let drain = tokio::spawn(async move {
        while let Some(req) = publish_rx.recv().await {
            count_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = req.reply.send(Ok(()));
        }
    });
    let node = std::sync::Mutex::new(crate::NodeState {
        identity_dir: Some(dir.path().to_path_buf()),
        publish_tx: Some(publish_tx),
        fleet_net_doc: Some(fleet_net_doc),
        ..crate::NodeState::default()
    });
    revoke_device_inner(&node, || None, std::sync::Arc::new(|_| {}), b_vk_hex, "lost".into())
        .await
        .unwrap();
    drop(node);
    drain.await.unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0, "no publish when no feed");
}

#[tokio::test]
async fn revoke_device_inner_self_revoke_publishes_cutoff_before_terminal() {
    std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
    let now = 1_700_000_000;
    let dir = tempfile::tempdir().unwrap();
    let (state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(now);
    let b_cert = state.enrollment_for_vk_hex(&b_vk_hex).expect("b cert");
    // Persist as B, cert-only (no seed) → self-revoke path.
    save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).unwrap();

    let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
    let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
    let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

    let (publish_tx, mut publish_rx) =
        tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
    // Record interleaving of publishes vs the "device-revoked-self" emit.
    let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let log_pub = log.clone();
    let drain = tokio::spawn(async move {
        while let Some(req) = publish_rx.recv().await {
            log_pub.lock().unwrap().push(format!("publish:{}", req.key_expr));
            let _ = req.reply.send(Ok(()));
        }
    });
    let log_emit = log.clone();
    let emit = std::sync::Arc::new(move |name: &str| {
        log_emit.lock().unwrap().push(format!("emit:{name}"));
    });
    let node = std::sync::Mutex::new(crate::NodeState {
        identity_dir: Some(dir.path().to_path_buf()),
        publish_tx: Some(publish_tx),
        fleet_net_doc: Some(fleet_net_doc),
        ..crate::NodeState::default()
    });
    revoke_device_inner(&node, || None, emit, b_vk_hex, "decommissioned".into())
        .await
        .unwrap();
    drop(node);
    drain.await.unwrap();

    let events = log.lock().unwrap();
    let pub_idx = events
        .iter()
        .position(|e| e == &format!("publish:harmony/vines/{feed_id}/authority"))
        .expect("feed cut-off published");
    let term_idx = events
        .iter()
        .position(|e| e == "emit:device-revoked-self")
        .expect("terminal emitted");
    assert!(pub_idx < term_idx, "feed cut-off must publish before terminal halt");
}
```

> **Fixture helper note:** `state.enrollment_for_vk_hex(&b_vk_hex)` is illustrative — use whatever accessor `two_device_fixture`/`OwnerState` already exposes to obtain B's `EnrollmentCert` (the fixture enrolls B, so its cert is reachable; if no accessor exists, have `two_device_fixture` also return `b_cert`). Confirm `NodeState` has public `publish_tx` / `fleet_net_doc` fields for struct-literal construction (they are `pub(crate)` on the same-crate `NodeState`); if a field is private, extend the existing test-fixture constructor pattern used by the epoch-bump test.

- [ ] **Step 2: Run to confirm failure**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(revoke_device_inner_master_revoke_publishes) + test(revoke_device_inner_no_feed_binding) + test(revoke_device_inner_self_revoke_publishes)' --features test-fixtures`
Expected: FAIL — no publish observed (cut-off not wired yet).

- [ ] **Step 3: Wire the cut-off**

1. Add `publish_tx` + `fleet_net_doc` to the guard snapshot tuple (revoke_device_inner ~791–815):

```rust
    let (trust_doc, trust_engine, identity_dir, revoked_flag, owner_sync, fleet_net, retire_nudge, publish_tx, fleet_net_doc) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.owner_trust_doc.clone(),
            g.owner_trust_sync.clone(),
            g.identity_dir.clone(),
            std::sync::Arc::clone(&g.owner_trust_revoked_self),
            g.sync_engine.clone(),
            g.fleet_net_sync.clone(),
            g.community_device_retire_nudge.clone(),
            g.publish_tx.clone(),
            g.fleet_net_doc.clone(),
        )
    };
```

2. Compute `now_ms` once near the existing `now` (seconds) computation:

```rust
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
```

3. Clone the cert before it is moved into the `mutate_trust_state` closure (~896):

```rust
    let cert_for_feed = cert.clone(); // ZEB-678 S3: cert is moved into add_revocation below
```

4. After the trust-flush block (immediately after the `if let Some(engine) = &trust_engine { … }` ending ~line 919, before the `if !is_self { … }` epoch-bump block ~927):

```rust
    // ZEB-678 S3: cut off the revoked device's migrated vine feed. Republishes its
    // stamped authority binding with the RevocationCert appended — self-revoke (own
    // feed, before terminal) and master-revoke (sibling's replicated feed_binding)
    // share this one path. Best-effort + non-fatal: a device that never migrated has
    // nothing to cut; a publish failure never fails the revoke.
    if let (Some(publish_tx), Some(fleet_net_doc)) = (&publish_tx, &fleet_net_doc) {
        match publish_feed_revocation(publish_tx, fleet_net_doc, &cert_for_feed, now_ms).await {
            Ok(true) => {
                tracing::info!(target = %device_vk_hex, "revoke_device: published vine feed cut-off")
            }
            Ok(false) => {
                tracing::debug!("revoke_device: revoked device has no migrated vine feed to cut")
            }
            Err(e) => {
                tracing::warn!(error = %e, "revoke_device: vine feed cut-off publish failed (non-fatal)")
            }
        }
    }
```

- [ ] **Step 4: Run to confirm pass + no regressions in the existing revoke tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(revoke_device_inner)' --features test-fixtures`
Expected: all `revoke_device_inner_*` tests pass (3 new + 5 existing; the FileOnly tests have no `publish_tx`/`fleet_net_doc` so the cut-off block is skipped and their assertions are unchanged).

- [ ] **Step 5: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/owner_commands.rs && git commit -m "feat(zeb-678-s3): publish vine feed cut-off on self- and master-revoke"
```

---

### Task 4: retry-arm republish (self-revoke died before terminal)

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (`RevocationPlan::AlreadyRevoked` enum ~100–107 + `plan_revocation` ~112 to carry `target`; the retry arm ~851–874; a stored-cert accessor; test in `mod tests`)

**Rationale:** The main-path cut-off (Task 3) runs after the trust flush and before the self terminal. If a prior self-revoke run added the revocation but the process died before publishing the cut-off (or before latching terminal), the retry arm (`AlreadyRevoked{is_self:true}`) must re-publish it — otherwise vine followers never learn the feed is revoked (they consult the authority cache, not the owner trust doc). Idempotent: republishing a revoked authority is a no-op on the follower (sticky-revoked).

**Interfaces:**
- Consumes: `trust_doc` (`Option<Arc<tokio::sync::Mutex<OwnerState>>>` — already snapshotted), `publish_tx`, `fleet_net_doc` (Task 3), a way to read the stored `RevocationCert` for the target.
- Produces: `RevocationPlan::AlreadyRevoked { is_self: bool, target: [u8;16] }`; the retry arm republishes the self feed cut-off.

- [ ] **Step 1: Write the failing test**

Pre-seed a stranded self-revocation (revocation already in the persisted doc, terminal flag unlatched — mirror `revoke_device_inner_retry_completes_pending_self_terminal` at 2507–2571), wire a **resident** `trust_doc` holding B's revocation + `publish_tx` + `fleet_net_doc` with B's stamped `feed_binding`, and assert the retry republishes the cut-off:

```rust
#[tokio::test]
async fn revoke_device_inner_retry_republishes_self_feed_cutoff() {
    std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
    let now = 1_700_000_000;
    let dir = tempfile::tempdir().unwrap();
    let (mut state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(now);
    let b_cert = state.enrollment_for_vk_hex(&b_vk_hex).expect("b cert");
    let b_target = crate::owner_state::device_id_from_signing_key(&b_sk);

    // Strand B's self-revocation into the doc (added, not yet terminal).
    let cert = harmony_owner::certs::RevocationCert::sign_self(
        &b_sk, state.owner_id, b_target, now, harmony_owner::certs::RevocationReason::Decommissioned,
    )
    .unwrap();
    state.add_revocation(cert, now, crate::trust::DEFAULT_ACTIVE_WINDOW_SECS).unwrap();
    save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).unwrap();

    let trust_doc = std::sync::Arc::new(tokio::sync::Mutex::new(state));

    let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
    let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
    let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

    let (publish_tx, mut publish_rx) =
        tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
    let published: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let pc = published.clone();
    let drain = tokio::spawn(async move {
        while let Some(req) = publish_rx.recv().await {
            pc.lock().unwrap().push(req.key_expr.clone());
            let _ = req.reply.send(Ok(()));
        }
    });
    let node = std::sync::Mutex::new(crate::NodeState {
        identity_dir: Some(dir.path().to_path_buf()),
        owner_trust_doc: Some(trust_doc),
        publish_tx: Some(publish_tx),
        fleet_net_doc: Some(fleet_net_doc),
        ..crate::NodeState::default()
    });
    revoke_device_inner(&node, || None, std::sync::Arc::new(|_| {}), b_vk_hex, "decommissioned".into())
        .await
        .unwrap();
    drop(node);
    drain.await.unwrap();

    assert!(
        published.lock().unwrap().iter().any(|k| k == &format!("harmony/vines/{feed_id}/authority")),
        "retry arm republishes the self feed cut-off"
    );
}
```

> Confirm `crate::owner_state::device_id_from_signing_key` (survey §1b), `OwnerState::add_revocation`, and `OwnerState::owner_id` visibility; adjust to the fixture's actual API where they differ.

- [ ] **Step 2: Run to confirm failure**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(revoke_device_inner_retry_republishes)' --features test-fixtures`
Expected: FAIL — retry arm returns without publishing.

- [ ] **Step 3: Implement**

1. Carry the target in the enum + planner:

```rust
pub(crate) enum RevocationPlan {
    AlreadyRevoked { is_self: bool, target: [u8; 16] },
    Planned(Box<PlannedRevocation>),
}
```
Populate `target` wherever `plan_revocation` currently builds `AlreadyRevoked { is_self }` (the target is already computed there from `device_vk_hex`).

2. Stored-cert retrieval. Check `harmony_owner::state::OwnerState` for a read accessor returning the stored `RevocationCert` for a target (the doc retains revocation certs for ZEB-668 S3 retire-announce deposits, so one very likely exists — e.g. `revocation_cert(target)` / `revocation_for(target)`). If present, use it. If absent, add a thin read-only accessor on `OwnerState` in the harmony-owner crate (git dep) returning `Option<&RevocationCert>`; keep it read-only (no mutation, no new invariant).

3. In the retry arm (`RevocationPlan::AlreadyRevoked { is_self: true, target }`, ~851–874), after the existing trust `flush_now`, before `complete_self_revoke_terminal`:

```rust
    // ZEB-678 S3: a self-revoke that added the revocation but died before publishing
    // its feed cut-off must re-publish it on retry (followers key on the authority
    // cache, not the trust doc). Idempotent — sticky-revoked on the follower.
    if let (Some(trust_doc), Some(publish_tx), Some(fleet_net_doc)) =
        (&trust_doc, &publish_tx, &fleet_net_doc)
    {
        let stored = { trust_doc.lock().await.revocation_cert(target).cloned() };
        if let Some(rev) = stored {
            if let Err(e) = publish_feed_revocation(publish_tx, fleet_net_doc, &rev, now_ms).await {
                tracing::warn!(error = %e, "revoke_device retry: feed cut-off republish failed (non-fatal)");
            }
        }
    }
```

(`now_ms` is the value added in Task 3; ensure it is computed before the retry arm — move its computation above the plan match if needed.)

- [ ] **Step 4: Run to confirm pass + no regressions**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -E 'test(revoke_device_inner)' --features test-fixtures`
Expected: all pass, including the existing `revoke_device_inner_retry_completes_pending_self_terminal` (FileOnly → no publish_tx/trust_doc, retry republish skipped, assertions unchanged).

- [ ] **Step 5: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add -A && git commit -m "feat(zeb-678-s3): retry-arm republishes self feed cut-off (died-before-terminal durability)"
```

> If Step 3.2 required an accessor in the harmony-owner crate, that repo's change lands in the same commit (cross-repo is fine per standing rules); note the crate + rev in the commit body.

---

### Task 5: retire the honesty copy + spec ledger

**Files:**
- Modify: `src/lib/components/RemoveDeviceDialog.svelte:97–100`
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`
- Modify: `docs/specs/2026-07-11-zeb-668-device-management-design.md` §8 honesty ledger

- [ ] **Step 1: Read the current copy + its test**

Read `RemoveDeviceDialog.svelte:88–116` and `DevicesPanel.test.ts` to find any existing assertion on the "not blocked yet" string.

- [ ] **Step 2: Update the honesty copy**

Replace the `<p class="dialog-message honesty">` block (lines 97–100) with copy that states the post-ZEB-678 reality (§7/§8): the feed half now blocks for a migrated feed; the DM half still doesn't (ZEB-580); reactions are best-effort; a never-posted feed has nothing to cut. Keep it concise and honest, e.g.:

```svelte
  <p class="dialog-message honesty">
    Once this removal syncs, followers stop accepting new posts to this device's vine
    feeds — except a feed it never posted to, which has nothing to cut, and reactions,
    which are best-effort. Its direct messages are a separate surface and aren't blocked
    yet — that cutoff lands in follow-up work.
  </p>
```

(Match the surrounding component's tone/voice; the exact wording is the implementer's call as long as it is accurate to §8 and no longer claims vine feeds are unblocked.)

- [ ] **Step 3: Update the frontend test**

If `DevicesPanel.test.ts` asserts the old string, update it to the new copy (assert on a stable fragment, e.g. `"stop accepting new posts"` and that it no longer claims feeds are "not blocked yet"). If there is no such assertion, add a focused one that the dialog renders the feed-cutoff copy.

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run test -- --run src/lib/components/__tests__/DevicesPanel.test.ts && npm run check`
Expected: vitest green, svelte-check/tsc clean.

- [ ] **Step 4: Retire the ZEB-668 §8 honesty-ledger row**

In `docs/specs/2026-07-11-zeb-668-device-management-design.md` §8, update the row stating existing feed publishing "is not blocked yet" to note it is **retired by ZEB-678** (migrated feeds now reject a revoked device's post-revocation records; DM half remains per ZEB-580). Do not delete the row — mark it resolved so the ledger stays an honest audit trail.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src/lib/components/RemoveDeviceDialog.svelte src/lib/components/__tests__/DevicesPanel.test.ts docs/specs/2026-07-11-zeb-668-device-management-design.md && git commit -m "feat(zeb-678-s3): retire 'feed publishing not blocked yet' honesty copy"
```

---

## Pre-PR full sweep

- [ ] Full gate from `src-tauri`:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
- [ ] Frontend: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run test -- --run && npm run check`
- [ ] Open PR to `zeblithic/harmony-client` (base `main`), fire `@coderabbitai review` **once** at open, converge Qodo + CodeRabbit per the standing loop. **Never auto-merge.**

## Self-Review (against spec §6/§7/§8/§10 S3)

- **§6 self-revoke ordering** — Task 3 self test asserts publish precedes `device-revoked-self`; Task 4 covers the died-before-terminal retry. ✓
- **§6 master-revoke via fleet-net feed_binding** — Task 2 scan + Task 3 master test read the sibling's replicated `feed_binding`, append the master cert, republish. ✓
- **§6 no re-sign** — Task 1 keeps `n_sig` untouched; `verify_authority` still passes on the revoked record. ✓
- **§4 step 4 fight-back resistance** — unchanged S1 cache (first-write-wins + sticky) enforces it; S3 adds no clearing path (Global Constraints). The compromised-device re-bind/un-revoke cases are already covered by S1 cache tests; S3 relies on them rather than duplicating. ✓
- **§8 honest residual** — Task 3 `no_feed_binding` test + `Ok(false)` path; Task 5 copy states it. ✓
- **§7 copy** — Task 5. ✓
- **Type consistency** — `publish_feed_revocation`/`feed_binding_for_device`/`build_revoked_authority` signatures match across tasks; `RevocationPlan::AlreadyRevoked` gains `target` uniformly. ✓
