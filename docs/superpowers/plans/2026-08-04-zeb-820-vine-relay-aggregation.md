# Aggregated vine relay set — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every device publish the full aggregated ≤4-entry vine relay set (self + freshest siblings from the fleet-net roster) instead of `[self]`, so follow-only delivery survives any single device going offline.

**Architecture:** A new pure `build_vine_relay_set` in `fleet_net.rs` reuses the ZEB-852/856-hardened `butler_set_order` sort to pick the freshest own-devices from the fleet-replicated `FleetNetDoc`, caps at `VINE_RELAY_SET_MAX`, and force-includes the publisher's live self entry. `PkarrVinesPublisher` is wired to call it on every publish tick via an injected fleet-snapshot closure. YAML/wire format unchanged.

**Tech Stack:** Rust (`harmony-app` lib crate), `cargo nextest`, ciborium CBOR, pkarr.

## Global Constraints

- **Reuse, do not reimplement, the sort:** `build_vine_relay_set` MUST call `crate::fleet_net::butler_set_order`; do not copy or fork its staleness/clamp/ordering logic.
- **Cap at `VINE_RELAY_SET_MAX` (= 4):** never produce a set larger than the cap; `build_vines_record_blob` REJECTS an oversize set (`pkarr_vines.rs:86`).
- **Freshness window is `crate::butler_deposit::BUTLER_SET_FRESHNESS_MS` (= 900_000 ms):** the same window butlers use; compute `stale_before_ms = now_ms.saturating_sub(BUTLER_SET_FRESHNESS_MS)` INSIDE `build_vine_relay_set` (never make the caller invert it — `butler_set_order` recovers `now` from that exact relation, `fleet_net.rs:277`).
- **Wire format untouched:** `VineRelayRecordPayload`, `VineRelayEntry`, the slot-key derivation, and `VINE_RELAY_SET_MAX` are unchanged. No new struct fields.
- **Gate/retraction behavior untouched:** the share gate, `enable`/`disable`/`republish`, the reconcile lock, the empty-set retraction paths (ZEB-811/822), and the `no-endpoint → advertise nothing` short-circuit all keep their current behavior.
- **Verification gates (CLAUDE.md):** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, and `cargo nextest run --locked ... --features test-fixtures` must all pass. Run cargo from `src-tauri/`.
- **Relink cost:** iterate with lib-scoped runs (`-p harmony-app --lib`) to avoid relinking ~97 integration binaries; do ONE full `--all-targets` sweep before the PR.

---

### Task 1: Pure `build_vine_relay_set` in `fleet_net.rs`

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (add function after `build_butler_set`, i.e. after line ~433; add a `#[cfg(test)]` test module at end of file)

**Interfaces:**
- Consumes: `crate::fleet_net::butler_set_order(doc: &FleetNetDoc, stale_before_ms: u64) -> Vec<(String, FleetNetRow)>` (existing); `crate::pkarr_vines::{VineRelayEntry, VINE_RELAY_SET_MAX}` (existing); `crate::butler_deposit::BUTLER_SET_FRESHNESS_MS` (existing, = 900_000).
- Produces: `pub fn build_vine_relay_set(doc: &FleetNetDoc, self_device_id: &str, self_entry: crate::pkarr_vines::VineRelayEntry, now_ms: u64) -> Vec<crate::pkarr_vines::VineRelayEntry>` — used by Task 2.

- [ ] **Step 1: Write the failing tests.** Append this module to the END of `src-tauri/src/fleet_net.rs`:

