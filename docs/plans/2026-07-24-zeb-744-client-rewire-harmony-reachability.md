# ZEB-744 PR 2 — Client rewire onto `harmony-reachability` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewire harmony-client onto the merged core crate `harmony-reachability` — pin-bump the lockstep rev, turn `reachability_record.rs` into a thin shim re-exporting the core record, and slim `reachability_resolver.rs` onto the core kernel (`MultiDeviceMap` + `lww_newer` + `ReachabilityFallback`) — with zero wire-format change and the full resolver + wire-fixture suites as the regression gate.

**Architecture:** The core crate already carries the record bytes + a generic multi-device LWW kernel (merged in harmony#292, squash `3745744`). This PR repoints the client at it. The record's ~40 consumers all reference it by module path, so a same-named re-export shim keeps every call site unchanged; the client-only helpers (inner-sign/verify, butler accessors, freshness check, `CanonicalPayload` impls) stay in that shim operating on the core-typed record via its public fields. `ReachabilityResolver` stays client-side; only its backing map type, its one LWW call site, and the `ReachabilityFallback` trait source change — all its documented ZEB-620/621/622/627/643/704 concurrency policy is untouched, so its 48-test suite is a black-box gate.

**Tech Stack:** Rust, Cargo git-rev pins, `cargo nextest`, `async-trait`, ciborium CBOR.

## Global Constraints

- **Package name is `harmony-app`** (`src-tauri/Cargo.toml:2`). In-crate paths use `crate::…`; integration tests use `harmony_app::…`. There is no `harmony_client` crate.
- **Merged core rev to pin: `374574499d1873f3d069af610d5bc789c78c1c36`** (harmony#292 squash `3745744` on core `main`). The 12 lockstep crates bump to it; **`harmony-pkarr` stays at its separate pin `80f6d80858f283d4f4094d483d548e50b8c4e107`** (lines 145 + 262) and MUST NOT be touched.
- **Zero wire-format change.** `canonical_cbor_encode(ReachabilityAnnouncePayload)` must stay byte-identical to the pinned golden hex. The wire-format fixture tests (`binary(wire_format_tests)`) and the in-module golden tests are the non-negotiable acceptance gate. Do NOT regenerate any `EXPECTED_*_HEX` constant.
- **Byte-identity of the delegate type:** core's `DelegateEndpoint` has the same field order + `#[serde(rename)]` keys (`d`/`ep`/`vk`/`hr`/`pn`) + bstr encoding as the old client `ButlerSetEntry`. Keep the client name working via `DelegateEndpoint as ButlerSetEntry` so the 8 construction sites + fixtures stay unchanged and byte-stable.
- **Core crate API (already published, verbatim):**
  - `harmony_reachability::{ReachabilityAnnouncePayload, DelegateEndpoint}` — record structs; all fields `pub` (`iroh_node_id:[u8;32]`, `home_relay_url:String`, `direct_addresses:Vec<SocketAddr>`, `announced_at_ms:u64`, `identity_signature:[u8;64]`, `butler_set:Vec<DelegateEndpoint>`, `bs_at:u64`; delegate: `device_id:[u8;16]`, `iroh_endpoint_id:[u8;32]`, `device_ed25519_verify:[u8;32]`, `home_relay:String`, `pinned:bool`).
  - `harmony_reachability::ReachabilityRecord` — `fn node_id(&self)->[u8;32]; fn announced_at_ms(&self)->u64;` (core impls it for `ReachabilityAnnouncePayload`).
  - `harmony_reachability::lww_newer<C: Ord, R: ReachabilityRecord>(prev_clock:&C, prev_rec:&R, next_clock:&C, next_rec:&R) -> bool`.
  - `harmony_reachability::MultiDeviceMap<Owner: Ord+Copy, V>` — `new()`, `insert(key:(Owner,[u8;32]), v)->Option<V>`, `get(&(Owner,[u8;32]))`, `get_mut(&…)`, `entry((Owner,[u8;32]))->btree_map::Entry`, `remove(&(Owner,[u8;32]))`, `iter()`, `is_empty()`, `len()`, `range_owner(&Owner)->impl Iterator<Item=(&(Owner,[u8;32]),&V)>`, `owner_keys(&Owner)->Vec<(Owner,[u8;32])>`, `find_by_node_id(&[u8;32])->impl Iterator`. Derives `Debug, Clone, Default`.
  - `harmony_reachability::ReachabilityFallback<Owner>: Send+Sync` — `#[async_trait] async fn resolve(&self, owner:&Owner)->Vec<ReachabilityAnnouncePayload>;`.
  - Also re-exported at crate root: `canonical_cbor_encode`, `canonical_cbor_decode`, `CborError`. NOT at root: `canonical_payload_bytes` (→ `::record::`), serde helpers (→ `::canonical::`).
- **Test cost:** any lib change relinks ~97 integration binaries (~50 min full). Per-task gates use `scripts/test-select` + the targeted binaries named in each task. **Because Task 1 changes `Cargo.toml`/`Cargo.lock` (a dependency-graph change), `test-select`'s module-mapping guard trips** — the per-task gates therefore run `scripts/test-select --context task --force` (the guard's sanctioned proceed-anyway), and the **authoritative dependency-change coverage is the full CI-parity sweep in Task 4** (`cargo nextest run --locked --workspace --all-targets`), which must be green before merge. All test commands run from `src-tauri/` with `--locked --features test-fixtures`.
- **Gates (CI parity):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

---

## File Structure

- `src-tauri/Cargo.toml` — bump 12 lockstep rev strings + add `harmony-reachability` dep (Task 1). One responsibility: the dependency pin.
- `src-tauri/src/reachability_record.rs` — rewritten from a ~795-line owner of the record into a thin shim: re-export the core record types; keep the client-only inner-sign/verify + butler accessors + freshness check + `CanonicalPayload` impls operating on the core type; keep all client-side tests (Task 2).
- `src-tauri/src/reachability_resolver.rs` — slim onto the kernel: `BTreeMap`→`MultiDeviceMap`, one `lww_newer` delegate, `ReachabilityFallback` trait sourced from core (Task 3). All policy unchanged.
- `src-tauri/src/pkarr_resolver_adapter.rs`, `src-tauri/src/reconnect_supervisor.rs` — `ReachabilityFallback` impl sites gain the `<OwnerAddr>` type arg (Task 3).
- Consumers (~40 files) and the wire-format fixtures (`tests/wire_format/reachability_announce_fixtures.rs`, `…/pkarr_routing_record_fixtures.rs`) — **unchanged** by design; verified green in Task 4.

---

### Task 1: Bump lockstep rev + add `harmony-reachability` dependency

**Files:**
- Modify: `src-tauri/Cargo.toml` (12 lockstep lines 120–159 + one new dep line in `[dependencies]`)
- Modify: `src-tauri/Cargo.lock` (regenerated by cargo)

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `harmony-reachability` resolvable at rev `374574499d…` for Tasks 2–3; the 12 existing lockstep crates on the same rev.

- [ ] **Step 1: Edit the 12 lockstep rev strings.** In `src-tauri/Cargo.toml`, on each of these 12 lines, replace `rev = "cb05de923f62cfbdb9dc3f59bbe25d2f269e857d"` with `rev = "374574499d1873f3d069af610d5bc789c78c1c36"` (leave every other field on the line — e.g. `features = ["recovery"]` on `harmony-owner` — intact): `harmony-runtime` (120), `harmony-identity` (121), `harmony-content` (122), `harmony-compute` (123), `harmony-telemetry` (124), `harmony-mailbox` (125), `harmony-owner` (126), `harmony-crypto` (131), `harmony-crdt-sync` (136), `harmony-tunnel` (150), `harmony-iroh` (158), `harmony-tunnel-iroh` (159).

- [ ] **Step 2: DO NOT touch the two `harmony-pkarr` lines** (145 in `[dependencies]`, 262 in `[dev-dependencies]`) — they stay at `rev = "80f6d80858f283d4f4094d483d548e50b8c4e107"`. This is a deliberate separate pin (comment at lines 137–144).

- [ ] **Step 3: Add the new dependency.** Insert into `[dependencies]`, adjacent to the other lockstep crates (e.g. right after the `harmony-tunnel-iroh` line 159):

```toml
harmony-reachability = { git = "https://github.com/zeblithic/harmony.git", rev = "374574499d1873f3d069af610d5bc789c78c1c36" }
```

- [ ] **Step 4: Regenerate the lockfile + verify it resolves and compiles.**

Run (from `src-tauri/`) plain `cargo check` with **no** `--locked` flag — cargo has no `--locked=false` form (it rejects the argument before resolution); omitting `--locked` is what lets cargo re-resolve and rewrite `Cargo.lock` for the new rev: `cargo check -p harmony-app --features test-fixtures 2>&1 | tail -20`
Expected: resolves the new git rev, updates `Cargo.lock`, compiles clean (the 12-crate bump is a no-op — `3745744` only *added* the new crate on top of `cb05de9`, touching none of their sources). `harmony-reachability` is added-but-not-yet-used; if (and only if) the build errors with `unused_crate_dependencies` for `harmony-reachability`, add `use harmony_reachability as _;` near the top of `src-tauri/src/lib.rs` and note it — Task 2 replaces it with real use. (Default lints do not enable that check; expect no such error.)

- [ ] **Step 5: Confirm `--locked` is now satisfied.**

Run: `cargo check --locked -p harmony-app --features test-fixtures 2>&1 | tail -5`
Expected: `Finished` with no re-resolution (proves `Cargo.lock` is committed-consistent for CI).

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "reachability: bump lockstep rev to harmony#292 + add harmony-reachability dep (ZEB-744)"
```

---

### Task 2: Rewrite `reachability_record.rs` as a thin shim over the core record

**Files:**
- Modify (large rewrite): `src-tauri/src/reachability_record.rs`
- Test: the file's own `#[cfg(test)] mod tests` (kept in place) + `src-tauri/tests/wire_format/{reachability_announce_fixtures.rs, pkarr_routing_record_fixtures.rs}` (unchanged; run as the byte-identity gate)

**Interfaces:**
- Consumes: `harmony_reachability::{ReachabilityAnnouncePayload, DelegateEndpoint}` (Task 1 dep).
- Produces: `crate::reachability_record::{ReachabilityAnnouncePayload, ButlerSetEntry, canonical_payload_bytes, REACHABILITY_RECORD_TTL_MS, fresh_butler_set, durable_butler_set, inner_signed_bytes, build_signed_payload, build_signed_payload_with_key, verify_inner_signature, reachability_freshness_check, InnerSigError}` — the **same public surface as today**, so no consumer import changes. `ButlerSetEntry` is now an alias of core `DelegateEndpoint`. The record type is now `harmony_reachability::ReachabilityAnnouncePayload`.

- [ ] **Step 1: Run the byte-identity gate BEFORE the change to capture the baseline.**

Run (from `src-tauri/`): `cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests) and test(reachability)' -E 'binary(wire_format_tests) and test(routing_blob)'`
Expected: PASS (current tree). Record the passing test names — these must still pass identically after the rewrite. (This is the regression baseline, not a new test.)

- [ ] **Step 2: Replace the two struct definitions + `is_zero_u64` with core re-exports.**

Delete from `reachability_record.rs`: the `pub struct ButlerSetEntry {…}` (lines ~31–68), the private `fn is_zero_u64` (~72–74), and the `pub struct ReachabilityAnnouncePayload {…}` (~80–133). In their place add:

```rust
// The record shape + delegate type + canonical CBOR now live in the core crate
// (harmony-reachability, ZEB-744 PR 1). This module keeps only the client-side
// inner-signature scheme, butler-set accessors, and freshness policy, operating
// on the core-owned record through its public fields.
pub use harmony_reachability::{DelegateEndpoint as ButlerSetEntry, ReachabilityAnnouncePayload};
```

- [ ] **Step 3: Keep the client `CanonicalPayload` impls + `canonical_payload_bytes` (they cannot move to core).**

Leave the two impl lines (~176–177) and `canonical_payload_bytes` (~180–182) in place — `CanonicalPayload`/`CanonicalPayloadSealed` are client traits (in `owner_state_crypto`), and `impl ClientTrait for CoreType` is legal (orphan rule; the trait is local, and the sealed super-trait is impl'd inside its defining crate). Confirm the imports at the top still resolve `CanonicalPayload`, `CanonicalPayloadSealed`, `canonical_cbor_encode`, `CryptoError` from `crate::owner_state_crypto`, and `serialize_bytes_as_bstr`, `Hlc`, `OwnerAddr` from `crate::owner_state_types`. Remove only the now-unused import of `deserialize_bytes_from_bstr` if it is no longer referenced (the structs that used it are gone); keep `serialize_bytes_as_bstr` (still used by `InnerSigInput`).

- [ ] **Step 4: Keep every category-B client fn unchanged.** These already operate on the record via public field access / struct-literal construction, which works against the core type (all fields are `pub` with identical names): `REACHABILITY_RECORD_TTL_MS`, `fresh_butler_set`, `durable_butler_set`, `inner_signed_bytes` (+ its local `InnerSigInput`), `build_signed_payload`, `build_signed_payload_with_key`, `verify_inner_signature`, `reachability_freshness_check`, `InnerSigError`. No signature or body change — only verify they still compile against the re-exported type.

- [ ] **Step 5: Keep all in-module tests.** The golden tests (`routing_blob_without_butler_set_is_wire_identical_to_legacy` with `EXPECTED_LEGACY_HEX`, `legacy_routing_blob_decodes_with_empty_butler_set`, `routing_blob_with_butler_set_round_trips`, `encoded_size_with_two_entries_under_bep44_budget`, `roundtrip_cbor`, `payload_keys_are_2_chars`), the butler tests (`butler_set_capped_at_two`, `stale_bs_at_is_filtered_by_reader`), the inner-sig tests (`inner_sig_roundtrip_with_real_identity`, `build_signed_payload_with_key_verifies_and_rejects_mutation`, `inner_sig_rejects_tampered_node_id`, `inner_sig_covers_butler_set_and_rejects_tamper`), and `reachability_freshness_check_bounds_announced_at` all stay. They now construct the core-typed record (identical field names) — no change needed beyond compiling.

- [ ] **Step 6: Build + run the module tests.**

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(reachability_record)'`
Expected: all module tests PASS, including `routing_blob_without_butler_set_is_wire_identical_to_legacy` (the byte-identity lock holds against the core-typed record).

- [ ] **Step 7: Run the wire-format fixture gate (cross-repo byte-identity proof).**

Run: `cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests)'`
Expected: PASS — specifically `reachability_announce_payload_wire_bytes_pinned`, `reachability_announce_payload_with_butler_set_wire_bytes_pinned`, `signed_event_reachability_announce_wire_bytes_pinned`, `routing_blob_canonical_cbor_pinned`. These files import `harmony_app::reachability_record::{…, ButlerSetEntry, …}` and are UNCHANGED — the alias re-export keeps them compiling and the bytes identical.

- [ ] **Step 8 (hardening, per spec §8): add an inner-signature preimage golden vector.**

The inner-sig preimage crosses the repo boundary (record fields now core-typed, `ac`/`hl` client-side), so lock it with a byte vector. Add to `mod tests`:

```rust
/// Byte-lock for the inner-signature preimage (`inner_signed_bytes`), which now
/// spans the repo boundary: record fields come from the core crate, `actor`/`hlc`
/// from the client. DO NOT REGENERATE — regenerating would mask a preimage drift.
#[test]
fn inner_signed_bytes_preimage_is_wire_pinned() {
    let hlc = fixture_hlc();
    let actor = OwnerAddr([0x11; 16]);
    let p = fixture_payload();
    let bytes = inner_signed_bytes(
        &p.iroh_node_id,
        &p.home_relay_url,
        &p.direct_addresses,
        p.announced_at_ms,
        &actor,
        &hlc,
        &p.butler_set,
        p.bs_at,
    )
    .expect("preimage");
    const EXPECTED_PREIMAGE_HEX: &str = "__GENERATE_AT_IMPL_TIME__";
    assert_eq!(hex::encode(&bytes), EXPECTED_PREIMAGE_HEX, "inner-sig preimage drifted");
}
```

Generate the constant ONCE: run the test, read the `left:` value from the assertion failure, paste it into `EXPECTED_PREIMAGE_HEX`, re-run to green. Verify `fixture_hlc()` / `fixture_payload()` exist in the module (they do, ~428/436); if `OwnerAddr`'s constructor differs, match the existing test style in the file.

- [ ] **Step 9: Per-task regression selection + fmt/clippy.**

Run:
```bash
scripts/test-select --context task --force   # --force: Task 1's manifest change trips the dep-graph guard; Task 4's full --workspace sweep is the authoritative dep-change coverage
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
```
Expected: selection green (paste the `round=… bucket=…` line into the report), fmt clean, clippy clean. (Clippy over `--all-targets` here compiles the consumers, catching any that the re-export failed to satisfy.)

- [ ] **Step 10: Commit.**

```bash
git add src-tauri/src/reachability_record.rs
git commit -m "reachability_record: shim over harmony-reachability core record; keep inner-sig + butler + freshness client-side (ZEB-744)"
```

---

### Task 3: Slim `reachability_resolver.rs` onto the core kernel

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs`
- Modify: `src-tauri/src/pkarr_resolver_adapter.rs` (fallback impl site)
- Modify: `src-tauri/src/reconnect_supervisor.rs` (test-stub fallback impl site)
- Test: the resolver's `mod tests` (32) + `mod fallback_tests` (16) — the 48-test regression gate

**Interfaces:**
- Consumes: `harmony_reachability::{MultiDeviceMap, lww_newer, ReachabilityFallback}`; `crate::reachability_record::ReachabilityAnnouncePayload` (now the core type via Task 2 shim — required so it impls `ReachabilityRecord`).
- Produces: unchanged public resolver API (`update`, `update_with_source`, `resolve*`, `list_*`, `remove_owner`, `maybe_refresh_stale`, `set_fallback_source`, `seed_from_pkarr`). `crate::reachability_resolver::ReachabilityFallback` still resolves (re-exported from core).

- [ ] **Step 1: Add the kernel imports.** Near the existing imports (top of `reachability_resolver.rs`), add:

```rust
use harmony_reachability::{lww_newer as core_lww_newer, MultiDeviceMap};
```

Keep `use crate::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr};` (line ~20) and `use crate::reachability_record::ReachabilityAnnouncePayload;` (line ~22) as-is.

- [ ] **Step 2: Swap the backing map type + construction.**
  - Line ~239: `inner: Arc<RwLock<BTreeMap<ResolverKey, ResolverSlots>>>,` → `inner: Arc<RwLock<MultiDeviceMap<OwnerAddr, ResolverSlots>>>,`
  - Line ~297 (`Default`): `inner: Arc::new(RwLock::new(BTreeMap::new())),` → `inner: Arc::new(RwLock::new(MultiDeviceMap::new())),`
  - Keep the `type ResolverKey = (OwnerAddr, [u8; 32]);` alias (line ~76) — `MultiDeviceMap`'s tuple key is the same shape, and the alias still names the entry key for `entry`/`remove` sites.
  - Remove the now-unused `BTreeMap` import if nothing else in the file uses it (grep first; `refresh_cooldowns` or other fields may still use `BTreeMap` — if so, keep the import).

- [ ] **Step 3: Repoint the three owner-prefix range scans to `range_owner`.** Each currently reads `map.range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))`; replace the `.range(...)` call with `.range_owner(actor)` (note: `range_owner` takes `&Owner`, and `actor` is already `&OwnerAddr` at these sites — pass `actor` directly; if a site holds an owned `OwnerAddr`, pass `&owner`):
  - `:495` in `resolve` (chained `.filter_map(|(_, v)| v.durable_preferred()…)` stays).
  - `:703` in `remove_owner` (chained `.map(|(k, _)| *k)` stays).
  - `:837` in `resolve_with_source` (chained `.filter_map(…e.source…)` stays).

- [ ] **Step 4: Repoint the reverse-by-node-id scan in `freshest_across_owners`.**
  - Change the param type (line ~218): `map: &'a BTreeMap<ResolverKey, ResolverSlots>` → `map: &'a MultiDeviceMap<OwnerAddr, ResolverSlots>`.
  - Change the body (lines ~221–223): replace `map.iter().filter(|((_, key_node_id), _)| key_node_id == node_id_bytes)` with `map.find_by_node_id(node_id_bytes)`. The trailing `.filter_map(|((owner, _), v)| v.freshest().map(|e| (*owner, e)))` and the `.reduce(...)` arbitration stay unchanged.

- [ ] **Step 5: Leave `entry`/`remove`/`iter` sites as-is (tuple-keyed, API-compatible).** `map.entry(key).or_default()` (:436), `map.remove(k)` (:707), and the three `map.iter()` scans (:502, :517, :542) call identically-named `MultiDeviceMap` methods with the same tuple key/args — no change. Verify they compile (Step 9).

- [ ] **Step 6: Replace the local `lww_newer` body with a delegate to the core kernel.** Replace the whole fn (lines ~919–943) with:

```rust
/// Same-source LWW: is `next` strictly newer than `prev`? Delegates to the core
/// kernel comparator (ZEB-744), passing the HLC as its `(wall_ms, logical,
/// device_id)` `Ord` tuple — harmony's `Hlc` deliberately does not derive `Ord`
/// (canonical-CBOR keying constraint), so the tuple is the clock the kernel orders.
/// The payload supplies `announced_at_ms` + `node_id` tie-breaks via the core
/// `ReachabilityRecord` impl. Full equality → `false` (byte-identical replay is a no-op).
fn lww_newer(prev: &ResolverEntry, next: &ResolverEntry) -> bool {
    let prev_clock = (prev.hlc.wall_ms, prev.hlc.logical, prev.hlc.device_id.as_str());
    let next_clock = (next.hlc.wall_ms, next.hlc.logical, next.hlc.device_id.as_str());
    core_lww_newer(&prev_clock, &prev.payload, &next_clock, &next.payload)
}
```

The single call site (`:456` in `update_with_source`, `lww_newer(prev, &next)`) is unchanged. Semantics are identical to the old body: primary `(wall_ms,logical,device_id)` tuple order → `announced_at_ms` tie-break → `iroh_node_id` tie-break → equality false.

- [ ] **Step 7: Source the `ReachabilityFallback` trait from core.**
  - In `reachability_resolver.rs`, delete the local trait def (lines ~69–72) and replace with a re-export so existing `crate::reachability_resolver::ReachabilityFallback` paths keep resolving:

    ```rust
    pub use harmony_reachability::ReachabilityFallback;
    ```
  - Field (line ~243): `fallback_source: Arc<RwLock<Option<Arc<dyn ReachabilityFallback>>>>` → `…Arc<dyn ReachabilityFallback<OwnerAddr>>…` (add `<OwnerAddr>` inside the `dyn`).
  - `set_fallback_source` (line ~728): param `fb: Arc<dyn ReachabilityFallback>` → `fb: Arc<dyn ReachabilityFallback<OwnerAddr>>`.
  - The four in-file test stubs — `StubFallback` (:1803), `DurableInjectingFallback` (:1914), `CountingFallback` (:2021), `InFlightFallback` (:2200): change each `impl ReachabilityFallback for X` → `impl ReachabilityFallback<OwnerAddr> for X` (keep the `#[async_trait]` and the `async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload>` body — param name is free; return type resolves to the core record via the shim).

