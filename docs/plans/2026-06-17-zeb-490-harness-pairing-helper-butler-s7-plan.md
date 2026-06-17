# ZEB-490 — harness SAS pairing helper + co-located butler `s7` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the e2e-harness its first SAS device-pairing capability (`pair_into_fleet`) and use it in a new `s7_butler_deposit_recover` scenario that tests the butler deposit→recover durability path co-located.

**Architecture:** All work is in the standalone `e2e-harness` crate (no `src-tauri/src` production change — the three pairing/butler RPCs already exist in the curated `serve` surface). Unit 1 = thin `NodeHandle::rpc` wrappers + a `pair_into_fleet` orchestrator + a pure `assert_sas_match` helper, in `e2e-harness/src/driver.rs`. Unit 2 = the `s7` scenario in `e2e-harness/tests/e2e_two_node.rs`, mirroring `s6_relay_deposit_recover` with layered characterize fallbacks at three racy boundaries (pairing, deposit, recovery).

**Tech Stack:** Rust, `tokio`, `serde_json`, `anyhow`; the `e2e-harness` driver pattern (`node.rpc(cmd, json!({…}))` with camelCase keys + the crate's `poll_until` convergence primitive); `--features e2e` scenario tests that spawn the real `harmony-app` binary.

**Spec:** `docs/specs/2026-06-17-zeb-490-harness-pairing-helper-butler-s7-design.md`.

### Gate commands (run from the indicated dir)

- Driver hygiene: `cd e2e-harness && cargo fmt -- --check` and `cargo clippy --all-targets --features e2e -- -D warnings`
- Unit test (no live node): `cd e2e-harness && cargo nextest run -E 'test(assert_sas_match)'`
- Scenario compile-check (no run): `cd e2e-harness && cargo nextest list --features e2e -E 'test(s7_butler_deposit_recover)'`
- Scenario run: `cd src-tauri && cargo build --bin harmony-app` then `cd e2e-harness && cargo nextest run --features e2e --release -E 'test(s7_butler_deposit_recover)' --test-threads 1`

> **As-built note (supersedes spec §8):** the new public API lives in the `e2e-harness` crate, not `src-tauri`, so the relevant gates are the `e2e-harness` crate's own fmt/clippy/nextest (above) — **not** the `-p harmony-app --lib` gates. The harness is not in CI, so the PR's CI run does not compile this code; the local `e2e-harness` clippy/list gate is the only gate. Call this out in the PR body.

---

### Task 1: `assert_sas_match` pure helper (TDD)

**Files:**
- Modify: `e2e-harness/src/driver.rs` (add a pure fn + a `#[cfg(test)]` unit test)

- [ ] **Step 1: Write the failing unit test**

Append to `e2e-harness/src/driver.rs` (the file has no `mod tests` yet — add one at the end):

```rust
#[cfg(test)]
mod tests {
    use super::assert_sas_match;

    #[test]
    fn assert_sas_match_ok_when_equal() {
        assert!(assert_sas_match("012845", "012845").is_ok());
    }

    #[test]
    fn assert_sas_match_err_when_differ() {
        let e = assert_sas_match("012845", "999999").unwrap_err();
        assert!(
            e.to_string().contains("SAS mismatch"),
            "error should name the mismatch, got: {e}"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd e2e-harness && cargo nextest run -E 'test(assert_sas_match)'`
Expected: FAIL — `cannot find function assert_sas_match` (compile error).

- [ ] **Step 3: Implement the minimal function**

Add near the top of `e2e-harness/src/driver.rs` (after the `as_str` helper, ~line 33):

```rust
/// ZEB-490: assert two pairing nodes derived the SAME 6-digit SAS — the real
/// SAS security property. A mismatch is a genuine bug (NOT a characterize case),
/// so this returns a hard `Err` the scenario surfaces rather than swallowing.
pub fn assert_sas_match(inviter_sas: &str, joiner_sas: &str) -> anyhow::Result<()> {
    if inviter_sas != joiner_sas {
        anyhow::bail!("SAS mismatch: inviter={inviter_sas} joiner={joiner_sas}");
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd e2e-harness && cargo nextest run -E 'test(assert_sas_match)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add e2e-harness/src/driver.rs
git commit -m "test(zeb-490): assert_sas_match pure SAS-equality helper + unit tests

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: thin pairing + butler RPC wrappers

**Files:**
- Modify: `e2e-harness/src/driver.rs` (add 6 pairing wrappers + 3 butler wrappers)

The arg-struct camelCase keys (verified in `src-tauri/src/api/rpc.rs`): `start_*_pairing` → `displayName`; `select_pairing_peer` → `peerSessionId`; `set_butler_pin` → `deviceId`. `confirm_pairing_sas` / `get_pairing_state` / `cancel_pairing` / `get_butler_pin` / `get_butler_held` take no args.

- [ ] **Step 1: Add the wrappers**

Append to `e2e-harness/src/driver.rs` (after `get_relay_held`, before `create_channel`, ~line 318). These follow the exact existing wrapper pattern:

```rust
// ── Pairing (ZEB-446) + butler rung (ZEB-489) — ZEB-490 ──────────────────────

/// ZEB-490: inviter side — load owner_state + master_seed, enter Discovering.
pub async fn start_inviter_pairing(node: &NodeHandle, display_name: &str) -> anyhow::Result<()> {
    node.rpc("start_inviter_pairing", json!({ "displayName": display_name }))
        .await
        .map(|_| ())
}

/// ZEB-490: joiner side — generate a fresh ed25519 signing key, enter Discovering.
pub async fn start_joiner_pairing(node: &NodeHandle, display_name: &str) -> anyhow::Result<()> {
    node.rpc("start_joiner_pairing", json!({ "displayName": display_name }))
        .await
        .map(|_| ())
}

/// ZEB-490: select the discovered peer by its session id (mutual — BOTH sides
/// must select each other before either advances to Handshaking).
pub async fn select_pairing_peer(node: &NodeHandle, peer_session_id: &str) -> anyhow::Result<()> {
    node.rpc(
        "select_pairing_peer",
        json!({ "peerSessionId": peer_session_id }),
    )
    .await
    .map(|_| ())
}

/// ZEB-490: confirm the SAS — exchanges the encrypted Confirm; the inviter then
/// signs the EnrollmentCert.
pub async fn confirm_pairing_sas(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("confirm_pairing_sas", json!({})).await.map(|_| ())
}

/// ZEB-490: snapshot the current PairingState ({ kind, ...fields }, camelCase).
pub async fn get_pairing_state(node: &NodeHandle) -> anyhow::Result<Value> {
    node.rpc("get_pairing_state", json!({})).await
}

/// ZEB-490: abort an in-progress pairing.
pub async fn cancel_pairing(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("cancel_pairing", json!({})).await.map(|_| ())
}

/// ZEB-490: pin/clear a fleet device as butler. `device_id` is the 64-hex
/// ed25519 verify key (the enrolled-set id), NOT the 32-hex identity hash.
pub async fn set_butler_pin(node: &NodeHandle, device_id: Option<&str>) -> anyhow::Result<()> {
    node.rpc("set_butler_pin", json!({ "deviceId": device_id }))
        .await
        .map(|_| ())
}

/// ZEB-490: read this fleet's butler pin status ({ pinnedDeviceId, pinnedAtMs }).
pub async fn get_butler_pin(node: &NodeHandle) -> anyhow::Result<Value> {
    node.rpc("get_butler_pin", json!({})).await
}

/// ZEB-490: list the butler-held dm-inbox entries on this node. A missing/non-
/// array `held` is a broken response CONTRACT (surface it), mirroring
/// `get_relay_held`, so a characterize fallback can't read a broken response as
/// "nothing held".
pub async fn get_butler_held(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("get_butler_held", json!({})).await?;
    let held = v
        .get("held")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("get_butler_held response missing 'held' array: {v}"))?;
    Ok(held.clone())
}
```

- [ ] **Step 2: Verify it compiles + lints clean**

Run: `cd e2e-harness && cargo clippy --all-targets --features e2e -- -D warnings`
Expected: 0 warnings, 0 errors.

Run: `cd e2e-harness && cargo fmt -- --check`
Expected: no diff.

- [ ] **Step 3: Commit**

```bash
git add e2e-harness/src/driver.rs
git commit -m "feat(zeb-490): e2e-harness pairing + butler RPC driver wrappers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: `pair_into_fleet` orchestrator

