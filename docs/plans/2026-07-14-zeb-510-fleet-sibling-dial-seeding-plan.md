# ZEB-510 Same-Owner Fleet-Sibling Dial-Seeding — Step 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the already-persisted `FleetNetDoc` sibling iroh endpoints into the `ReachabilityResolver` so an owner's primary device P can dial its own butler sibling B2 for async-DM deposits.

**Architecture:** Add a new `ReachabilitySource::FleetSibling` with its own dedicated `fleet` slot in `ResolverSlots` (kept distinct from `durable`/`pkarr` so a sibling that is also a shared-community co-member cannot clobber it under the same resolver key). A pure `FleetNetRow → ReachabilityAnnouncePayload` mapper produces verification-exempt (zero-signature) entries. Two feed sites push sibling rows through that mapper into the resolver: a **boot-replay hook** in `start_node` (seeds siblings from the persisted `fleet_net.cbor` at startup) and a **live-merge hook** in the fleet-net snapshot-refresh task (propagates siblings that come online / change endpoint mid-session). Fleet entries are dial-able via `freshest()` but are excluded from the `maybe_refresh_stale` pkarr re-resolve path.

**Tech Stack:** Rust (Tauri backend, `src-tauri/`), `cargo nextest`, iroh transport, zenoh fleet-net sync, the e2e-harness two-node driver.

**Design doc:** `docs/specs/2026-07-14-zeb-510-fleet-sibling-dial-seeding-design.md` (approved 2026-07-14).

**Scope note (step 1 only):** This plan covers ONLY the FleetNetDoc→resolver wiring. Step 2 (the SAS first-contact seed store) is **gated on the empirical s7 result** and is explicitly NOT in this plan. See "The step-1-vs-step-2 gate" at the end.

## Global Constraints

- **CI gates (run from `src-tauri/`, all must pass before PR):**
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`
- **`--all-targets`, `--locked`, and `--features test-fixtures` are load-bearing** — never drop them from clippy/test commands (CLAUDE.md).
- **Per-source-slot invariant:** each `ReachabilitySource` writes ONLY its own slot in `ResolverSlots`. `FleetSibling` MUST use the new `fleet` slot, never `durable` or `pkarr`.
- **Self-row exclusion:** never feed P's own `device_id` row into the resolver (would make P dial itself). Both feed sites filter it out via `fleet_net::sibling_rows`.
- **Verification-exempt trust model:** `FleetSibling` payloads carry `identity_signature: [0u8; 64]`. This is safe because the resolver never verifies signatures — the ingest boundary is fleet-net's symmetric-key decrypt (only enrolled siblings holding the fleet KeyTree produce a decryptable row).
- **No pkarr refresh for fleet entries:** a same-owner sibling is not in `self_owner`'s pkarr blob (cross-WAN sibling rendezvous is the deferred ZEB-513 path). `maybe_refresh_stale` MUST skip `FleetSibling` entries.
- **RCH5 / display filters untouched:** the fleet feed bypasses the community-membership projection (`community_membership.rs`) and must NOT be routed through `filter_peers_by_shared_membership` (`network_health.rs`, display-only).
- **`cd` drifts between Bash calls** — always use absolute paths or `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && ...` in a single compound command.

---

### Task 1: `ReachabilitySource::FleetSibling` + dedicated `fleet` resolver slot

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (enum `:83-102`, `ResolverSlots` `:135-166`, `update_with_source` `:335-416`, `maybe_refresh_stale` `:506-583`)
- Test: `src-tauri/src/reachability_resolver.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `ReachabilitySource::FleetSibling` (new enum variant, `Copy`), `ReachabilitySource::as_dto_str` returns `"fleetSibling"` for it. `update_with_source(actor, payload, hlc, ReachabilitySource::FleetSibling)` writes the dedicated `fleet` slot, participates in `freshest()` (dial authority), is excluded from `durable_preferred()` (butler/diagnostics authority), and is skipped by `maybe_refresh_stale`. Signature of `update_with_source` is UNCHANGED: `pub fn update_with_source(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc, source: ReachabilitySource)`.
- Consumes: nothing new.

- [ ] **Step 1: Write the failing tests**