- [ ] **Step 8: Update the two out-of-file fallback impl sites.**
  - `src-tauri/src/pkarr_resolver_adapter.rs`: keep `use crate::reachability_resolver::ReachabilityFallback;` (line ~16; still valid via re-export). Change `impl ReachabilityFallback for PkarrResolverAdapter` (line ~71) → `impl ReachabilityFallback<OwnerAddr> for PkarrResolverAdapter`. Ensure `OwnerAddr` is in scope (add `use crate::owner_state_types::OwnerAddr;` if not already imported).
  - `src-tauri/src/reconnect_supervisor.rs`: the test-stub `CountingFallback` (line ~913) — same `impl … <OwnerAddr> …` change + `OwnerAddr` in scope.

- [ ] **Step 9: Compile the touched crate.**

Run: `cargo check --locked -p harmony-app --features test-fixtures 2>&1 | tail -20`
Expected: clean. If a `dyn ReachabilityFallback` site was missed it errors here ("wrong number of type arguments"); if the record type isn't the core one, `lww_newer`'s `ReachabilityRecord` bound fails — both are localized, fixable errors.

- [ ] **Step 10: Run the full resolver regression suite (the gate).**

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'binary(harmony-app) and test(reachability_resolver)'`
(If that filter under-selects, use `-E 'test(reachability_resolver::)'` or run the module by path.) Expected: **all 48 tests PASS** — `mod tests` (32: LWW per-device, dual-slot cross-source, generation/TOCTOU, multi-device coexistence, supervisor-kick gate, `reverse_lookup_*_zeb704`, `remove_owner_*`) and `mod fallback_tests` (16: async fallback/cache, `seed_from_pkarr`, cooldown/stale-refresh ZEB-621, future-skew, fleet-sibling ZEB-510/702). Any failure here is a real regression in the map/comparator swap — do not proceed.

- [ ] **Step 11: Per-task selection + fmt/clippy.**

Run:
```bash
scripts/test-select --context task --force   # --force: Task 1's manifest change trips the dep-graph guard; Task 4's full --workspace sweep is the authoritative dep-change coverage
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
```
Expected: green (paste the `round=… bucket=…` line into the report); clippy `--all-targets` compiles `pkarr_resolver_adapter` + `reconnect_supervisor` + the resolver test stubs, catching any missed impl-site arg.

- [ ] **Step 12: Commit.**

```bash
git add src-tauri/src/reachability_resolver.rs src-tauri/src/pkarr_resolver_adapter.rs src-tauri/src/reconnect_supervisor.rs
git commit -m "reachability_resolver: rebuild on harmony-reachability kernel (MultiDeviceMap + core lww_newer + core ReachabilityFallback); policy unchanged (ZEB-744)"
```

---

### Task 4: CI-parity full-workspace sweep

**Files:**
- No source changes (verification only). If this task surfaces a broken consumer, fix it minimally here and note it in the report.

**Interfaces:**
- Consumes: the merged state of Tasks 1–3.
- Produces: a green CI-parity signal proving no consumer (of the ~40 record consumers or the fallback trait) broke.

- [ ] **Step 1: Full fmt + clippy.**

Run (from `src-tauri/`):
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -25
```
Expected: both clean.