```rust
#[cfg(test)]
mod vine_relay_set_tests {
    use super::*;
    use crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
    use crate::owner_state_types::Hlc;
    use crate::pkarr_vines::{VineRelayEntry, VINE_RELAY_SET_MAX};

    const SELF_ID: &str = "self-device";
    const SELF_EP: [u8; 32] = [0xEE; 32];

    fn row(ep: u8, relay: &str, wall_ms: u64) -> FleetNetRow {
        FleetNetRow {
            iroh_endpoint_id: [ep; 32],
            home_relay: relay.to_string(),
            seen_at: Hlc { wall_ms, logical: 0, device_id: String::new() },
            feed_binding: None,
        }
    }

    fn self_entry() -> VineRelayEntry {
        VineRelayEntry { iroh_endpoint_id: SELF_EP, home_relay: "https://self.example".to_string() }
    }

    fn doc_with(rows: &[(&str, FleetNetRow)]) -> FleetNetDoc {
        let mut d = FleetNetDoc::default();
        for (id, r) in rows {
            d.devices.insert((*id).to_string(), r.clone());
        }
        d
    }

    #[test]
    fn empty_doc_yields_self_only() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let out = build_vine_relay_set(&FleetNetDoc::default(), SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].iroh_endpoint_id, SELF_EP);
    }

    #[test]
    fn self_snapshot_row_replaced_by_live_entry() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        // self's snapshot row carries STALE transport (ep 0x11); must be dropped
        // in favor of self_entry's live ep 0xEE.
        let doc = doc_with(&[
            (SELF_ID, row(0x11, "https://old-self.example", now)),
            ("bb", row(0x22, "https://b.example", now)),
        ]);
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().filter(|e| e.iroh_endpoint_id == SELF_EP).count(), 1);
        assert!(out.iter().all(|e| e.iroh_endpoint_id != [0x11; 32]), "stale self row must be replaced");
        assert!(out.iter().any(|e| e.iroh_endpoint_id == [0x22; 32]));
    }

    #[test]
    fn caps_at_max_with_self_forced() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        // 6 fresh siblings, self NOT among them → self force-included, capped.
        let rows: Vec<(&str, FleetNetRow)> = vec![
            ("s0", row(0x30, "https://s.example", now)),
            ("s1", row(0x31, "https://s.example", now)),
            ("s2", row(0x32, "https://s.example", now)),
            ("s3", row(0x33, "https://s.example", now)),
            ("s4", row(0x34, "https://s.example", now)),
            ("s5", row(0x35, "https://s.example", now)),
        ];
        let out = build_vine_relay_set(&doc_with(&rows), SELF_ID, self_entry(), now);
        assert_eq!(out.len(), VINE_RELAY_SET_MAX);
        assert_eq!(out.iter().filter(|e| e.iroh_endpoint_id == SELF_EP).count(), 1);
        assert_eq!(out[0].iroh_endpoint_id, SELF_EP, "force-inserted self leads");
    }

    #[test]
    fn stale_sibling_excluded() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let doc = doc_with(&[
            ("bb", row(0x22, "https://b.example", now)),
            ("cc", row(0x33, "https://c.example", now - BUTLER_SET_FRESHNESS_MS - 1)),
        ]);
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 2, "self (forced) + fresh bb; stale cc excluded");
        assert!(out.iter().any(|e| e.iroh_endpoint_id == [0x22; 32]));
        assert!(out.iter().all(|e| e.iroh_endpoint_id != [0x33; 32]));
    }

    #[test]
    fn future_skewed_sibling_does_not_outrank_present() {
        // Inherits the ZEB-852 clamp from butler_set_order: an in-window but
        // future-dated sibling must not out-rank an honest present row. Both are
        // clamped to `now`, then the ascending device-id tiebreak decides.
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let doc = doc_with(&[
            ("bb-honest", row(0x22, "https://b.example", now)),
            ("zz-skewed", row(0x33, "https://z.example", now + BUTLER_SET_FRESHNESS_MS / 2)),
        ]);
        // self force-inserts at front; assert honest bb precedes skewed zz.
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        let pos = |ep: [u8; 32]| out.iter().position(|e| e.iroh_endpoint_id == ep).unwrap();
        assert!(pos([0x22; 32]) < pos([0x33; 32]), "clamped honest row must precede future-skewed row");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(vine_relay_set)'`
Expected: FAIL to compile — `build_vine_relay_set` is not defined yet.

- [ ] **Step 3: Implement `build_vine_relay_set`.** Insert immediately after `build_butler_set` (after line ~433, before `selection_view`):

