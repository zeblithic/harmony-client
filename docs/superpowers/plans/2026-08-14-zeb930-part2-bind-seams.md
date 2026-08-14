# ZEB-930 Parts 2–3 — beacon/pkarr bind seams + boot over-dial — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close the beacon/pkarr fail-open gap in R4 bounded-degree dialing by
forwarding the vouch-verified enrolled key into the admission oracle before the
seed fires its kick; then quantify the boot over-dial and fix only if material.

**Architecture:** Thread `enrolled_vk: Option<[u8;32]>` through
`ReachabilityResolver::seed_from_pkarr`; it binds inside, before the
`update_with_source` that fires the auto-kick. Beacon caller passes the real key;
invite-redeem callers pass `None` (fail-open unchanged).

**Tech Stack:** Rust, tokio, cargo nextest. Run from `src-tauri/`.

## Global Constraints

- Run cargo from `src-tauri/`, always `--locked`, `--features test-fixtures` for
  `--all-targets`/integration runs.
- MSRV-safe: prefer `if let Some(..)` over `let-else` in new code.
- Two-hash discipline: `_device_hash` (`DeviceIdentityHash`, device-address,
  `[u8;16]`) ≠ `enrolled_vk` (enrolled Ed25519 signing key, `[u8;32]`). Never
  synthesize a placeholder enrolled vk — absence is `None`.
- Final pre-PR gate: full `cargo nextest run --locked --workspace --all-targets
  --features test-fixtures` + `clippy --all-targets` + `fmt --check`.

---

### Task 1: Thread `enrolled_vk` through `seed_from_pkarr` + resolver unit tests

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (`seed_from_pkarr` sig+body; 2 test callers; new test)
- Modify: `src-tauri/src/community_gateway_dial_driver.rs:~742` (beacon → `Some`); `:~1912` (test → `None`)
- Modify: `src-tauri/src/lib.rs` (3 invite-redeem callers → `None`)
- Modify: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs:~670` (→ `None`)

**Interfaces:**
- Produces: `seed_from_pkarr(&self, owner_addr: OwnerAddr, _device_hash: DeviceIdentityHash, enrolled_vk: Option<[u8;32]>, payload: ReachabilityAnnouncePayload)`

- [ ] **Step 1: Write the failing resolver unit test** (in `reachability_resolver.rs` `#[cfg(test)] mod tests`)

```rust
/// ZEB-930 Part 2: `seed_from_pkarr` binds the enrolled key BEFORE its internal
/// update fires the kick when given `Some(vk)`, so the seeded node is
/// classifiable the instant it is kicked. `None` leaves it fail-open (unchanged).
#[tokio::test]
async fn seed_from_pkarr_some_binds_none_fails_open() {
    use crate::admission_oracle::AdmissionOracle;
    let oracle = std::sync::Arc::new(AdmissionOracle::new(true));
    let r = ReachabilityResolver::new();
    r.set_admission_oracle(std::sync::Arc::clone(&oracle));
    oracle.publish_admitted(std::collections::BTreeSet::new()); // nothing admitted

    let owner = OwnerAddr([0x11; 16]);
    let dh = crate::owner_state_types::DeviceIdentityHash([0u8; 16]);
    let vk = [0xBB; 32];

    // Some(vk): binds -> node_id classifiable -> denied (bound to a non-admitted key).
    let node_some = node_id_bytes(0x42);
    r.seed_from_pkarr(owner, dh, Some(vk), make_payload(0x42, 1_000)).await;
    assert!(!oracle.admit(&node_some), "Some(vk) binds -> non-admitted key denied");

    // None: no bind -> node_id unknown -> fail-open.
    let node_none = node_id_bytes(0x43);
    r.seed_from_pkarr(owner, dh, None, make_payload(0x43, 1_000)).await;
    assert!(oracle.admit(&node_none), "None leaves node_id unbound -> fail-open");
}
```

- [ ] **Step 2: Run it — expect a COMPILE failure** (arity mismatch): `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(seed_from_pkarr_some_binds)'` → fails to build.

- [ ] **Step 3: Change the signature + body** (`reachability_resolver.rs`)

```rust
pub async fn seed_from_pkarr(
    &self,
    owner_addr: OwnerAddr,
    _device_hash: DeviceIdentityHash,
    enrolled_vk: Option<[u8; 32]>,
    payload: ReachabilityAnnouncePayload,
) {
    // ZEB-930 Part 2: forward the vouch-verified enrolled key into the admission
    // oracle BEFORE the update fires the supervisor auto-kick, so the seeded peer
    // is bounded-degree-classifiable the instant it is kicked (race-free). Only
    // callers holding a membership-verified key pass Some; the rest pass None and
    // stay fail-open. `_device_hash` is the device-ADDRESS notion — NOT this
    // enrolled Ed25519 vk; they never converge.
    if let Some(vk) = enrolled_vk {
        self.note_enrolled_binding(owner_addr.0, payload.iroh_node_id, vk);
    }
    let hlc = Hlc {
        wall_ms: payload.announced_at_ms,
        logical: 0,
        device_id: String::new(),
    };
    self.update_with_source(owner_addr, payload, hlc, ReachabilitySource::PkarrLive);
}
```

Also update the doc comment above it to mention the new `enrolled_vk` param.

- [ ] **Step 4: Update the beacon production caller** (`community_gateway_dial_driver.rs`, the `seed_from_pkarr` at ~742)