**Files:**
- Modify: `e2e-harness/src/driver.rs` (add `pair_into_fleet` + two private poll helpers)

The PairingState flow (camelCase): `Idle → Discovering → Discovered{peers:[{sessionId,role,joinerEd25519VerifyHex,…}]} → Handshaking{sasDigits} → WaitingPeerConfirm → Enrolling → Complete{deviceIdHex}`. `set_butler_pin` needs the joiner's **64-hex `joinerEd25519VerifyHex`** (captured from the inviter's Discovered peers), NOT `Complete.deviceIdHex`.

- [ ] **Step 1: Add the orchestrator + helpers**

Append to `e2e-harness/src/driver.rs` (after the wrappers from Task 2):

```rust
/// ZEB-490: drive the real ZEB-446 SAS pairing handshake between two local
/// nodes until the joiner is enrolled into the inviter's fleet. Returns the
/// joiner's 64-hex ed25519 verify key — the device id `set_butler_pin` expects.
///
/// Mutual-selection contract (state_machine `maybe_advance_to_handshake`
/// requires `sent_select && received_select`): both sides SelectPeer each other.
/// Each `poll_until` gets the full `deadline` budget independently (matches the
/// existing per-step budgeting in s6). Returns `Err` on any timeout so the
/// caller's boundary-1 characterize fallback can fire instead of panicking.
pub async fn pair_into_fleet(
    inviter: &NodeHandle,
    joiner: &NodeHandle,
    display: &str,
    deadline: Duration,
) -> anyhow::Result<String> {
    start_inviter_pairing(inviter, &format!("{display}-P")).await?;
    start_joiner_pairing(joiner, &format!("{display}-B2")).await?;

    // Inviter discovers the joiner; capture the joiner's session id + its
    // ed25519 verify key (the pin device id). Both sides keep broadcasting
    // DISCOVER until they SelectPeer, so polling them sequentially here (before
    // any select) still lets both discover each other.
    let (joiner_session, joiner_vk) = poll_until(deadline, || async {
        let st = get_pairing_state(inviter).await?;
        if st.get("kind").and_then(Value::as_str) != Some("discovered") {
            return Ok(None);
        }
        let Some(peer) = st
            .get("peers")
            .and_then(Value::as_array)
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p.get("role").and_then(Value::as_str) == Some("joiner"))
            })
            .cloned()
        else {
            return Ok(None);
        };
        match (
            peer.get("sessionId").and_then(Value::as_str),
            peer.get("joinerEd25519VerifyHex").and_then(Value::as_str),
        ) {
            (Some(sid), Some(vk)) => Ok(Some((sid.to_string(), vk.to_string()))),
            _ => Ok(None),
        }
    })
    .await?;

    // Joiner discovers the inviter; capture the inviter's session id.
    let inviter_session = poll_until(deadline, || async {
        let st = get_pairing_state(joiner).await?;
        if st.get("kind").and_then(Value::as_str) != Some("discovered") {
            return Ok(None);
        }
        Ok(st
            .get("peers")
            .and_then(Value::as_array)
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p.get("role").and_then(Value::as_str) == Some("inviter"))
            })
            .and_then(|p| p.get("sessionId").and_then(Value::as_str))
            .map(str::to_string))
    })
    .await?;

    // Mutual select.
    select_pairing_peer(inviter, &joiner_session).await?;
    select_pairing_peer(joiner, &inviter_session).await?;

    // Both derive SAS; assert the codes match (the real security property).
    let inviter_sas = poll_pairing_sas(inviter, deadline).await?;
    let joiner_sas = poll_pairing_sas(joiner, deadline).await?;
    assert_sas_match(&inviter_sas, &joiner_sas)?;

    // Both confirm; the inviter signs the EnrollmentCert.
    confirm_pairing_sas(inviter).await?;
    confirm_pairing_sas(joiner).await?;

    // Both reach Complete (joiner now enrolled in the inviter's fleet).
    poll_pairing_complete(inviter, deadline).await?;
    poll_pairing_complete(joiner, deadline).await?;

    Ok(joiner_vk)
}

/// Poll a node until it is `Handshaking`, returning its 6-digit `sasDigits`.
async fn poll_pairing_sas(node: &NodeHandle, deadline: Duration) -> anyhow::Result<String> {
    poll_until(deadline, || async {
        let st = get_pairing_state(node).await?;
        if st.get("kind").and_then(Value::as_str) != Some("handshaking") {
            return Ok(None);
        }
        Ok(st
            .get("sasDigits")
            .and_then(Value::as_str)
            .map(str::to_string))
    })
    .await
}

/// Poll a node until it reaches `Complete` (enrollment finished).
async fn poll_pairing_complete(node: &NodeHandle, deadline: Duration) -> anyhow::Result<()> {
    poll_until(deadline, || async {
        let st = get_pairing_state(node).await?;
        Ok((st.get("kind").and_then(Value::as_str) == Some("complete")).then_some(()))
    })
    .await
}
```