```rust
/// Aggregate the creator's active devices into a vine relay set (max
/// [`crate::pkarr_vines::VINE_RELAY_SET_MAX`]). The vines analogue of
/// [`build_butler_set`], minus the `vk_lookup` layer: a `VineRelayEntry`
/// carries only `iroh_endpoint_id` + `home_relay`, both present directly in
/// `FleetNetRow`, so no per-device verify-key resolution is needed.
///
/// `self_entry` is the publishing device's own live transport data (it is
/// online by definition at publish time) and appears exactly once: when self's
/// snapshot row is in the fresh ordering it is replaced by `self_entry`'s
/// fresher data; when self's row is stale/missing or fresh siblings filled the
/// cap, `self_entry` is force-inserted at the front, evicting the
/// lowest-priority entry if the set is full.
///
/// Sibling ordering, staleness filtering, and the ZEB-852/856 peer-inflation
/// hardening all come from reusing [`butler_set_order`]. `now_ms` is the
/// receiver clock; the freshness window is `BUTLER_SET_FRESHNESS_MS`, and the
/// `stale_before_ms` inversion `butler_set_order` expects is computed HERE so
/// no caller can get it wrong.
///
/// Pin promotion is inherited but invisible: `butler_set_order` may lead with
/// the owner's pinned device, but `VineRelayEntry` has no `pinned` field, so
/// that affects only ORDER (a dialing-preference hint), never what a follower
/// observes.
pub fn build_vine_relay_set(
    doc: &FleetNetDoc,
    self_device_id: &str,
    self_entry: crate::pkarr_vines::VineRelayEntry,
    now_ms: u64,
) -> Vec<crate::pkarr_vines::VineRelayEntry> {
    use crate::pkarr_vines::{VineRelayEntry, VINE_RELAY_SET_MAX};

    let stale_before_ms = now_ms.saturating_sub(crate::butler_deposit::BUTLER_SET_FRESHNESS_MS);

    let mut out: Vec<VineRelayEntry> = Vec::new();
    let mut saw_self = false;
    for (dev_id, row) in butler_set_order(doc, stale_before_ms) {
        if out.len() >= VINE_RELAY_SET_MAX {
            break;
        }
        if dev_id == self_device_id {
            // Self appears once: replace its snapshot row with the fresher live
            // transport data captured at blob-build time.
            saw_self = true;
            out.push(self_entry.clone());
            continue;
        }
        out.push(VineRelayEntry {
            iroh_endpoint_id: row.iroh_endpoint_id,
            home_relay: row.home_relay.clone(),
        });
    }
    if !saw_self {
        // Self's row is stale/missing, or fresh siblings filled the cap. The
        // publisher is online NOW (it is publishing this record), so force its
        // live entry in, evicting the lowest-priority sibling if the set is full.
        if out.len() >= VINE_RELAY_SET_MAX {
            out.pop();
        }
        out.insert(0, self_entry);
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass.**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(vine_relay_set)'`
Expected: PASS (5 tests).

- [ ] **Step 5: Lint + format the change.**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/src/fleet_net.rs
git commit -m "feat(ZEB-820): build_vine_relay_set — aggregate fleet devices into a capped relay set"
```

---

### Task 2: Wire `build_vine_relay_set` into `PkarrVinesPublisher`

**Files:**
- Modify: `src-tauri/src/pkarr_vines_publisher.rs` (`build_blob`/`build_blob_or_retraction` signatures ~lines 39-86; `PkarrVinesPublisher` struct + `new` ~lines 95-145; `reconcile_locked` ~lines 236-283; all 9 test call sites; add one integration test)
- Modify: `src-tauri/src/lib.rs` (the `PkarrVinesPublisher::new` call site at line 9749)

**Interfaces:**
- Consumes: `crate::fleet_net::build_vine_relay_set` (Task 1); `crate::fleet_net::FleetNetDoc` (existing).
- Produces: `PkarrVinesPublisher::new(..)` with two new trailing params `self_device_id: String` and `fleet_snapshot: Arc<dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync>`.

- [ ] **Step 1: Change `build_blob` and `build_blob_or_retraction` to take a pre-built relay set.** Replace the bodies at `pkarr_vines_publisher.rs:39-86` with:

```rust
fn build_blob(
    share: bool,
    own_vine_count: usize,
    relay_set: Vec<VineRelayEntry>,
    now_ms: u64,
) -> Option<Vec<u8>> {
    if !share || own_vine_count == 0 {
        return None;
    }
    let payload = VineRelayRecordPayload { relay_set, issued_at_ms: now_ms };
    build_vines_record_blob(&payload).ok()
}
```

Leave `build_retraction_blob` (lines 64-70) unchanged. Then:

```rust
fn build_blob_or_retraction(
    share: bool,
    own_vine_count: usize,
    relay_set: Vec<VineRelayEntry>,
    now_ms: u64,
) -> Vec<u8> {
    build_blob(share, own_vine_count, relay_set, now_ms)
        .unwrap_or_else(|| build_retraction_blob(now_ms))
}
```

- [ ] **Step 2: Add the two new fields + constructor params.** In the `PkarrVinesPublisher` struct (after the `has_own_vines` field, ~line 115) add:

```rust
    /// SP1 64-hex fleet-net device id of THIS device — the key form used in
    /// `FleetNetDoc::devices` and passed to `build_vine_relay_set` so self's
    /// snapshot row is replaced by the live self entry rather than duplicated.
    self_device_id: String,
    /// Reads a fresh `FleetNetDoc` snapshot on every publish tick (captures the
    /// live `Arc<RwLock<FleetNetDoc>>` in prod; `Arc::new(FleetNetDoc::default)`
    /// in tests → the set collapses to `[self]`, preserving pre-ZEB-820 shape).
    fleet_snapshot: Arc<dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync>,