```rust
self.reachability
    .seed_from_pkarr(
        beacon_owner,
        DeviceIdentityHash([0u8; 16]),
        Some(hit.membership_device_vk),
        hit.payload,
    )
    .await;
```

- [ ] **Step 5: Update the 3 invite-redeem callers** in `lib.rs` (rung-0 ~64416, retry-dial ~64574, witness ~64750): insert `None` as the new 3rd argument, before the `routing`/`cand.routing` payload. Add a short `// ZEB-930: joiner holds no membership-verified key -> fail-open (unchanged)` at each.

- [ ] **Step 6: Update the remaining test callers to `None`**: `reachability_resolver.rs` (2 existing `seed_from_pkarr` tests), `community_gateway_dial_driver.rs:~1912`, `tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs:~670`.

- [ ] **Step 7: Run resolver tests + the new test** — expect PASS:

```
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(seed_from_pkarr) + test(admission_oracle_binding)'
```

- [ ] **Step 8: `cargo fmt --all` then commit**

```
git add -A && git commit -m "ZEB-930 Part 2: bind enrolled key on the beacon/pkarr seed path"
```

---

### Task 2: Beacon-path admission-oracle bind guard (driver level)

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (new test in `mod tests`)

**Interfaces:**
- Consumes: `harness(...)`, `beacon(...)`, `test_member(..)`, `FIXTURE_DEVICE_VK`, `h.resolver.set_admission_oracle`

- [ ] **Step 1: Write the guard test**

```rust
/// ZEB-930 Part 2: a vouch-verified beacon seed forwards node_id ->
/// membership_device_vk into the admission oracle, so the beacon peer is
/// bounded-degree-classifiable (not fail-open) the instant its seed-kick fires.
#[tokio::test]
async fn beacon_seed_binds_enrolled_key_in_admission_oracle() {
    use crate::admission_oracle::AdmissionOracle;
    let community = SpaceId([0x12; 16]);
    let (member_pub, member_owner) = test_member(2);
    let node_id = [0x2D; 32];
    let handle = SupervisorHandle::new();
    let h = harness(
        community,
        vec![member_owner],
        Some(beacon(member_pub, node_id)),
        true,
        Some(handle.clone()),
    );
    let oracle = std::sync::Arc::new(AdmissionOracle::new(true));
    h.resolver.set_admission_oracle(std::sync::Arc::clone(&oracle));

    h.driver.run_one_pass().await;

    // The seed must have bound node_id -> FIXTURE_DEVICE_VK. Prove it: with that
    // key NOT admitted the node is denied (bound), not fail-open; admitting it flips.
    oracle.publish_admitted(std::collections::BTreeSet::new());
    assert!(
        !oracle.admit(&node_id),
        "beacon seed must bind node_id -> enrolled key (not fail-open)"
    );
    oracle.publish_admitted(std::collections::BTreeSet::from([FIXTURE_DEVICE_VK]));
    assert!(
        oracle.admit(&node_id),
        "admitting the bound enrolled key makes the beacon node dialable"
    );
}
```

- [ ] **Step 2: Run it — expect PASS**: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(beacon_seed_binds_enrolled_key)'`

- [ ] **Step 3: `cargo fmt --all` then commit**: `ZEB-930 Part 2: guard beacon seed binds enrolled key in the oracle`

---

### Task 3: Part 3 — quantify the boot over-dial, fix only if material

**Files:**
- Investigate: `event_loop.rs` (oracle install ordering vs boot seeds), membership-apply/boot-reconcile seams that call `resolver.update(...)`.
- Then EITHER add a regression guard test (immaterial) OR bind at the durable-CRDT membership-replay seam (material).

- [ ] **Step 1: Audit boot ingest seams.** Enumerate every boot-time path that seeds the resolver and can fire a supervisor kick; classify each as binding (calls `note_enrolled_binding`) vs non-binding. Determine the fail-open window: how long a boot-seeded durable member stays unbound before `address_book_sync` delivers a binding row in steady state.

- [ ] **Step 2: Write the materiality verdict** into a short section of the findings and the ZEB-930 comment. **Surface it to the reviewer before any scope expansion.**

- [ ] **Step 3a (immaterial):** add a regression guard test pinning the property (e.g. oracle installs before the first kick-firing seed, or the fail-open window is bounded by design). No production change.

- [ ] **Step 3b (material):** call `note_enrolled_binding(owner, node_id, enrolled_vk)` at the durable-CRDT membership-replay seam before its `update`, with a unit test mirroring Task 1's bind assertion.

- [ ] **Step 4: `cargo fmt --all` then commit.**

---

### Completion

Use superpowers:finishing-a-development-branch: run the full workspace gate,
then push and open a PR to main (base `main`). Fire exactly one
`@coderabbitai review`; do not re-trigger any bot.

## Self-Review

- **Coverage:** Part 2 (spec §Part 2) → Tasks 1–2; Part 3 (spec §Part 3) → Task 3.
- **Types:** `enrolled_vk: Option<[u8;32]>`, `membership_device_vk: [u8;32]`,
  `OwnerAddr(pub [u8;16])`, `note_enrolled_binding(owner:[u8;16], node_id:[u8;32],
  enrolled_vk:[u8;32])` — consistent across tasks.
- **No placeholders:** all test/impl code is concrete; Task 3's deliverable is
  branch-conditional on a measured verdict, by design (ticket: "fix only if material").