- [ ] **Step 2: Verify it compiles + lints clean**

Run: `cd e2e-harness && cargo clippy --all-targets --features e2e -- -D warnings`
Expected: 0 warnings. (`pair_into_fleet` is `pub` so it won't be dead-code; the two `async fn poll_pairing_*` are used by it.)

Run: `cd e2e-harness && cargo fmt -- --check`
Expected: no diff.

- [ ] **Step 3: Commit**

```bash
git add e2e-harness/src/driver.rs
git commit -m "feat(zeb-490): pair_into_fleet orchestrator drives real SAS pairing to enrollment

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: `s7_butler_deposit_recover` scenario

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (add the scenario + an inline 3-node spawn that mints only A + P)

Mirrors `s6_relay_deposit_recover` (`e2e_two_node.rs:1213`). B2 is spawned **without** minting (a fresh `NodeHandle::spawn` boots unminted — see `single_node_mints_owner`; B2 acquires identity by enrolling into P's fleet during pairing).

- [ ] **Step 1: Add the scenario**

Append to `e2e-harness/tests/e2e_two_node.rs` (after `s6_relay_deposit_recover`, end of file):

```rust
// ─────────────────────────────────────────────────────────────────────────────
// ZEB-490 — s7: butler deposit→recover (co-located).
//
// A (sender) befriends P (recipient primary). P pairs a SECOND local instance
// B2 into its fleet via the real ZEB-446 SAS handshake, then pins B2 as butler.
// With P offline, A creates the DM Space + sends — after the no-ack windows the
// deposit fans out to P's butler B2. P relaunches, fleet-merges with B2,
// recovers the deposited invite + message.
//
// Layered characterize fallbacks at three racy co-located boundaries (the s6
// pattern): (1) pairing may not establish (Zenoh harmony/pairing/v2/lan/** is
// the same transport class as the ZEB-466 gap); (2) the deposit may not land on
// B2; (3) recovery may not complete. Every boundary that DOES establish becomes
// a hard assertion. set_butler_pin + get_butler_pin are hard-asserted whenever
// boundary 1 passes (the guaranteed-value core). The cross-WAN playbook
// Scenario D3 is the authoritative proof; this is its co-located sibling.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s7_butler_deposit_recover() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    // --- Spawn A (sender) + P (primary) + B2 (fresh joiner). Mint A + P only;
    //     B2 stays unminted and acquires identity by enrolling into P's fleet.
    let mut run = RunDir::new("s7").expect("run dir");
    let a_home = fresh_home("s7-a");
    let p_home = fresh_home("s7-p");
    let b2_home = fresh_home("s7-b2");
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let a = NodeHandle::spawn(mk(&a_home, "alice")).await.expect("spawn a");
    let mut p = NodeHandle::spawn(mk(&p_home, "primary")).await.expect("spawn p");
    let b2 = NodeHandle::spawn(mk(&b2_home, "butler")).await.expect("spawn b2");
    a.rpc("mint_owner_identity", json!({})).await.expect("a mint");
    p.rpc("mint_owner_identity", json!({})).await.expect("p mint");

    let a_owner = owner_id(&a).await;
    let p_owner = owner_id(&p).await;

    // --- Boundary 1: pair B2 into P's fleet via the real SAS handshake.
    let b2_device = match pair_into_fleet(&p, &b2, "s7", Duration::from_secs(180)).await {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!(
                "S7 FINDING: SAS pairing did not establish co-located within 180s \
                 ({e}). Pairing discovery rides Zenoh harmony/pairing/v2/lan/** — \
                 the same transport class as the ZEB-466 co-located gap. File a \
                 finding ticket; the cross-WAN Scenario D3 is the real proof. \
                 Skipping pin/HELD/RECV/CLEARED."
            );
            run.mark_success();
            drop((a, p, b2, a_home, p_home, b2_home));
            return;
        }
    };
    eprintln!("S7 PAIRED: B2 enrolled into P's fleet (device {b2_device}).");

    // --- Hard assertion: pin B2 as butler and read it back. The enrolled-set
    //     gate (set_butler_pin_inner) accepts B2 because it is genuinely enrolled.
    set_butler_pin(&p, Some(&b2_device))
        .await
        .expect("pin B2 as butler");
    let pin = get_butler_pin(&p).await.expect("get butler pin");
    assert_eq!(
        pin.get("pinnedDeviceId").and_then(Value::as_str),
        Some(b2_device.as_str()),
        "get_butler_pin reflects the pinned butler device"
    );
    eprintln!("S7 PIN: P pinned B2 as butler; get_butler_pin confirms.");

    // --- Friendship A<->P while P ONLINE (so A's device directory learns P's
    //     fleet devices incl. B2's reachability). Reuse s6's friend dance.
    let token = generate_friend_token(&a).await.expect("friend token");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("P never redeemed A's friend token within 120s; last error: {last_err}");
        }
        match redeem_friend_token(&p, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&a, &p_owner).await?;
        Ok(friend_is_active(&a, &p_owner).await?.then_some(()))
    })
    .await
    .expect("A has P as active friend");
    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&p, &a_owner).await?.then_some(()))
    })
    .await
    .expect("P has A as active friend");

    // --- P goes OFFLINE (real SIGKILL). The DM Space does NOT exist on P yet.
    p.kill().await.expect("kill p");

    // --- A creates the DM Space + sends. P unreachable → after the no-ack
    //     windows the deposit fans out to P's butler B2.
    let a_space = add_dm_space(&a, "s7-dm", &p_owner)
        .await
        .expect("a dm space");
    send_dm(&a, &a_space, b"butler-durable-hello", "text/plain")
        .await
        .expect("a send_dm accepted by the engine");

    // --- Boundary 2 (HELD): B2 holds the deposit for P while P is offline.
    //     Generous budget — deposit fires only after DEPOSIT_NOACK_WINDOWS=2.
    let held = poll_until(Duration::from_secs(90), || async {
        let entries = get_butler_held(&b2).await?;
        Ok(entries
            .into_iter()
            .find(|e| e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())))
    })
    .await;

    let held_entry = match held {
        Ok(e) => e,
        Err(_) => {
            eprintln!(
                "S7 FINDING: butler deposit never landed on B2 within 90s co-located \
                 (held=false). The sender may not resolve/dial P's butler co-located, \
                 or DEPOSIT_NOACK_WINDOWS exceeded the budget. File a finding ticket; \
                 confirm via the cross-WAN Scenario D3. Skipping RECV/CLEARED."
            );
            run.mark_success();
            drop((a, p, b2, a_home, p_home, b2_home));
            return;
        }
    };
    let held_space = held_entry
        .get("spaceIdHex")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let held_cid = held_entry
        .get("messageCidHex")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    eprintln!("S7 HELD: B2 is holding A's deposit for P (space {held_space}, cid {held_cid}).");

    // --- P comes back ONLINE; fleet-merges with B2, recovers the deposited
    //     invite + message (apply_deposited_invite bootstraps the DM Space).
    p = p.relaunch().await.expect("relaunch p");

    // --- Boundary 3 (RECV): A's plaintext shows up in P's thread post-reconnect.
    let recovered = poll_until(Duration::from_secs(120), || async {
        let msgs = read_dm_plaintext_any(&p, &[a_space.as_str()])
            .await
            .unwrap_or_default();
        Ok(msgs
            .iter()
            .any(|(_, body)| body == b"butler-durable-hello")
            .then_some(()))
    })
    .await;

    if recovered.is_err() {
        eprintln!(
            "S7 FINDING: P did not recover the deposited DM within 120s co-located. \
             B2-held the deposit (HELD passed) but the P<->B2 fleet sync or \
             apply_deposited_invite recovery did not complete co-located. File a \
             finding ticket; confirm via the cross-WAN Scenario D3. Skipping CLEARED."
        );
        run.mark_success();
        drop((a, p, b2, a_home, p_home, b2_home));
        return;
    }
    eprintln!("S7 RECV: P recovered the butler-deposited DM after reconnect.");

    // --- CLEARED: butler's `ingested_by` is a grow-only set (the recovered
    //     signal). Once P ingests, the held entry either gains a device in
    //     ingestedByDevices OR is GC'd away — accept either as cleared.
    let cleared = poll_until(Duration::from_secs(60), || async {
        let entries = get_butler_held(&b2).await?;
        let entry = entries
            .iter()
            .find(|e| e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str()));
        let done = match entry {
            None => true, // entry GC'd away after recovery
            Some(e) => e
                .get("ingestedByDevices")
                .and_then(Value::as_array)
                .map(|arr| !arr.is_empty())
                .unwrap_or(false),
        };
        Ok(done.then_some(()))
    })
    .await;
    assert!(
        cleared.is_ok(),
        "CLEARED: B2's held entry recorded P's recovery (ingestedByDevices grew or entry GC'd)"
    );
    eprintln!("S7 CLEARED: B2 recorded P's recovery of the deposit.");

    run.mark_success();
    drop((a, p, b2, a_home, p_home, b2_home));
}
```

- [ ] **Step 2: Verify the scenario compiles + lists (no run yet)**

Run: `cd e2e-harness && cargo nextest list --features e2e -E 'test(s7_butler_deposit_recover)'`
Expected: lists `s7_butler_deposit_recover` (compiles clean).

Run: `cd e2e-harness && cargo clippy --all-targets --features e2e -- -D warnings`
Expected: 0 warnings.

Run: `cd e2e-harness && cargo fmt -- --check`
Expected: no diff.

- [ ] **Step 3: Commit**

```bash
git add e2e-harness/tests/e2e_two_node.rs
git commit -m "test(zeb-490): s7_butler_deposit_recover co-located scenario

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: build `harmony-app`, run `s7`, capture the outcome