```

In `new` (lines 126-145) add the two params at the END of the parameter list and the two fields to the struct literal:

```rust
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        own_addr_hex: String,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        endpoint: Option<Arc<IrohEndpoint>>,
        share: Arc<AtomicBool>,
        has_own_vines: Arc<dyn Fn() -> usize + Send + Sync>,
        self_device_id: String,
        fleet_snapshot: Arc<dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            own_addr_hex,
            identity_signing_key,
            identity_pub,
            endpoint,
            share,
            has_own_vines,
            self_device_id,
            fleet_snapshot,
            reconcile_lock: tokio::sync::Mutex::new(()),
        }
    }
```

Also add the `VineRelayEntry` import if not already in scope — it is (line 27 imports `VineRelayEntry`). `VineRelayRecordPayload` is also imported there. Good.

- [ ] **Step 3: Rewire `reconcile_locked`'s record builder to aggregate.** In `reconcile_locked` (lines 236-283), the gate check currently is `if build_blob(share, own_vine_count, endpoint_id, home_relay, now_ms).is_some()`. Replace it with the explicit gate (endpoint already confirmed `Some` above), and inside the `record_builder` closure build the aggregated set from a fresh snapshot. The closure must additionally capture `fleet_snapshot` and `self_device_id`. Replace lines 250-282 with:

```rust
        if share && own_vine_count > 0 {
            let id_sk = self.identity_signing_key.clone();
            let id_pub = self.identity_pub;
            let endpoint_for_builder = Arc::clone(&endpoint);
            let share_flag = Arc::clone(&self.share);
            let has_own_vines = Arc::clone(&self.has_own_vines);
            let fleet_snapshot = Arc::clone(&self.fleet_snapshot);
            let self_device_id = self.self_device_id.clone();
            let record_builder: RecordBuilder = Arc::new(move |at_ms| {
                // Fresh read on EVERY publish (never boot-frozen — ZEB-521).
                let endpoint_id = *endpoint_for_builder.node_id().as_bytes();
                let home_relay = endpoint_for_builder
                    .home_relay()
                    .map(|r| r.to_string())
                    .unwrap_or_default();
                let share = share_flag.load(Ordering::Relaxed);
                let own_vine_count = has_own_vines();
                // ZEB-820: aggregate self + freshest siblings from a fresh fleet
                // snapshot instead of advertising only self.
                let self_entry = crate::pkarr_vines::VineRelayEntry {
                    iroh_endpoint_id: endpoint_id,
                    home_relay,
                };
                let relay_set = crate::fleet_net::build_vine_relay_set(
                    &(fleet_snapshot)(),
                    &self_device_id,
                    self_entry,
                    at_ms,
                );
                let blob = build_blob_or_retraction(share, own_vine_count, relay_set, at_ms);
                PkarrRoutingRecord::sign_new(
                    blob,
                    id_pub,
                    at_ms,
                    at_ms + REACHABILITY_RECORD_TTL_MS,
                    &id_sk,
                )
                .expect("sign — fixed-size buffers should not fail")
            });

            self.publisher
                .register(HANDLE.to_string(), self.key_builder(), record_builder)
                .await;
            return;
        }