Add these tests to `mod fallback_tests` (`src-tauri/src/reachability_resolver.rs:1516` — the module that already contains `make_payload`, `StubFallback`, and the `use super::*; use async_trait::async_trait; use std::sync::Arc;` imports). Do NOT put them in the earlier `mod tests` (`:845`) — `make_payload`/`StubFallback` are not in scope there. The helpers reused: `make_payload(node_id_byte: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload` (`:1519`, sets `iroh_node_id: [node_id_byte; 32]` — depends on the seed byte ONLY, so two payloads with the same seed share a node id), `OwnerAddr([u8; 16])`, and the `StubFallback`/`ReachabilityFallback` pattern (`:1533-1542`).

```rust
    #[test]
    fn fleet_sibling_dto_tag() {
        assert_eq!(
            ReachabilitySource::FleetSibling.as_dto_str(),
            "fleetSibling"
        );
    }

    #[test]
    fn fleet_sibling_entry_is_dialable_via_node_id_and_freshest() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x51; 16]);
        let payload = make_payload(0xB2, 5000);
        r.update_with_source(
            owner,
            payload.clone(),
            Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );
        // Dial authority: resolvable by node id (uses freshest()).
        let (got_owner, got) = r
            .resolve_by_node_id(&payload.iroh_node_id)
            .expect("fleet entry resolvable by node id");
        assert_eq!(got_owner, owner);
        assert_eq!(got.iroh_node_id, payload.iroh_node_id);
        // Butler/diagnostics authority (durable_preferred) excludes fleet:
        // fleet rows carry an empty butler_set and must not shape butler views.
        assert!(
            r.resolve(&owner).is_empty(),
            "fleet-only key must not surface via durable_preferred resolve()"
        );
    }

    #[test]
    fn fleet_and_durable_slots_coexist_without_clobber_on_same_key() {
        // A sibling that is ALSO a shared-community co-member: its community
        // DurableCrdt record and its FleetSibling record share the SAME key
        // (self_owner, node_id). Distinct slots must keep both.
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x77; 16]);
        let node = 0xAB;
        let durable = make_payload(node, 100);
        let fleet = make_payload(node, 200); // fresher announce
        assert_eq!(durable.iroh_node_id, fleet.iroh_node_id, "same key");

        r.update_with_source(
            owner,
            durable.clone(),
            Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            ReachabilitySource::DurableCrdt,
        );
        r.update_with_source(
            owner,
            fleet.clone(),
            Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );

        // freshest() (dial) = the fresher fleet record.
        let (_, freshest) = r
            .resolve_by_node_id(&fleet.iroh_node_id)
            .expect("resolvable");
        assert_eq!(freshest.announced_at_ms, 200);
        // durable_preferred() (butler/diag) still returns the durable record —
        // proof the fleet write did NOT clobber the durable slot.
        let diag = r.resolve(&owner);
        assert_eq!(diag.len(), 1);
        assert_eq!(diag[0].announced_at_ms, 100);
    }

    struct CountingFallback {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl ReachabilityFallback for CountingFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Vec::new()
        }
    }

    #[tokio::test]
    async fn maybe_refresh_stale_skips_fleet_sibling_but_fires_for_durable() {
        let r = ReachabilityResolver::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&calls),
        }));

        // A deliberately STALE fleet entry (announced far in the past).
        let owner_f = OwnerAddr([0xF1; 16]);
        let fleet = make_payload(0xF1, 1_000);
        r.update_with_source(
            owner_f,
            fleet.clone(),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );
        // now_ms far past the staleness window; every OTHER early-return
        // condition (record present, fallback installed, cooldown fresh) is
        // satisfied, so a non-zero count could ONLY come from a missing guard.
        r.maybe_refresh_stale(owner_f, fleet.iroh_node_id, 1_000 + 10_000_000);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "fleet-sibling stale entry must not trigger a pkarr re-resolve"
        );

        // Positive control: a stale DURABLE entry DOES trigger the refresh.
        let owner_d = OwnerAddr([0xD1; 16]);
        let durable = make_payload(0xD1, 1_000);
        r.update_with_source(
            owner_d,
            durable.clone(),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            ReachabilitySource::DurableCrdt,
        );
        r.maybe_refresh_stale(owner_d, durable.iroh_node_id, 1_000 + 10_000_000);
        let mut fired = false;
        for _ in 0..200 {
            if calls.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                fired = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(fired, "durable stale entry must trigger a pkarr re-resolve");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_sibling) + test(fleet_and_durable) + test(maybe_refresh_stale_skips_fleet)'`