- [ ] **Step 2: Full CI-parity test sweep.** (This is the one `--full` run; budget ~50 min for the relink + execution — supervise with a wall-clock net.)

Run: `cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -40`
Expected: all pass. The wire-format fixtures + resolver suite + every integration test that constructs `ReachabilityAnnouncePayload`/`ButlerSetEntry` (`pkarr_community_fallback`, `iroh_zenoh_registration`, `community_reachability_two_engine`, `introduction_broker_roundtrip`, `zeb_373_dynamic_dial`, the `zeb376_intro_fixtures`, etc.) compile against the re-exported types and pass unchanged.

- [ ] **Step 3: Frontend gate (unaffected, but CI runs it — confirm no accidental breakage).**

Run (from repo root): `npx tsc --noEmit && npx vitest run 2>&1 | tail -15`
Expected: green (this PR is Rust-only; this is a sanity check, not a change site).

- [ ] **Step 4: Commit any fix (only if Step 1–2 surfaced a consumer break).** If everything was green, there is nothing to commit — proceed to the whole-branch review. If a consumer needed a change, commit it alone:

```bash
git add <the-one-file>
git commit -m "reachability: repoint <file> onto harmony-reachability types (ZEB-744)"
```

---

## Self-Review

**Spec coverage:** §4 record byte-preserving move → Task 2 (shim + golden gate). §5 resolver kernel swap (map + `lww_newer` + fallback, policy stays) → Task 3. §6 dep edges (no pkarr/iroh, rides lockstep) → Task 1. §7 sequencing (bump rev → delete/shim record → slim resolver → fixtures green) → Tasks 1–4. §8 byte-preservation gate + inner-sig preimage hardening → Task 2 Steps 6–8. §10 test strategy (scoped per-task, full final) → Task test steps + Task 4.