```

The gate-closed block below it (the `active_handles` check + `register_retraction`) is unchanged. Note `now_ms`/`endpoint_id`/`home_relay` computed at lines 241-246 for the pre-check are now only used by the (removed) old gate call — the `share`/`own_vine_count` at lines 247-248 remain for the new `if share && own_vine_count > 0`. Remove the now-unused `endpoint_id`/`home_relay`/`now_ms` locals at 241-246 IF clippy flags them unused (the closure recomputes its own); keep whichever the gate-closed path still needs (it needs none of them). Verify with clippy in Step 6.

- [ ] **Step 4: Update all 9 test call sites + the pure-combinator tests.** Every `PkarrVinesPublisher::new(...)` in the test module (lines 477, 531, 609, 714, 821, 969, 1005, 1037, 1055) gains two trailing args:

```rust
        "self-device".to_string(),
        std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
```

Update the pure-combinator tests to the new `build_blob`/`build_blob_or_retraction` signatures. Replace the `test_builder` helper (lines 352-362) and the three combinator tests:

```rust
    fn self_relay_set() -> Vec<VineRelayEntry> {
        vec![VineRelayEntry {
            iroh_endpoint_id: TEST_SELF_ENDPOINT,
            home_relay: "https://relay.example".to_string(),
        }]
    }

    #[test]
    fn blob_absent_when_gate_off_or_no_vines() {
        assert!(build_blob(false, 3, self_relay_set(), 1_000).is_none());
        assert!(build_blob(true, 0, self_relay_set(), 1_000).is_none());
    }

    #[test]
    fn blob_encodes_given_relay_set_when_enabled() {
        let blob = build_blob(true, 3, self_relay_set(), 1_000).expect("enabled with vines publishes");
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(p.relay_set.len(), 1);
        assert_eq!(p.relay_set[0].iroh_endpoint_id, TEST_SELF_ENDPOINT);
    }

    #[test]
    fn retraction_blob_when_gate_flips_closed_after_registration() {
        let blob = build_blob_or_retraction(true, 0, self_relay_set(), 1_000);
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert!(p.relay_set.is_empty(), "gate closed — must retract");
        assert_eq!(p.issued_at_ms, 1_000);
    }

    #[test]
    fn full_blob_when_gate_open() {
        let blob = build_blob_or_retraction(true, 3, self_relay_set(), 1_000);
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(p.relay_set.len(), 1);
        assert_eq!(p.relay_set[0].iroh_endpoint_id, TEST_SELF_ENDPOINT);
    }
```

(The old `blob_contains_self_entry_when_enabled` test is renamed to `blob_encodes_given_relay_set_when_enabled` above — remove the old one.)

- [ ] **Step 5: Update the production call site.** At `src-tauri/src/lib.rs:9749`, the `PkarrVinesPublisher::new(...)` call currently passes 7 args ending with `has_own_vines`. Add two trailing args. The self device id is the SAME 64-hex SP1 id the butler blob builder passes as `self_device_id` to `build_butler_set` (bound as `device_id` at lib.rs:9058); `fleet_net_snapshot` (the `Arc<RwLock<FleetNetDoc>>`) is in scope here (it is cloned at lib.rs:9883, just below). Insert after `has_own_vines,`:

```rust
                            device_id.clone(),
                            {
                                let fs = std::sync::Arc::clone(&fleet_net_snapshot);
                                std::sync::Arc::new(move || {
                                    fs.read().unwrap_or_else(|p| p.into_inner()).clone()
                                })
                                    as std::sync::Arc<
                                        dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync,
                                    >
                            },
```

If `device_id` is not in scope at that exact line, source the same 64-hex SP1 id the butler builder uses (grep `self_device_id:` / the `build_butler_set(&snap, &device_id, ...)` call at ~9058 in the same boot fn). The compile in Step 6 confirms the binding.

- [ ] **Step 6: Build, lint, and run the publisher tests.**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean (resolve any unused-local warning from Step 3 by deleting the now-dead `endpoint_id`/`home_relay`/`now_ms` pre-check locals).

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(pkarr_vines)'`
Expected: PASS — existing publisher tests green (default snapshot → `[self]`, so single-entry assertions hold).

- [ ] **Step 7: Add the end-to-end aggregation test.** Append to the `pkarr_vines_publisher.rs` test module (mirror `disable_after_enable_publishes_retraction`'s real-identity setup — a placeholder address fails `verify_vines_record`'s binding):