Expected: FAIL to compile — `no variant named FleetSibling found for enum ReachabilitySource`.

- [ ] **Step 3: Add the `FleetSibling` enum variant + `as_dto_str` arm**

In `src-tauri/src/reachability_resolver.rs`, extend the enum (currently `:83-90`) and its `as_dto_str` (currently `:96-100`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachabilitySource {
    /// Projected from a durable community-membership ReachabilityAnnounce CRDT
    /// event (persisted, replicated, boot-replayed).
    DurableCrdt,
    /// Fetched live from the recipient's pkarr routing blob.
    PkarrLive,
    /// ZEB-510: a same-owner fleet sibling's iroh endpoint, seeded from the
    /// owner's durable `FleetNetDoc` (fleet_net.cbor). Verification-exempt: the
    /// ingest boundary is fleet-net's symmetric-key decrypt, so the entry
    /// carries a zero-filled `identity_signature`. Never present in
    /// `self_owner`'s pkarr blob (that is the deferred ZEB-513 cross-WAN path).
    FleetSibling,
}

impl ReachabilitySource {
    /// Stable camelCase tag for the `connectivity_list_peer_reachability` DTO so
    /// consumers (e.g. the e2e durable-replication barrier) can tell a durable
    /// CRDT-replicated record apart from a live pkarr cache-back.
    pub fn as_dto_str(self) -> &'static str {
        match self {
            ReachabilitySource::DurableCrdt => "durableCrdt",
            ReachabilitySource::PkarrLive => "pkarrLive",
            ReachabilitySource::FleetSibling => "fleetSibling",
        }
    }
}
```

- [ ] **Step 4: Add the dedicated `fleet` slot + extend `freshest()`; leave `durable_preferred()` semantics unchanged**

Replace `ResolverSlots` (currently `:135-166`) with:

```rust
#[derive(Debug, Clone, Default)]
struct ResolverSlots {
    durable: Option<ResolverEntry>,
    pkarr: Option<ResolverEntry>,
    /// ZEB-510: same-owner fleet-sibling endpoint, seeded from `FleetNetDoc`.
    /// A DISTINCT slot (not `durable`) because a sibling that is also a shared-
    /// community co-member lands its community `DurableCrdt` record under the
    /// SAME resolver key `(self_owner, sibling_node_id)`; sharing a slot would
    /// let the two sources clobber each other, breaking the per-source-slot
    /// invariant this dual/tri-slot storage exists to protect.
    fleet: Option<ResolverEntry>,
}

impl ResolverSlots {
    /// Dial authority: the entry whose payload was announced most recently
    /// (greater `effective_announced_at_ms` — the future-skew-clamped announce
    /// time, ZEB-621). Ties break by source authority (durable > pkarr > fleet)
    /// so a verified community record still wins a tie against an unsigned
    /// fleet-sibling one, and the result is deterministic. `None` only for an
    /// empty triple (never stored — `update_with_source` writes at least one
    /// slot before any entry lands in the map).
    fn freshest(&self) -> Option<&ResolverEntry> {
        fn rank(s: ReachabilitySource) -> u8 {
            match s {
                ReachabilitySource::DurableCrdt => 2,
                ReachabilitySource::PkarrLive => 1,
                ReachabilitySource::FleetSibling => 0,
            }
        }
        [
            self.durable.as_ref(),
            self.pkarr.as_ref(),
            self.fleet.as_ref(),
        ]
        .into_iter()
        .flatten()
        .max_by(|a, b| {
            a.effective_announced_at_ms
                .cmp(&b.effective_announced_at_ms)
                .then_with(|| rank(a.source).cmp(&rank(b.source)))
        })
    }