**Files:** none (this task runs the scenario and records which boundary it reached).

- [ ] **Step 1: Build the harmony-app binary the harness spawns**

Run: `cd src-tauri && cargo build --bin harmony-app`
Expected: builds (can take several minutes cold; supervise with a wall-clock safety net per the long-running-background rule — foreground with a generous timeout, or a ScheduleWakeup heartbeat).

- [ ] **Step 2: Run s7 (serial, release, single test)**

Run: `cd e2e-harness && cargo nextest run --features e2e --release -E 'test(s7_butler_deposit_recover)' --test-threads 1`
Expected: the test PASSES (it always `mark_success()`es — either via a full HELD→RECV→CLEARED assert chain or via an `S7 FINDING:` characterize fallback). Read the captured stderr (the `S7 …` lines + the run-dir logs) to determine which boundary it reached.

> Supervision: this scenario can take 5+ minutes (pairing + iroh first-contact friend handshake + the deposit no-ack windows). Use the long-running-background discipline — never trust a stale output mtime as "hung"; `pgrep cargo/harmony-app` before any kill; be patient.

- [ ] **Step 3: Record the outcome**

- If the **full chain asserted** (`S7 PAIRED → PIN → HELD → RECV → CLEARED`): this is the first co-located proof of the butler deposit→recover durability path. Note it in the PR body.
- If it **characterized at a boundary** (`S7 FINDING: …`): file a Linear follow-up ticket (sibling to ZEB-488) describing the exact boundary that stalled co-located (pairing / deposit / recovery), the transport hypothesis, and that the cross-WAN Scenario D3 is the authoritative proof. **Never invent the ticket id** — file it, then reference the assigned id in the PR. Note which boundaries DID assert (e.g. PAIRED + PIN proven even if HELD characterized).