```rust
    /// ZEB-820: with a fleet snapshot carrying fresh siblings, the published
    /// record resolves to the AGGREGATED set (self + siblings), not just self.
    #[tokio::test]
    async fn aggregated_set_includes_fresh_siblings() {
        let (publisher, relay) = test_publisher().await;
        let resolver = harmony_pkarr::PkarrResolver::new(single_relay_client(&relay));

        let endpoint = test_endpoint().await;
        let identity = crate::vine_signing::test_identity();
        let addr = crate::vine_signing::signer_address(&identity);

        // Two fresh siblings (ids differ from the publisher's self device id).
        const SIB_A: [u8; 32] = [0xA1; 32];
        const SIB_B: [u8; 32] = [0xB2; 32];
        let fleet = std::sync::Arc::new(move || {
            let now = now_ms();
            let mut doc = crate::fleet_net::FleetNetDoc::default();
            let mk = |ep: [u8; 32], relay: &str| crate::fleet_net::FleetNetRow {
                iroh_endpoint_id: ep,
                home_relay: relay.to_string(),
                seen_at: crate::owner_state_types::Hlc { wall_ms: now, logical: 0, device_id: String::new() },
                feed_binding: None,
            };
            doc.devices.insert("sib-a".to_string(), mk(SIB_A, "https://a.example"));
            doc.devices.insert("sib-b".to_string(), mk(SIB_B, "https://b.example"));
            doc
        });

        let vp = PkarrVinesPublisher::new(
            std::sync::Arc::clone(&publisher),
            addr.clone(),
            crate::vine_signing::identity_signing_key(&identity),
            crate::vine_signing::identity_pub_64(&identity),
            Some(endpoint),
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(|| 1),
            "self-device".to_string(),
            fleet,
        );

        vp.enable().await;

        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "aggregated vines publish did not land");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if relay_set.iter().any(|e| e.iroh_endpoint_id == SIB_A)
                    && relay_set.iter().any(|e| e.iroh_endpoint_id == SIB_B)
                {
                    // Self is force-included too — the publisher's live endpoint.
                    assert!(relay_set.len() >= 3, "self + 2 siblings");
                    return;
                }
            }
        }
    }
```

- [ ] **Step 8: Run the new test.**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(aggregated_set_includes_fresh_siblings)'`
Expected: PASS.

- [ ] **Step 9: Full-gate sweep (CI parity) before commit.**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(pkarr_vines) + test(vine_relay_set) + test(fleet_net)'`
Expected: all clean/green. (This scoped `--all-targets` run compiles every target so integration-binary compile errors surface, per the `--all-targets` note in CLAUDE.md, without executing the whole 4,100-test suite.)

- [ ] **Step 10: Commit.**

```bash
git add src-tauri/src/pkarr_vines_publisher.rs src-tauri/src/lib.rs
git commit -m "feat(ZEB-820): publish aggregated vine relay set from the fleet-net roster"
```

## Self-Review

**1. Spec coverage:**
- Aggregator (`build_vine_relay_set`, reuse `butler_set_order`, cap, self force-include, staleness) → Task 1. ✓
- Every-device-publishes / no wire change → Task 2 (record builder aggregates on each tick; `VineRelayRecordPayload` untouched). ✓
- Wiring (`new` params, `reconcile_locked`, `lib.rs:9749`, self_device_id, fleet_snapshot closure) → Task 2 Steps 2/3/5. ✓
- Gate/retraction unchanged → Task 2 keeps the gate-closed block and retraction path untouched; existing retraction tests re-run in Step 6. ✓
- Testing (pure unit set + publisher default-snapshot preservation + sibling aggregation) → Task 1 Step 1, Task 2 Steps 4/7. ✓
- No gap found.

**2. Placeholder scan:** No TBD/TODO; all code is inline. The one conditional ("if `device_id` is not in scope") names the exact fallback lookup (the `build_butler_set` self-id at ~9058) and is resolved by the Step 6 compile — not a placeholder.

**3. Type consistency:** `build_vine_relay_set(&FleetNetDoc, &str, VineRelayEntry, u64) -> Vec<VineRelayEntry>` is defined identically in Task 1's Produces block and called with those exact arg types in Task 2 Step 3. `new`'s two new params (`String`, `Arc<dyn Fn() -> FleetNetDoc + Send + Sync>`) match between Task 2 Step 2 (definition), Step 4 (test call sites), and Step 5 (prod call site). `build_blob(bool, usize, Vec<VineRelayEntry>, u64)` matches between Step 1 (def) and Steps 4 (tests). Consistent.