    /// Butler / diagnostics authority: the durable slot if present, else pkarr.
    /// ZEB-510: the `fleet` slot is deliberately EXCLUDED — fleet entries carry
    /// an empty `butler_set` and are dial-only, so they must not shape the
    /// butler-set / diagnostics view. (Excluding them here also keeps a stale
    /// fleet entry out of the `maybe_refresh_stale` pkarr path when a key has no
    /// durable/pkarr slot at all.)
    fn durable_preferred(&self) -> Option<&ResolverEntry> {
        self.durable.as_ref().or(self.pkarr.as_ref())
    }
}
```

- [ ] **Step 5: Extend `update_with_source` — `was_present`, the slot-target match, and the HLC clamp**

In `update_with_source` (`:335-416`), make three edits.

(a) Extend `was_present` (currently `let was_present = slots.durable.is_some() || slots.pkarr.is_some();`) to include the fleet slot — otherwise a heartbeat re-feed of an already-known fleet sibling would spuriously re-fire `ReconnectTrigger::NewPeer` every time:

```rust
        let was_present =
            slots.durable.is_some() || slots.pkarr.is_some() || slots.fleet.is_some();
```

(b) Add the `FleetSibling` arm to the slot-target match (currently two arms):

```rust
        // Each source writes ONLY its own slot; same-source replacement is LWW.
        let target = match source {
            ReachabilitySource::DurableCrdt => &mut slots.durable,
            ReachabilitySource::PkarrLive => &mut slots.pkarr,
            ReachabilitySource::FleetSibling => &mut slots.fleet,
        };
```

(c) Add the `FleetSibling` arm to the HLC future-skew clamp match (currently `PkarrLive => clamp`, `DurableCrdt => hlc`). Fleet `seen_at` HLCs are authored by the sibling's own device (self-stamped by subject), so they are trusted verbatim for same-source LWW like `DurableCrdt` — no clamp. (The dial-freshness comparator `effective_announced_at_ms` is still clamped for ALL sources by the line just above this match, so a future-dated fleet row cannot permanently pin the dial route.)

```rust
        let hlc = match source {
            ReachabilitySource::PkarrLive => Hlc {
                wall_ms: hlc.wall_ms.min(skew_ceiling),
                ..hlc
            },
            ReachabilitySource::DurableCrdt | ReachabilitySource::FleetSibling => hlc,
        };
```

- [ ] **Step 6: Add the `FleetSibling` skip-guard to `maybe_refresh_stale`**

`maybe_refresh_stale` (`:506`) resolves the entry via `resolve_entry_by_node_id` (which uses `freshest()`, so a stale fleet entry WILL be found) and would then fire a pkarr re-resolve for `self_owner`. Add a guard immediately after the entry is resolved (right after the existing `let Some((_, entry)) = self.resolve_entry_by_node_id(&node_id) else { return; };`):

```rust
        let Some((_, entry)) = self.resolve_entry_by_node_id(&node_id) else {
            return;
        };
        // ZEB-510: a fleet-sibling entry is a same-owner LAN/fleet-net record,
        // never present in `self_owner`'s pkarr blob. A pkarr re-resolve for
        // `self_owner` would fetch P's OWN record (or no-op), never the
        // sibling's endpoint — so never refresh a fleet entry here. (Cross-WAN
        // sibling rendezvous over pkarr is the deferred ZEB-513 path.)
        if entry.source == ReachabilitySource::FleetSibling {
            return;
        }
```

- [ ] **Step 7: Run the new tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_sibling) + test(fleet_and_durable) + test(maybe_refresh_stale_skips_fleet)'`
Expected: PASS (4 tests: `fleet_sibling_dto_tag`, `fleet_sibling_entry_is_dialable_via_node_id_and_freshest`, `fleet_and_durable_slots_coexist_without_clobber_on_same_key`, `maybe_refresh_stale_skips_fleet_sibling_but_fires_for_durable`).

- [ ] **Step 8: Run the full resolver module + clippy to confirm no regressions**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability)' && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: all reachability tests PASS; clippy clean.

- [ ] **Step 9: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/reachability_resolver.rs && git commit -m "feat(zeb-510): add FleetSibling reachability source + dedicated fleet slot"
```

---

### Task 2: `FleetNetRow → ReachabilityAnnouncePayload` mapper + sibling-rows helper

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (add two free functions near `build_butler_set` / `selection_view`, `:194-345`)
- Test: `src-tauri/src/fleet_net.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub fn sibling_reachability_payload(row: &FleetNetRow) -> crate::reachability_record::ReachabilityAnnouncePayload` — pure mapper; zero-filled signature, empty butler set.
  - `pub fn sibling_rows(doc: &FleetNetDoc, self_device_id: &str) -> Vec<(String, FleetNetRow)>` — every device row EXCEPT `self_device_id`, as owned clones (so callers feed the resolver without holding the doc lock).
- Consumes: `FleetNetRow` (`:37-60`), `FleetNetDoc` (`:76-96`), `crate::reachability_record::ReachabilityAnnouncePayload` (`reachability_record.rs:81-133`).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src-tauri/src/fleet_net.rs`. (If no test module exists, add `#[cfg(test)] mod tests { use super::*; ... }`.) The `FleetNetRow`/`Hlc` field names come verbatim from `:37-60` and `owner_state_types.rs:318-326`.