No commit in this task unless the run surfaces a needed code fix (e.g. a wrong camelCase key) — if so, fix, re-run, and commit the fix.

---

### Task 6: final gate sweep + PR

**Files:** none (gates + PR).

- [ ] **Step 1: Full e2e-harness gate sweep**

```bash
cd e2e-harness
cargo fmt -- --check
cargo clippy --all-targets --features e2e -- -D warnings
cargo nextest run -E 'test(assert_sas_match)'
cargo nextest list --features e2e -E 'test(s7_butler_deposit_recover)'
```
Expected: fmt clean, clippy 0 warnings, unit tests pass, scenario lists.

- [ ] **Step 2: Sanity-check no `src-tauri/src` production code was touched**

Run: `git diff --stat origin/main...HEAD -- src-tauri/src`
Expected: empty (this is pure harness/test + docs work).

- [ ] **Step 3: Push + open the PR**

```bash
git push -u origin zeb-490-harness-pairing-helper-butler-s7
```

Open the PR with `gh pr create --repo zeblithic/harmony-client`. Body must:
- Use **`Closes ZEB-490`** only (no parent/ref ids in close-trigger format — Linear closes every ZEB-NNN in the body).
- Summarize Unit 1 (pairing helper + wrappers) + Unit 2 (s7) + the layered characterize design.
- State the **s7 run outcome** from Task 5 (full proof vs which boundary characterized + the follow-up ticket id if any).
- Note explicitly: **CI does not compile the e2e-harness crate** (it is not in the src-tauri workspace), so the harness code is validated by the local `e2e-harness` clippy/list/nextest gates, not by the PR's CI checks — the green CI run only covers the (unchanged) src-tauri/frontend.