**Placeholder scan:** the only intentional generate-at-impl-time value is `EXPECTED_PREIMAGE_HEX` in Task 2 Step 8, with explicit bootstrap instructions (run → capture `left:` → pin → DO NOT REGENERATE). No other TBD/TODO.

**Type consistency:** `MultiDeviceMap<OwnerAddr, ResolverSlots>` (Task 3) matches `ResolverKey = (OwnerAddr,[u8;32])` and `OwnerAddr: Copy+Ord` (verified in source). `lww_newer` clock `C = (u64,u32,&str)` is `Ord`; `R = ReachabilityAnnouncePayload` impls core `ReachabilityRecord`. `ReachabilityFallback<OwnerAddr>` applied uniformly at the field, setter, and all 6 impl sites. `DelegateEndpoint as ButlerSetEntry` keeps the 8 construction sites + fixtures byte-stable. Record public fields (`iroh_node_id`, `home_relay_url`, `direct_addresses`, `announced_at_ms`, `identity_signature`, `butler_set`, `bs_at`) match the client fns' field access exactly.

**Ordering:** strictly linear 1→2→3→4 (Task 3 needs the core-typed record from Task 2 for the `ReachabilityRecord` bound; Task 2 needs the dep from Task 1). No parallelism — correct for subagent-driven execution.