```rust
    fn row(ep: u8, relay: &str, wall_ms: u64) -> FleetNetRow {
        FleetNetRow {
            iroh_endpoint_id: [ep; 32],
            home_relay: relay.to_string(),
            seen_at: crate::owner_state_types::Hlc {
                wall_ms,
                logical: 0,
                device_id: "dev".into(),
            },
            feed_binding: None,
        }
    }

    #[test]
    fn sibling_reachability_payload_maps_fields_and_is_unsigned() {
        let r = row(0xB2, "https://relay.example/", 4242);
        let p = sibling_reachability_payload(&r);
        assert_eq!(p.iroh_node_id, [0xB2; 32]);
        assert_eq!(p.home_relay_url, "https://relay.example/");
        assert_eq!(p.announced_at_ms, 4242);
        assert!(p.direct_addresses.is_empty());
        assert_eq!(p.identity_signature, [0u8; 64]); // verification-exempt
        assert!(p.butler_set.is_empty());
        assert_eq!(p.bs_at, 0);
    }

    #[test]
    fn sibling_rows_excludes_self_and_returns_the_rest() {
        let mut doc = FleetNetDoc::default();
        doc.devices.insert("self-id".into(), row(0x01, "a", 10));
        doc.devices.insert("sib-b2".into(), row(0x02, "b", 20));
        doc.devices.insert("sib-b3".into(), row(0x03, "c", 30));

        let out = sibling_rows(&doc, "self-id");
        let ids: std::collections::BTreeSet<&str> =
            out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(out.len(), 2);
        assert!(ids.contains("sib-b2"));
        assert!(ids.contains("sib-b3"));
        assert!(!ids.contains("self-id"), "self row must be excluded");
    }

    #[test]
    fn sibling_rows_empty_when_only_self_present() {
        let mut doc = FleetNetDoc::default();
        doc.devices.insert("self-id".into(), row(0x01, "a", 10));
        assert!(sibling_rows(&doc, "self-id").is_empty());
    }
```

> `FleetNetDoc` has an `impl Default` (`fleet_net.rs:98`), so `FleetNetDoc::default()` compiles. If the test module `mod tests` does not yet exist in `fleet_net.rs`, add `#[cfg(test)] mod tests { use super::*; … }` and put the `row` helper + these three tests inside it.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sibling_reachability_payload) + test(sibling_rows)'`
Expected: FAIL to compile — `cannot find function sibling_reachability_payload` / `sibling_rows`.

- [ ] **Step 3: Implement the mapper and helper**

Add to `src-tauri/src/fleet_net.rs` (near `selection_view`, `:333`). Field names are verbatim from `reachability_record.rs:81-133`.