- [ ] **Step 4: Drive the bot loop to convergence**

Qodo + CodeAnt auto-run; `@coderabbitai review` manually each round (auto-reviews are disabled repo-wide). Address every finding, one push per round, bundle fixes. Greptile auto-skips (excluded author) — scan its bucket only if Jake manually triggers it; never trigger it. At convergence (all bots clean + the structural CI checks green), pushover Jake at the ready-to-merge gate. **Do NOT self-merge.**

---

## Self-review

**Spec coverage:** Unit 1 helper (`pair_into_fleet` + 6 pairing + 3 butler wrappers + `assert_sas_match`) = Tasks 1–3 ✓. Unit 2 scenario `s7` = Task 4 ✓. Layered characterize fallbacks (3 boundaries) = Task 4 scenario body ✓. SAS-match unit coverage = Task 1 ✓. Run + capture outcome + file finding = Task 5 ✓. Gates + PR (with the CI-blind-to-harness note) = Task 6 ✓. Scope boundary (no src-tauri change, no cross-WAN, no CI wiring) = enforced by Task 6 Step 2 ✓.

**Placeholder scan:** No TBD/TODO. Every code step shows complete code. Task 5's "file a follow-up ticket" is a deliberate runtime decision (outcome-dependent), not a placeholder — the *what* and *how* are fully specified.

**Type consistency:** `assert_sas_match(&str, &str) -> anyhow::Result<()>` defined in Task 1, used in Task 3 ✓. `get_pairing_state`/`set_butler_pin`/`get_butler_held`/`get_butler_pin` signatures defined in Task 2, used in Tasks 3–4 ✓. `pair_into_fleet(&NodeHandle, &NodeHandle, &str, Duration) -> Result<String>` defined in Task 3, called in Task 4 with `(&p, &b2, "s7", Duration::from_secs(180))` ✓. camelCase keys (`displayName`/`peerSessionId`/`deviceId`/`pinnedDeviceId`/`senderOwnerHex`/`spaceIdHex`/`messageCidHex`/`ingestedByDevices`) match the verified DTO/arg-struct serde ✓. `b2_device` (64-hex `joinerEd25519VerifyHex`) returned by `pair_into_fleet` and passed to `set_butler_pin` ✓.