```rust
/// ZEB-510: project a durable fleet-net device row into a dial-target
/// reachability payload for the [`crate::reachability_resolver::ReachabilityResolver`].
///
/// The row's `iroh_endpoint_id` becomes the payload's `iroh_node_id` (the
/// resolver keys on it). The payload is **verification-exempt**: `identity_
/// signature` is zero-filled because the trust boundary for a fleet row is
/// fleet-net's symmetric-key decrypt (only an enrolled sibling holding the
/// owner's fleet KeyTree produces a decryptable row), not a per-record
/// identity signature. `butler_set`/`bs_at` are empty — a sibling is a dial
/// target here, not advertising its own butlers — and `direct_addresses` is
/// empty because node-id-based dialing holepunches/relays (fleet rows carry no
/// direct addrs).
pub fn sibling_reachability_payload(
    row: &FleetNetRow,
) -> crate::reachability_record::ReachabilityAnnouncePayload {
    crate::reachability_record::ReachabilityAnnouncePayload {
        iroh_node_id: row.iroh_endpoint_id,
        home_relay_url: row.home_relay.clone(),
        direct_addresses: Vec::new(),
        announced_at_ms: row.seen_at.wall_ms,
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

/// ZEB-510: every device row in `doc` EXCEPT `self_device_id`, as owned clones.
///
/// Owned clones (not borrows) so callers can drop the `FleetNetDoc` lock before
/// feeding the resolver. The self row is excluded so P never dials itself.
pub fn sibling_rows(doc: &FleetNetDoc, self_device_id: &str) -> Vec<(String, FleetNetRow)> {
    doc.devices
        .iter()
        .filter(|(id, _)| id.as_str() != self_device_id)
        .map(|(id, row)| (id.clone(), row.clone()))
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sibling_reachability_payload) + test(sibling_rows)'`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/fleet_net.rs && git commit -m "feat(zeb-510): FleetNetRow->reachability payload mapper + sibling-rows helper"
```

---

### Task 3: Boot-replay hook — seed persisted siblings into the resolver at `start_node`

**Files:**
- Modify: `src-tauri/src/lib.rs` (`start_node`, immediately after the fleet-net self-row stamp block closes, `:5667`)

**Interfaces:**
- Consumes: `crate::fleet_net::sibling_rows` + `crate::fleet_net::sibling_reachability_payload` (Task 2), `crate::reachability_resolver::ReachabilitySource::FleetSibling` (Task 1). In-scope bindings at this site: `fleet_net_doc: Arc<tokio::sync::Mutex<FleetNetDoc>>`, `device_id: String` (self SP1 64-hex), `self_owner: OwnerAddr` (defined `:4715`), `reachability_resolver: ReachabilityResolver` (`:4308`/`:4404`).
- Produces: at boot, one `FleetSibling` resolver entry per persisted non-self device row.

- [ ] **Step 1: Add the boot-replay hook**

In `src-tauri/src/lib.rs`, insert this block immediately AFTER the self-row stamp block closes at `:5667` (the closing `}` of `if let Some(ep_arc) = iroh_endpoint_arc.as_ref() { … }`) and BEFORE `fleet_net_doc_opt = Some(std::sync::Arc::clone(&fleet_net_doc));` (`:5668`). The snapshot-then-feed pattern avoids holding the doc lock across the resolver writes.

```rust
                    // ZEB-510: seed same-owner fleet siblings' iroh endpoints
                    // into the reachability resolver so P can dial a butler
                    // sibling B2 for async-DM deposits. `fleet_net.cbor` already
                    // persists each enrolled sibling's endpoint (consumed today
                    // only to build the pkarr butler-set advert) — this is the
                    // missing wire from that durable doc to the dialer. The self
                    // row is excluded (never dial ourselves). Mirrors the
                    // ReachabilityAnnounce boot-replay further below (~7912).
                    {
                        let siblings = {
                            let doc = fleet_net_doc.lock().await;
                            crate::fleet_net::sibling_rows(&doc, &device_id)
                        };
                        for (_dev_id, row) in siblings {
                            reachability_resolver.update_with_source(
                                self_owner,
                                crate::fleet_net::sibling_reachability_payload(&row),
                                row.seen_at.clone(),
                                crate::reachability_resolver::ReachabilitySource::FleetSibling,
                            );
                        }
                    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --features test-fixtures`
Expected: compiles clean. (If `self_owner` is not in scope here, search `let self_owner` in `start_node` and confirm the binding name — the design doc records it at `:4715` as `OwnerAddr(loaded.state.owner_id)`. If the resolver handle is shadowed locally, use the canonical `reachability_resolver` binding from `:4308`.)

- [ ] **Step 3: Run the fleet-net + reachability + start-node smoke tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net) + test(reachability) + test(start_node)'`
Expected: PASS (no behavioral regressions; this hook is additive).

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/lib.rs && git commit -m "feat(zeb-510): boot-replay persisted fleet siblings into reachability resolver"
```

---

### Task 4: Live-merge hook — feed freshly-merged siblings into the resolver mid-session

**Files:**
- Modify: `src-tauri/src/lib.rs` (the fleet-net snapshot-refresh task, capture block `:9057-9069` and the nudge-handler body `:9085`)

**Interfaces:**
- Consumes: `crate::fleet_net::sibling_rows` + `crate::fleet_net::sibling_reachability_payload` (Task 2), `ReachabilitySource::FleetSibling` (Task 1). In-scope in the capture block: `fleet_net_doc` (as `task_doc`), `device_id` (as `task_device_id`, `:9064`), `self_owner` (as `task_self_owner`, `:9063`), and `reachability_resolver` (`:4308`, still live — cloned into state at `:11049`, so available to clone here). Inside the task, the merged doc snapshot is `new_doc` (`:9085`).
- Produces: on every applied remote fleet-net merge, the merged non-self sibling rows are re-fed into the resolver. Re-feed-all is idempotent — `update_with_source` LWW-rejects any row whose `seen_at` HLC is not strictly newer than the stored one.

**Context:** The design doc §4 named `event_loop.rs` as the live-merge site. That is incorrect: `merge_from` runs inside `FleetSyncEngine`, and the event-loop zenoh bridge has no resolver/doc/`device_id` in scope. The real post-merge hook is the engine's `on_applied` closure (`lib.rs:5577`), which fires a nudge consumed by THIS snapshot-refresh task — the task already re-reads the whole doc (`new_doc`) on every applied merge, so it is the correct, in-scope home for the resolver feed.

- [ ] **Step 1: Capture a resolver clone into the task**

In `src-tauri/src/lib.rs`, in the capture block (`:9058-9068`, the `let task_* = …;` lines just before `let mut nudge_rx = fleet_net_snap_nudge_rx;`), add:

```rust
                        // ZEB-510: feed freshly-merged sibling endpoints into
                        // the dial resolver from inside this refresh task (the
                        // engine's on_applied nudge fires here on every applied
                        // remote merge).
                        let task_resolver = reachability_resolver.clone();
```

- [ ] **Step 2: Feed merged siblings after each nudge**

Inside the `msg = nudge_rx.recv() => { … }` arm, immediately after the line that snapshots the merged doc — `let new_doc = { task_doc.lock().await.clone() };` (`:9085`) — insert:

```rust
                                        // ZEB-510: a sibling coming online or
                                        // changing endpoint mid-session must
                                        // reach the dialer. Re-feed-all is
                                        // idempotent (LWW rejects rows whose
                                        // seen_at HLC is not strictly newer),
                                        // and the self row is excluded.
                                        for (_dev_id, row) in
                                            crate::fleet_net::sibling_rows(&new_doc, &task_device_id)
                                        {
                                            task_resolver.update_with_source(
                                                task_self_owner,
                                                crate::fleet_net::sibling_reachability_payload(&row),
                                                row.seen_at.clone(),
                                                crate::reachability_resolver::ReachabilitySource::FleetSibling,
                                            );
                                        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --features test-fixtures`
Expected: compiles clean. (If `reachability_resolver` was moved before this block, clone it earlier or reuse an existing in-scope clone — confirm via `grep -n "reachability_resolver" src/lib.rs` that no move precedes `:9057`.)

- [ ] **Step 4: Run lib + clippy**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net) + test(reachability)' && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/lib.rs && git commit -m "feat(zeb-510): live-feed merged fleet siblings into reachability resolver"
```

---

### Task 5: Promote the s7 `HELD` boundary to a hard assert (acceptance / step-1-vs-step-2 gate)

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (`s7_butler_deposit_recover`, the `HELD` soft-fallback `:1817-1865`)

**Interfaces:**
- Consumes: `get_butler_held`, `poll_until` from `e2e_harness::driver` (unchanged). The `REACHABILITY` barrier (`:1796-1801`) and Boundary-0b are already hard asserts.
- Produces: a `HELD` hard assertion that fails loudly if P's published butler-set still does not carry B2's real endpoint co-located.

**IMPORTANT — this task is the empirical gate, not just a code change:**
Promoting `HELD` makes the test PASS only if Tasks 1–4 actually let P learn B2's endpoint and route the deposit to B2 co-located. There are two legitimate outcomes, and the second is NOT a task failure:
1. **s7 goes green** → step 1 is sufficient. **Stop here** — step 2 (SAS seed) is not needed.
2. **The promoted `HELD` assert times out** → co-located first-contact does not converge fleet-net on step 1 alone. This is the **step-1-vs-step-2 gate firing** (design doc §"The step-1-vs-step-2 gate"). **Do NOT weaken or revert the assert.** Halt, record the outcome, and surface it to the controller/Jake for the step-2 (SAS seed) decision that Jake explicitly wants to make from this empirical result.

Leave the downstream `RECV` (`:1872-1904`) and `CLEARED` (`:1910-1948`) soft-fallbacks AS-IS for now and add a one-line residual note — the design doc permits promoting "at least `HELD`" and noting the residual if the recover half still needs work.

- [ ] **Step 1: Replace the `HELD` soft-fallback with a hard assert**

In `e2e-harness/tests/e2e_two_node.rs`, the current block (`:1817-1865`) polls for the held entry and, on `Err`, prints an `S7 FINDING (ZEB-510)` and `run.mark_success(); … return;`. Replace the `let held = poll_until(…).await;` + `let held_entry = match held { … }` region with a hard `.expect(...)`:

```rust
    // ZEB-510 (step 1): P now seeds sibling B2's iroh endpoint into its dial
    // resolver from the persisted FleetNetDoc, so P's published butler-set
    // reaches B2 and A's deposit lands on B2. HARD ASSERT — if this times out,
    // step 1 alone did not converge fleet-net co-located and the step-2 SAS
    // seed is required (do not weaken this assert; that is the gate signal).
    let held_entry = poll_until(Duration::from_secs(120), || async {
        let entries = get_butler_held(&b2).await?;
        Ok(entries
            .into_iter()
            .find(|e| e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())))
    })
    .await
    .expect(
        "S7 HELD (ZEB-510): B2 must hold A's deposit for P within 120s co-located — \
         P should have learned B2's iroh endpoint via the FleetNetDoc→resolver wiring",
    );
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
    // RESIDUAL (ZEB-510 step 1): RECV/CLEARED below remain soft-characterize
    // fallbacks — the recover half (B2->P handoff) is validated cross-WAN by
    // Scenario D3; promote them in a follow-up if they pass co-located.
```

Keep the existing `RECV` and `CLEARED` blocks (`:1872` onward) unchanged.

- [ ] **Step 2: Confirm the harness compiles**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/e2e-harness && cargo nextest run --features e2e --no-run`
Expected: compiles clean (no run yet).

- [ ] **Step 3: Run s7 (the acceptance / gate run)**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/e2e-harness && cargo nextest run --features e2e --test-threads 1 -E 'test(s7_butler_deposit_recover)'`
Expected — one of two outcomes:
- **PASS** → step 1 is sufficient; record it and proceed to the final review. Step 2 is not built.
- **FAIL at `S7 HELD`** (120s timeout) → the step-1-vs-step-2 gate has fired. Record the exact failure, do NOT weaken the assert, and STOP for the controller to surface the step-2 decision. (Any OTHER failure — compile, panic, a different boundary — is a real defect to fix, not the gate.)

> This is a live multi-node test (spawns A + P + B2, ~minutes). It is NOT part of the `src-tauri` gate and runs only under the `e2e` feature. If the build of `harmony-app` is stale, the harness rebuilds it first (see `e2e-harness/README.md`).

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add e2e-harness/tests/e2e_two_node.rs && git commit -m "test(zeb-510): promote s7 HELD boundary to a hard assert"
```

---

## Final gate (after all tasks)

Run the full CI-parity sweep before opening the PR:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri \
  && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

Expected: fmt clean, clippy clean, all tests pass. Then run the s7 acceptance per Task 5 Step 3 and record the gate outcome.

## The step-1-vs-step-2 gate (summary)

Step 1 relies on P and B2 having converged fleet-net **at least once** (so P's `FleetNetDoc` holds B2's real self-authored row), then re-dialing B2 from the persisted row. Co-located multicast peering *should* establish that first convergence. Task 5's s7 result decides:
- **s7 green** → done; step 2 (SAS first-contact seed) is NOT built.
- **s7 `HELD` still times out** → build step 2 per the design doc §"Step 2 — SAS first-contact seed (GATED)". That is a **new plan**, gated on Jake's decision — Jake noted he suspects step 2 will be needed but wants to see this empirical result first.

## Out of scope (do NOT build in this plan)

- Step 2 SAS endpoint-exchange + `fleet_peer_seed.cbor` store (gated; separate plan).
- Cross-WAN pkarr rendezvous fallback (~ZEB-513).
- Any owner-state, community-membership, or DM-signing change.
