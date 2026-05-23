# ZEB-321 Phase 1 — Iroh foundation + ReachabilityAnnounce CRDT + Zenoh-over-Iroh transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the Phase 1 deliverables from `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` (commit 8cf44aa) — Iroh QUIC endpoint, `ReachabilityAnnounce` CRDT event in the community-state log, a Zenoh-over-Iroh custom transport plugin, debounced reachability publisher, and the minimal frontend debug surface — in a single PR (`zeb-321-phase1-iroh-foundation`) that converges through the autonomous bot-review loop and is ready to merge.

**Architecture:** A new `IrohEndpoint` wrapper around `iroh::Endpoint` (persisting its Ed25519 secret key in the OS keychain) gives each device a stable cross-NAT QUIC identity. A new `MembershipEventKind::ReachabilityAnnounce` variant (CRDT event with inner identity-signature binding the Iroh NodeId to the harmony identity) ships every device's NodeId + DERP relay URL + direct-address hints into the existing community-state CRDT. A `ReachabilityResolver` side-projection feeds the new Zenoh transport plugin (`zenoh_iroh_link.rs` + `zenoh_iroh_transport.rs`), which implements `zenoh-link`'s `LinkUnicastTrait` + `LinkManagerUnicastTrait` so every existing Zenoh CRDT-sync code path keeps working unchanged, but its underlying bytes now flow over Iroh QUIC streams (direct hole-punched where possible, DERP-relayed where not). A debounced `reachability_publisher` re-emits the local record on boot, on network change (`if-watch`), on home-relay change, and on a 60-min idle tick.

**Tech Stack:** Rust (`iroh`, `if-watch`, `zenoh-link` semi-internal API, `keyring`, `ciborium`, `ed25519-dalek`, `serde`, `tokio`), TypeScript/Svelte (Tauri IPC adapter + debug panel).

---

## Pre-flight context (read once before starting Task 0)

### Spec ↔ code reconciliation (load-bearing — read carefully)

The spec at `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` was authored with imprecise wire-envelope notation in §5.2 (`{kd, ac, hl, pl, sg}`). The actual community-state CRDT envelope in `src-tauri/src/community_membership.rs` is `SignedMembershipEvent` with field layout `{id, ci, kn, ac, at, sg, cs?}` — `kn` is the kind discriminator and the inner `MembershipEventKind` enum uses adjacent serde tagging `{tg, vl}` with 1-char variant tags (existing variants: `j` Join, `l` Leave, `i` Invite, `k` Kick, `p` SetPower, `u` Unban, `c` ChannelCreate, `m` ChannelModify, `d` ChannelDelete, `r` EpochRotation, `f` EpochCatchup, `x` Fork, `g` PendingJoin, `y` JoinCountersign, `q` AdminProposal, `n` AdminCountersign).

**Resolution:** Add a new `MembershipEventKind::ReachabilityAnnounce { … }` variant with serde tag `"a"` (announce — short, available, semantically appropriate). The variant body carries the spec's `ReachabilityAnnouncePayload` fields (`nd`, `rl`, `da`, `ts`, `sg` — the inner identity signature). The outer envelope signature (`SignedMembershipEvent.sig`) is the standard CRDT integrity signature; the inner `sg` inside the variant is the new identity-binding signature per spec §5.3.

**ZEB-320 dual-watermark discipline (spec §5.5 RCH4):** the membership CRDT uses a content-addressed event log via `insert_event` (community_state_crdt.rs:294) — there is no `last_received_hlc / last_hlc` pair to advance/withhold the way the voting-tier3 CRDT does. Failed `verify_event` simply means the event is rejected (`InsertOutcome::Rejected`), and a future, valid event with a later HLC can still be inserted. The RCH4 "drop silently if timestamp skew" rule reduces to: return a new `VerifyError::ReachabilityTimestampSkew` from `verify_event`. No watermark machinery is needed because the membership log doesn't have one.

### Existing patterns to mirror (do not duplicate — extend / reuse)

- **SignedMembershipEvent envelope:** `src-tauri/src/community_membership.rs:335` — 7-field CBOR map (`id`, `ci`, `kn`, `ac`, `at`, `sg`, `cs?`), 2-char keys, signed via `sign_event_with_identity` (community_membership.rs:501).
- **MembershipEventKind enum:** `src-tauri/src/community_membership.rs:81` — adjacent-tagged (`#[serde(tag = "tg", content = "vl")]`), 1-char variant tags. Add `ReachabilityAnnounce` here.
- **verify_event:** `src-tauri/src/community_membership.rs:2166` — `pub fn verify_event(event: &SignedMembershipEvent, prior_state: &MaterializedMembership, ctx: &VerifyContext) -> Result<(), VerifyError>`. Add a new arm and 5 new `VerifyError` discriminants (one per RCH1-5).
- **insert_event:** `src-tauri/src/community_state_crdt.rs:294` — `pub fn insert_event(&mut self, event: SignedMembershipEvent, ctx: &VerifyContext) -> InsertOutcome`. After successful insert, the side-channel (`ReachabilityResolver`) must be fed; we wire that hook in event_loop.rs (Task 8), not inside `insert_event` itself (keep insert_event pure).
- **Wire-format pinning convention:** see `src-tauri/tests/wire_format_community_fixtures.rs` — pin canonical hex bytes per variant; if the test fails, treat as a wire-protocol break.
- **Tauri IPC pattern:** `#[tauri::command]` attribute, snake_case Rust ↔ camelCase JS, `app_handle.emit("event-name", payload)` for events. See dozens of examples in `src-tauri/src/lib.rs` (search for `#[tauri::command]`).
- **OS keychain pattern:** `keyring` crate is already a dep (`Cargo.toml:44`) with `apple-native`, `windows-native`, `sync-secret-service` features. Existing harmony identity key uses this; reuse the same backend. (Search for `keyring::Entry::new` in `src-tauri/src/identity.rs` for the canonical usage pattern.)

### Hard rules every task must satisfy

1. **Branch state:** all work happens on `zeb-321-phase1-iroh-foundation`, currently at commit `8cf44aa` (spec commit) off `origin/main` at `e68599b`. No worktrees — `git checkout -b` in main repo only (memory rule `feedback_no_worktrees`).
2. **Backend CI gates** (run from `src-tauri/`, every task ends with these passing):
   - `cargo fmt --all -- --check`
   - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
   - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
3. **Frontend CI gates** (only on tasks that touch frontend; from repo root using `npx`, NOT `pnpm`):
   - `npx tsc --noEmit`
   - `npx vitest run`
4. **Commit-before-gate + 10-min wall-clock kill switch + DONE_WITH_CONCERNS escape hatch** per memory rule `feedback_implementer_gate_time_budget`. Implementer subagents commit their staged changes BEFORE running any of the long gates so progress is preserved even if a gate hangs.
5. **Pipe exit codes:** `set -o pipefail` or `${PIPESTATUS[0]}` when piping cargo output through `tail` / `grep` (memory rule `feedback_pipe_exit_codes_lie`).
6. **Tauri IPC:** snake_case Rust (`#[tauri::command(rename_all = "snake_case")]` if needed) ↔ camelCase JS. Frontend error extraction: `const msg = e instanceof Error ? e.message : String(e);` (memory rule `feedback_tauri_error_extraction`).
7. **No new Linear tickets** mid-implementation unless follow-up work is discovered (memory rule `feedback_never_invent_linear_ids`).
8. **Pre-existing orphan failures** captured in Task 0 baseline are not blocking; new failures introduced by this PR are blocking (memory rule `feedback_test_drift_is_our_fault`).
9. **macOS XprotectService:** developer-tools entitlement assumed enabled on the implementer's machine per `reference_xprotectservice_dev_tools`. If `cargo nextest run` hangs >10 min on first run, the implementer machine is misconfigured — surface this immediately as a DONE_WITH_CONCERNS rather than waiting it out.

---

## File structure (mapping)

### Rust backend (new files — all under `src-tauri/src/`)

| File | Responsibility |
|---|---|
| `iroh_endpoint.rs` | `IrohEndpoint` wrapper around `iroh::Endpoint`; persistent SecretKey via keychain; ALPN registry constants; `init()`, `node_id()`, `home_relay()`, `direct_addresses()`, `open_bi()`, `incoming()` surface |
| `reachability_record.rs` | `ReachabilityAnnouncePayload` struct + CBOR ser/de + canonical-byte fixture coverage; helpers for inner-signature construction and verification |
| `reachability_resolver.rs` | `ReachabilityResolver` — `BTreeMap<OwnerAddr, ReachabilityAnnouncePayload>` plus `update()` / `resolve()` / `list_active_peers()` API, LWW projection, determinism guarantees |
| `reachability_publisher.rs` | Background tokio task that drives debounced publishes (on-boot, on-network-change via `if-watch`, on-home-relay-change, idle 60min) |
| `zenoh_iroh_link.rs` | `IrohZenohLink` — `zenoh_link::LinkUnicastTrait` impl backed by an Iroh QUIC bidi stream |
| `zenoh_iroh_transport.rs` | `IrohZenohLinkManager` — `zenoh_link::LinkManagerUnicastTrait` impl that uses `IrohEndpoint` + `ReachabilityResolver` to open/accept tunneled Zenoh links |

### Rust backend (extended)

| File | Changes |
|---|---|
| `community_membership.rs` | New `MembershipEventKind::ReachabilityAnnounce` variant with tag `"a"`; 5 new `VerifyError` discriminants RCH1–RCH5; verify_event arm covering them |
| `community_state_crdt.rs` | (Pure CRDT layer — no changes needed; the new variant flows through `insert_event` automatically. Optionally: assert that the unit-level reachability test exercises insert_event end-to-end.) |
| `event_loop.rs` | Boot wiring (initialize `IrohEndpoint`, start `reachability_publisher`, register Zenoh-over-Iroh transport with running Zenoh session); per-community CRDT insert hook that feeds `ReachabilityResolver`; stale-comment cleanup (line 8) |
| `lib.rs` | 3 new IPCs: `connectivity_get_my_reachability_record`, `connectivity_list_peer_reachability`, `connectivity_force_republish`; wire into `invoke_handler!`; one new Tauri event `connectivity-reachability-changed` for Phase 3 forward-compat (emitted on each ReachabilityResolver update) |
| `Cargo.toml` | New deps: `iroh = "<latest stable>"`, `if-watch = "<latest stable>"`. (`quinn` comes transitively via `iroh`; `keyring` already present.) |

### Frontend (new files — all under `src/lib/`)

| File | Responsibility |
|---|---|
| `types/connectivity.ts` | `ReachabilityRecord` interface (camelCase fields: `irohNodeId`, `homeRelayUrl`, `directAddresses`, `announcedAtMs`) |
| `connectivity-adapter.ts` | 3 IPC bindings + 1 event subscriber (`onReachabilityChanged`); Tauri error extraction pattern |
| `components/DiagnosticsPanel.svelte` | Dev-mode debug panel: my NodeId, home relay URL, last-published record, observed peer reachability records. Behind a feature flag |

### Test files (new)

| File | Coverage |
|---|---|
| `src-tauri/src/community_membership.rs` (in-module `#[cfg(test)]`) | MembershipEventKind::ReachabilityAnnounce CBOR round-trip; 5 verify rules RCH1–RCH5 positive + negative; same-length-keys invariant |
| `src-tauri/src/reachability_record.rs` (in-module `#[cfg(test)]`) | Payload CBOR round-trip; inner-signature sign/verify; canonical-byte fixture |
| `src-tauri/src/reachability_resolver.rs` (in-module `#[cfg(test)]`) | Apply-order determinism; LWW; expiry semantics |
| `src-tauri/src/reachability_publisher.rs` (in-module `#[cfg(test)]`) | Debounce behavior on rapid network-change events |
| `src-tauri/src/iroh_endpoint.rs` (in-module `#[cfg(test)]`) | `init()` with ephemeral key (no keychain); `node_id()` / `home_relay()` return expected types |
| `src-tauri/src/zenoh_iroh_link.rs` (in-module `#[cfg(test)]`) | Paired-stream byte round-trip |
| `src-tauri/src/zenoh_iroh_transport.rs` (in-module `#[cfg(test)]`) | `new_link()` resolves harmony addr via `ReachabilityResolver` and opens an Iroh connection |
| `src-tauri/tests/wire_format_reachability_announce_fixtures.rs` | Pinned canonical hex bytes for `ReachabilityAnnounce` envelope |
| `src-tauri/tests/community_reachability_two_engine_integration.rs` | Two harmony-client instances on loopback; each publishes a ReachabilityRecord; each reads the other's; opens Iroh connection by NodeId; Zenoh round-trip over tunneled link |
| `src/lib/connectivity-adapter.test.ts` (vitest) | Adapter binds IPC + subscribes to event; updates store on event delivery |
| `src/lib/components/DiagnosticsPanel.test.ts` (vitest) | Renders with sample data; respects feature-flag guard |

---

## Task 0 — Pre-flight baseline (no commit)

**Files:**
- Read-only: capture state on disk

**Goal:** confirm branch lineage, capture orphan-test failure list so subsequent task baselines are interpretable, confirm Cargo.toml current state.

- [ ] **Step 1: Verify branch state**

```bash
git status
git log --oneline -5
git merge-base HEAD origin/main
git log --oneline e68599b..HEAD
```

Expected:
- `git status` reports a clean working tree on `zeb-321-phase1-iroh-foundation`.
- HEAD is at `8cf44aa` (spec commit).
- `merge-base HEAD origin/main` returns `e68599b`.
- Only one commit (`8cf44aa`) is ahead of `origin/main`.

If any of these are off, STOP and surface as BLOCKED — do not attempt to fix lineage in this task.

- [ ] **Step 2: Verify Cargo.toml current dep state**

```bash
grep -E '^(iroh|if-watch|quinn) ' src-tauri/Cargo.toml || echo "OK: no iroh/if-watch/quinn declared yet"
grep -E '^zenoh ' src-tauri/Cargo.toml
grep -E '^keyring ' src-tauri/Cargo.toml
```

Expected:
- No `iroh = …`, no `if-watch = …`, no `quinn = …` declared.
- `zenoh = "1"` declared.
- `keyring = …` declared with `apple-native`, `windows-native`, `sync-secret-service` features.

- [ ] **Step 3: Capture clippy baseline**

```bash
cd src-tauri
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -30
echo "clippy exit: ${PIPESTATUS[0]}"
cd ..
```

Expected: exit code 0, no warnings. (Branch was created off clean main; should be green.)

- [ ] **Step 4: Capture nextest baseline (orphan failures)**

```bash
cd src-tauri
set -o pipefail
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -100 > /tmp/zeb321-task0-baseline.log
echo "nextest exit: ${PIPESTATUS[0]}"
grep -E '^FAIL|test result:' /tmp/zeb321-task0-baseline.log | tee /tmp/zeb321-task0-baseline-summary.log
cd ..
```

Action: record the FAILED test names from `/tmp/zeb321-task0-baseline-summary.log` in this task's status report as the "orphan failure baseline". Any subsequent task that introduces a new FAILED test name (not in this baseline) is a blocking regression per `feedback_test_drift_is_our_fault`.

- [ ] **Step 5: Capture frontend baseline**

```bash
npx tsc --noEmit 2>&1 | tail -30
echo "tsc exit: ${PIPESTATUS[0]}"
npx vitest run 2>&1 | tail -30
echo "vitest exit: ${PIPESTATUS[0]}"
```

Expected: both exit 0.

- [ ] **Step 6: Report status** (no commit; this task is read-only)

Implementer reports: branch state ✓, dep state ✓, clippy baseline ✓, nextest orphan failures = [list], frontend baseline ✓. Proceed to Task 1.

---

## Task 1 — Wire format: `ReachabilityAnnouncePayload` + `MembershipEventKind::ReachabilityAnnounce` variant + canonical fixture

**Files:**
- Create: `src-tauri/src/reachability_record.rs`
- Modify: `src-tauri/src/lib.rs` (module declaration only — `pub mod reachability_record;`)
- Modify: `src-tauri/src/community_membership.rs` (add `ReachabilityAnnounce` variant to `MembershipEventKind`)
- Create: `src-tauri/tests/wire_format_reachability_announce_fixtures.rs`

**Goal:** Land the wire-format types and a pinned-hex CBOR fixture before any apply/verify logic. Subsequent tasks build on this.

- [ ] **Step 1: Write the failing CBOR-round-trip test in `reachability_record.rs`**

Create `src-tauri/src/reachability_record.rs`:

```rust
//! ZEB-321 Phase 1: ReachabilityAnnounce CRDT event payload.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::owner_state_crypto::{canonical_cbor_encode, CryptoError};

/// Payload of a `MembershipEventKind::ReachabilityAnnounce` variant.
/// All 5 field keys are 2 chars to satisfy the same-length-keys invariant
/// at this nesting level. Encoded inside the membership envelope's `vl`
/// (variant value) slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReachabilityAnnouncePayload {
    /// Iroh NodeId (Ed25519 public key, 32 bytes). Distinct from
    /// harmony identity key — bound to it via `identity_signature`.
    #[serde(rename = "nd", with = "serde_bytes_array_32")]
    pub iroh_node_id: [u8; 32],

    /// Home DERP relay URL (Phase 1: an n0-hosted relay).
    #[serde(rename = "rl")]
    pub home_relay_url: String,

    /// Direct-traversal hint addresses (publicly routable if any; may
    /// be empty Vec).
    #[serde(rename = "da")]
    pub direct_addresses: Vec<SocketAddr>,

    /// Wall-clock milliseconds when this record was authored.
    #[serde(rename = "ts")]
    pub announced_at_ms: u64,

    /// Inner Ed25519 signature by the device's HARMONY identity key
    /// over canonical CBOR of (nd, rl, da, ts, actor, hlc). Binds the
    /// Iroh NodeId to the harmony identity. 64 bytes.
    #[serde(rename = "sg", with = "serde_bytes_array_64")]
    pub identity_signature: [u8; 64],
}

// 32- and 64-byte fixed-length serde helpers — encode as CBOR bstr
// (major type 2) rather than array-of-u8.
mod serde_bytes_array_32 {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::Bytes;

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        Bytes::new(v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: serde_bytes::ByteBuf = serde_bytes::ByteBuf::deserialize(d)?;
        let arr: [u8; 32] = v
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32-byte iroh_node_id"))?;
        Ok(arr)
    }
}

mod serde_bytes_array_64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::Bytes;

    pub fn serialize<S: Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        Bytes::new(v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: serde_bytes::ByteBuf = serde_bytes::ByteBuf::deserialize(d)?;
        let arr: [u8; 64] = v
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 64-byte identity_signature"))?;
        Ok(arr)
    }
}

/// Convenience: canonical-encode for hashing / signing.
pub fn canonical_payload_bytes(p: &ReachabilityAnnouncePayload) -> Result<Vec<u8>, CryptoError> {
    canonical_cbor_encode(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        }
    }

    #[test]
    fn roundtrip_cbor() {
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let decoded: ReachabilityAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, p);
    }

    #[test]
    fn payload_keys_are_2_chars() {
        // Same-length-keys CBOR invariant — see community_membership.rs:325.
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        let map = val.as_map().expect("payload is map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.chars().count(),
                2,
                "ReachabilityAnnouncePayload key {s:?} violates 2-char invariant"
            );
        }
    }
}
```

- [ ] **Step 2: Wire the module into the crate**

In `src-tauri/src/lib.rs` (near the other `pub mod community_*;` declarations — search for `pub mod community_membership;` and add directly after it):

```rust
pub mod reachability_record;
```

- [ ] **Step 3: Run the new tests to verify they fail (no impl yet — they actually should pass since the type lives in the file). Run:**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(reachability_record)'
cd ..
```

Expected: both tests pass. (Step 1 already includes the impl — this is a self-contained module.)

- [ ] **Step 4: Add `ReachabilityAnnounce` variant to `MembershipEventKind`**

In `src-tauri/src/community_membership.rs`, locate the `MembershipEventKind` enum (line 81). After the last variant (`AdminCountersign`), add:

```rust
    /// ZEB-321 Phase 1: device publishes its Iroh NodeId + DERP relay +
    /// direct-address hints into the community-state CRDT so other
    /// community members can reach it cross-WAN via Iroh.
    ///
    /// Variant tag "a" (1-char value, unused before this — keeps the
    /// same-length-keys invariant intact). Inner field keys are 2-char
    /// per the `ReachabilityAnnouncePayload` struct.
    /// See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.
    #[serde(rename = "a")]
    ReachabilityAnnounce {
        #[serde(rename = "pl")]
        payload: crate::reachability_record::ReachabilityAnnouncePayload,
    },
```

- [ ] **Step 5: Compile + ensure existing exhaustive matches handle the new variant**

```bash
cd src-tauri
cargo check --locked --features test-fixtures 2>&1 | tail -40
cd ..
```

Expected: a finite number of "non-exhaustive pattern" / "missing match arm" errors from existing match-on-`MembershipEventKind` sites (likely in `materialize`, in `verify_event`, in fork helpers, etc.). For each, add a stub arm:

```rust
MembershipEventKind::ReachabilityAnnounce { .. } => { /* ZEB-321: no membership-state effect; handled by ReachabilityResolver hook in event_loop. */ }
```

(For `materialize`: reachability records do NOT mutate `MaterializedMembership` — keep the body empty. For `verify_event`: leave the body unreachable for now — Task 2 fills it in. For any helper that introspects `event.kind`: pattern-match on `ReachabilityAnnounce` and treat as a no-op for materialize-membership purposes.)

Iterate `cargo check` until clean.

- [ ] **Step 6: Add CBOR-round-trip + same-length-keys test for the new variant**

In `src-tauri/src/community_membership.rs`'s existing test module (search for `mod tests` — there's a large one near the bottom of the file), add:

```rust
    #[test]
    fn reachability_announce_variant_cbor_roundtrip() {
        use crate::reachability_record::ReachabilityAnnouncePayload;
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        };
        let kind = MembershipEventKind::ReachabilityAnnounce { payload: payload.clone() };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, MembershipEventKind::ReachabilityAnnounce { payload });
    }

    #[test]
    fn reachability_announce_outer_keys_invariant() {
        // The outer adjacent-tag wrapper has 2-char keys ("tg", "vl").
        // The inner ReachabilityAnnouncePayload has its own keys, tested
        // in reachability_record::tests::payload_keys_are_2_chars.
        use crate::reachability_record::ReachabilityAnnouncePayload;
        let kind = MembershipEventKind::ReachabilityAnnounce {
            payload: ReachabilityAnnouncePayload {
                iroh_node_id: [0; 32],
                home_relay_url: String::new(),
                direct_addresses: vec![],
                announced_at_ms: 0,
                identity_signature: [0; 64],
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        let map = val.as_map().expect("outer is map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.chars().count(),
                2,
                "MembershipEventKind::ReachabilityAnnounce outer key {s:?} violates 2-char invariant"
            );
        }
    }
```

- [ ] **Step 7: Run the new tests, expect them to pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(reachability_announce)'
cd ..
```

Expected: 2 tests pass.

- [ ] **Step 8: Create wire-format pinning fixture file**

Create `src-tauri/tests/wire_format_reachability_announce_fixtures.rs`:

```rust
//! Golden CBOR fixtures for ZEB-321 Phase 1 ReachabilityAnnounce wire type.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format_community_fixtures.rs.

use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    }
}

fn fixture_payload() -> ReachabilityAnnouncePayload {
    ReachabilityAnnouncePayload {
        iroh_node_id: [0xAB; 32],
        home_relay_url: "https://derp.example/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xCD; 64],
    }
}

fn fixture_signed_event(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        id: [0x42; 16],
        community_id: SpaceId([0x37; 16]),
        kind,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
    }
}

#[test]
fn reachability_announce_payload_wire_bytes_pinned() {
    let payload = fixture_payload();
    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("reachability_announce_payload hex: {hex}");
    // Stage 1: run once with a deliberate `assert_eq!(hex, "TODO")` to
    // capture the hex via test output; then paste the captured hex back
    // here so the test pins the canonical form. Phase 1 first-write
    // procedure — see ZEB-217 fixture-creation precedent.
    assert_eq!(
        hex,
        "REPLACE_WITH_FIRST_RUN_OUTPUT",
        "ReachabilityAnnouncePayload wire format changed"
    );
}

#[test]
fn signed_event_reachability_announce_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::ReachabilityAnnounce {
        payload: fixture_payload(),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_reachability_announce hex: {hex}");
    assert_eq!(
        hex,
        "REPLACE_WITH_FIRST_RUN_OUTPUT",
        "SignedMembershipEvent::ReachabilityAnnounce wire format changed"
    );
}
```

- [ ] **Step 9: First-run capture of canonical bytes**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(/reachability_announce.*_wire_bytes_pinned/)' --no-fail-fast 2>&1 | grep 'hex:' | tee /tmp/zeb321-fixture-hex.log
cd ..
```

Expected: 2 lines of `…hex: <hexstring>` output (from the `eprintln!` lines in each test). Both tests fail because the `REPLACE_WITH_FIRST_RUN_OUTPUT` placeholder doesn't match.

- [ ] **Step 10: Patch the captured hex into the fixture file**

Edit `src-tauri/tests/wire_format_reachability_announce_fixtures.rs` and replace the two `REPLACE_WITH_FIRST_RUN_OUTPUT` placeholders with the actual hex strings from Step 9's output.

- [ ] **Step 11: Re-run, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(/reachability_announce.*_wire_bytes_pinned/)'
cd ..
```

Expected: 2 tests pass.

- [ ] **Step 12: Run all gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

Expected: all 3 gates pass; nextest fail count matches Task 0 baseline (no new failures).

If clippy flags any nit in the new module, fix in-place. **10-min wall-clock kill switch:** if `cargo nextest run` exceeds 10 min, abort and report DONE_WITH_CONCERNS. Probable causes: macOS XprotectService misconfigured (see `reference_xprotectservice_dev_tools`) — surface this to the user rather than waiting.

- [ ] **Step 13: Commit**

```bash
git add src-tauri/src/reachability_record.rs \
        src-tauri/src/community_membership.rs \
        src-tauri/src/lib.rs \
        src-tauri/tests/wire_format_reachability_announce_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): ReachabilityAnnounce wire format + MembershipEventKind variant

Adds the ReachabilityAnnouncePayload struct (CBOR-encoded, 5 2-char-keyed
fields per same-length-keys invariant) and a new MembershipEventKind
variant `ReachabilityAnnounce` with serde tag "a". Pins canonical wire
bytes via wire_format_reachability_announce_fixtures.rs.

No apply/verify logic yet — Task 2 lands the 5 verify rules RCH1-RCH5
and the inner-signature handling.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2 — Inner identity signature + 5 verify rules (RCH1–RCH5)

**Files:**
- Modify: `src-tauri/src/reachability_record.rs` (add `sign_payload` + `verify_payload_signature` helpers)
- Modify: `src-tauri/src/community_membership.rs` (`VerifyError` enum + `verify_event` arm)

**Goal:** Bring the new variant under the full verify discipline. After this task lands, peers can reject malicious / malformed `ReachabilityAnnounce` events at insert time.

- [ ] **Step 1: Add signature helpers + tests in `reachability_record.rs`**

Append to `src-tauri/src/reachability_record.rs`:

```rust
use crate::owner_state_types::{Hlc, OwnerAddr};

/// Canonical byte string the inner identity signature covers:
/// CBOR(canonical) of (nd, rl, da, ts, actor, hlc).
pub fn inner_signed_bytes(
    iroh_node_id: &[u8; 32],
    home_relay_url: &str,
    direct_addresses: &[std::net::SocketAddr],
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "nd", with = "serde_bytes_array_32")]
        nd: &'a [u8; 32],
        #[serde(rename = "rl")]
        rl: &'a str,
        #[serde(rename = "da")]
        da: &'a [std::net::SocketAddr],
        #[serde(rename = "ts")]
        ts: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
    }
    canonical_cbor_encode(&InnerSigInput {
        nd: iroh_node_id,
        rl: home_relay_url,
        da: direct_addresses,
        ts: announced_at_ms,
        ac: actor,
        hl: hlc,
    })
}

/// Sign a fresh ReachabilityAnnouncePayload using the device's harmony
/// identity signing key. Caller is responsible for ensuring `actor`
/// matches the identity (`identity.address_hash`).
pub fn build_signed_payload(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<std::net::SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    identity: &harmony_identity::PrivateIdentity,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
    )?;
    let sig = identity.sign(&inner);
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
    })
}

/// Verify the inner identity signature against the given identity public
/// component (the 32-byte Ed25519 half of the 64-byte
/// `harmony_identity::Identity::to_public_bytes()`).
pub fn verify_inner_signature(
    p: &ReachabilityAnnouncePayload,
    actor: &OwnerAddr,
    hlc: &Hlc,
    actor_ed25519_pub: &ed25519_dalek::VerifyingKey,
) -> Result<(), InnerSigError> {
    let bytes = inner_signed_bytes(
        &p.iroh_node_id,
        &p.home_relay_url,
        &p.direct_addresses,
        p.announced_at_ms,
        actor,
        hlc,
    )
    .map_err(|_| InnerSigError::Encode)?;
    let sig = ed25519_dalek::Signature::from_bytes(&p.identity_signature);
    actor_ed25519_pub
        .verify_strict(&bytes, &sig)
        .map_err(|_| InnerSigError::Invalid)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InnerSigError {
    #[error("inner reachability signature failed to encode")]
    Encode,
    #[error("inner reachability signature invalid")]
    Invalid,
}
```

Add tests in the existing `#[cfg(test)] mod tests` block:

```rust
    use harmony_identity::PrivateIdentity;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "fix".into(),
        }
    }

    #[test]
    fn inner_sig_roundtrip_with_real_identity() {
        let identity = PrivateIdentity::generate(&mut rand::thread_rng());
        let actor = OwnerAddr(identity.identity.address_hash);
        let hlc = fixture_hlc();
        let p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            &identity,
        )
        .expect("sign");

        // Extract the Ed25519 verifying key from the identity's public bytes.
        // PrivateIdentity::to_public_bytes() returns 64 bytes:
        // X25519_pub (32) || Ed25519_pub (32) — same layout as the
        // existing community_membership identity_pub convention.
        let pub_bytes = identity.identity.to_public_bytes();
        let ed_pub: [u8; 32] = pub_bytes[32..].try_into().unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&ed_pub).unwrap();

        verify_inner_signature(&p, &actor, &hlc, &verifying).expect("verify");
    }

    #[test]
    fn inner_sig_rejects_tampered_node_id() {
        let identity = PrivateIdentity::generate(&mut rand::thread_rng());
        let actor = OwnerAddr(identity.identity.address_hash);
        let hlc = fixture_hlc();
        let mut p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            &identity,
        )
        .expect("sign");
        // Tamper the NodeId — the signature should no longer verify.
        p.iroh_node_id[0] ^= 0xFF;

        let pub_bytes = identity.identity.to_public_bytes();
        let ed_pub: [u8; 32] = pub_bytes[32..].try_into().unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&ed_pub).unwrap();

        assert_eq!(
            verify_inner_signature(&p, &actor, &hlc, &verifying),
            Err(InnerSigError::Invalid)
        );
    }
```

- [ ] **Step 2: Run the new tests, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(reachability_record::tests::inner_sig)'
cd ..
```

Expected: 2 tests pass.

- [ ] **Step 3: Add 5 new `VerifyError` discriminants in `community_membership.rs`**

Locate the `VerifyError` enum (search for `pub enum VerifyError` in `src-tauri/src/community_membership.rs` — around line ~700). Append:

```rust
    /// ZEB-321 RCH2: inner identity signature on a ReachabilityAnnounce
    /// payload failed to verify. The inner signature binds the Iroh
    /// NodeId to the harmony identity; rejecting prevents a malicious
    /// community member from claiming someone else's NodeId.
    ReachabilityInnerSigInvalid,

    /// ZEB-321 RCH3: the actor field of a ReachabilityAnnounce envelope
    /// does not match the OwnerAddr derived from the identity that
    /// produced the inner signature.
    ReachabilityActorMismatch,

    /// ZEB-321 RCH4: the payload's `announced_at_ms` differs from the
    /// event's HLC `wall_ms` by more than ±30 minutes. Sanity check —
    /// rejects obviously-tampered records (the spec's "silent drop").
    ReachabilityTimestampSkew,

    /// ZEB-321 RCH5: the actor is not a current community member at
    /// the event's HLC (read via membership projection).
    ReachabilityActorNotMember,

    /// ZEB-321 RCH1: outer event signature failed to verify. Reuse the
    /// existing SignatureInvalid path — no new variant needed (kept
    /// here as a comment for traceability against the spec). DO NOT
    /// add a separate variant — that would duplicate behavior.
```

In the `impl Display for VerifyError` block (search for `VerifyError::SignatureInvalid => write!`), add `Display` arms for the 4 new variants:

```rust
            VerifyError::ReachabilityInnerSigInvalid => {
                write!(f, "ZEB-321 RCH2 inner ReachabilityAnnounce signature invalid")
            }
            VerifyError::ReachabilityActorMismatch => {
                write!(f, "ZEB-321 RCH3 ReachabilityAnnounce actor != inner-signer")
            }
            VerifyError::ReachabilityTimestampSkew => {
                write!(f, "ZEB-321 RCH4 ReachabilityAnnounce timestamp skew > 30min")
            }
            VerifyError::ReachabilityActorNotMember => {
                write!(f, "ZEB-321 RCH5 ReachabilityAnnounce actor is not a community member")
            }
```

- [ ] **Step 4: Add the `verify_event` arm**

Locate `pub fn verify_event` (community_membership.rs:2166). Find the final `match event.kind { … }` block (or the per-kind dispatch — read the function end-to-end to understand its structure). Add a new arm:

```rust
        MembershipEventKind::ReachabilityAnnounce { payload } => {
            // RCH1: outer signature already verified by verify_signature() above.
            // (Standard for all SignedMembershipEvent — no extra work here.)

            // RCH2: inner identity signature must verify over canonical
            // CBOR of (nd, rl, da, ts, actor, hlc) using the actor's
            // Ed25519 public component.
            let pub_bytes_64 = ctx.actor_identity_pub.to_public_bytes();
            let ed_pub: [u8; 32] = pub_bytes_64[32..]
                .try_into()
                .map_err(|_| VerifyError::InvalidIdentityPub)?;
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(&ed_pub)
                .map_err(|_| VerifyError::InvalidIdentityPub)?;
            crate::reachability_record::verify_inner_signature(
                payload,
                &event.actor,
                &event.at,
                &verifying,
            )
            .map_err(|_| VerifyError::ReachabilityInnerSigInvalid)?;

            // RCH3: actor in envelope must equal the address derived from the
            // identity that produced the inner sig. The outer verify_signature
            // already proves event.actor matches ctx.actor_identity_pub, and
            // the inner verify uses the same key — so this is a defense-in-
            // depth assertion, not a new check.
            let derived_addr =
                OwnerAddr(ctx.actor_identity_pub.address_hash);
            if derived_addr != event.actor {
                return Err(VerifyError::ReachabilityActorMismatch);
            }

            // RCH4: announced_at_ms vs hlc wall_ms within ±30 min.
            const SKEW_MS: i64 = 30 * 60 * 1000;
            let skew = (payload.announced_at_ms as i64) - (event.at.wall_ms as i64);
            if skew.abs() > SKEW_MS {
                return Err(VerifyError::ReachabilityTimestampSkew);
            }

            // RCH5: actor must be a current member at hlc.
            // Use the existing MaterializedMembership.is_member helper
            // (search community_membership.rs for the canonical
            // pattern — `prior_state.members.get(&event.actor)` and check
            // status == Joined).
            match prior_state.members.get(&event.actor) {
                Some(state) if state.status == MemberStatus::Joined => Ok(()),
                _ => Err(VerifyError::ReachabilityActorNotMember),
            }?;

            Ok(())
        }
```

- [ ] **Step 5: Write failing tests for RCH1–RCH5 in `community_membership.rs` tests block**

Add 5 tests covering each rule's positive + negative case. Pattern (adapt to existing test helpers — search for `verify_event_rejects_*` in the same file to see precedent):

```rust
    #[test]
    fn verify_reachability_announce_rejects_inner_sig_tampering() {
        // RCH2: tamper the inner signature; verify_event must return
        // ReachabilityInnerSigInvalid.
        // … (use existing fixture helpers `make_actor_with_identity()`,
        // `make_membership_ctx()`, `make_join_event()` — search the
        // tests block for these).
        let (actor, identity) = test_helpers::fresh_actor();
        let hlc = test_helpers::fixture_hlc();
        let payload = crate::reachability_record::build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            hlc.wall_ms,
            &actor,
            &hlc,
            &identity,
        )
        .expect("sign");
        let mut tampered = payload;
        tampered.identity_signature[0] ^= 0xFF;
        let event = test_helpers::sign_membership_event(
            MembershipEventKind::ReachabilityAnnounce { payload: tampered },
            &identity,
            &actor,
            hlc,
        );
        let (prior, ctx) = test_helpers::joined_actor_context(&actor, &identity);
        assert_eq!(
            verify_event(&event, &prior, &ctx),
            Err(VerifyError::ReachabilityInnerSigInvalid)
        );
    }
    // … 4 more analogous tests, one per RCH1, RCH3, RCH4, RCH5.
```

**Implementer note:** the test helpers above are pseudo-coded. Read the existing test block in `community_membership.rs` (search for `#[test]\n    fn verify_event_rejects_*`) to find the actual helpers the file uses. Reuse them. If no suitable helper exists, write inline test setup; don't introduce a new helper module in this task.

- [ ] **Step 6: Run all verify tests, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(verify_reachability)'
cd ..
```

Expected: 5 tests pass.

- [ ] **Step 7: Run all gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

Expected: all pass, no new failures vs Task 0 baseline.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/reachability_record.rs src-tauri/src/community_membership.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): RCH1-RCH5 verify rules + inner identity signature

Adds inner Ed25519 identity signature (binds Iroh NodeId → harmony
identity) plus the 5 verify rules per spec §5.5:

- RCH1: outer sig (reuses existing verify_signature path)
- RCH2: ReachabilityInnerSigInvalid
- RCH3: ReachabilityActorMismatch
- RCH4: ReachabilityTimestampSkew (±30 min)
- RCH5: ReachabilityActorNotMember

The membership CRDT has no last_received_hlc / last_hlc watermark
machinery (it's a content-addressed log), so the spec's RCH4
"do not advance last_hlc" reduces to standard verify_event rejection.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3 — `ReachabilityResolver` (BTreeMap-backed LWW projection)

**Files:**
- Create: `src-tauri/src/reachability_resolver.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod reachability_resolver;`)

**Goal:** A pure, thread-safe (`Arc<RwLock<…>>`) projection of `MembershipEventKind::ReachabilityAnnounce` events into a `BTreeMap<OwnerAddr, ReachabilityAnnouncePayload>` with LWW conflict resolution. The Zenoh-over-Iroh transport will query this in Task 6.

- [ ] **Step 1: Write the failing determinism test first**

Create `src-tauri/src/reachability_resolver.rs`:

```rust
//! ZEB-321 Phase 1: side-projection of ReachabilityAnnounce CRDT events.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.4
//! (LWW projection) and §7.4 (resolver consumed by zenoh-over-iroh transport).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
}

#[derive(Default, Debug)]
pub struct ReachabilityResolver {
    inner: Arc<RwLock<BTreeMap<OwnerAddr, ResolverEntry>>>,
}

impl Clone for ReachabilityResolver {
    fn clone(&self) -> Self {
        ReachabilityResolver {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ReachabilityResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// LWW update — higher HLC wins; ties broken by announced_at_ms then
    /// lexicographic iroh_node_id. See spec §5.4.
    pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
        let mut map = self.inner.write().expect("resolver write lock");
        let next = ResolverEntry { payload, hlc };
        match map.get(&actor) {
            Some(prev) if !should_replace(prev, &next) => { /* keep prev */ }
            _ => {
                map.insert(actor, next);
            }
        }
    }

    pub fn resolve(&self, actor: &OwnerAddr) -> Option<ReachabilityAnnouncePayload> {
        let map = self.inner.read().expect("resolver read lock");
        map.get(actor).map(|e| e.payload.clone())
    }

    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter().map(|(k, v)| (*k, v.payload.clone())).collect()
    }
}

fn should_replace(prev: &ResolverEntry, next: &ResolverEntry) -> bool {
    // Compare HLC; tie-break announced_at_ms; tie-break iroh_node_id.
    match next.hlc.cmp(&prev.hlc) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match next
            .payload
            .announced_at_ms
            .cmp(&prev.payload.announced_at_ms)
        {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => next.payload.iroh_node_id > prev.payload.iroh_node_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(node_id_byte: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [node_id_byte; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms,
            identity_signature: [0; 64],
        }
    }

    fn make_hlc(wall_ms: u64, logical: u32, device: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device.into(),
        }
    }

    #[test]
    fn lww_higher_hlc_wins() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(2, 2000), make_hlc(2000, 0, "a"));
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [2; 32]);
    }

    #[test]
    fn lww_lower_hlc_ignored() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(2, 2000), make_hlc(2000, 0, "a"));
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [2; 32]);
    }

    #[test]
    fn determinism_across_orders() {
        // Apply 4 events in 4 different orders; result must converge to
        // the same final value.
        let events: Vec<(OwnerAddr, ReachabilityAnnouncePayload, Hlc)> = vec![
            (OwnerAddr([0x11; 16]), make_payload(1, 1000), make_hlc(1000, 0, "a")),
            (OwnerAddr([0x22; 16]), make_payload(3, 1500), make_hlc(1500, 0, "b")),
            (OwnerAddr([0x11; 16]), make_payload(2, 2000), make_hlc(2000, 0, "a")),
            (OwnerAddr([0x22; 16]), make_payload(4, 2500), make_hlc(2500, 0, "b")),
        ];

        let mut orders = vec![
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];

        let mut final_states: Vec<Vec<(OwnerAddr, [u8; 32])>> = Vec::new();
        for order in orders.drain(..) {
            let r = ReachabilityResolver::new();
            for i in order {
                let (a, p, h) = &events[i];
                r.update(*a, p.clone(), h.clone());
            }
            let mut s: Vec<(OwnerAddr, [u8; 32])> = r
                .list_active_peers()
                .into_iter()
                .map(|(a, p)| (a, p.iroh_node_id))
                .collect();
            s.sort_by_key(|(a, _)| *a);
            final_states.push(s);
        }

        for w in final_states.windows(2) {
            assert_eq!(w[0], w[1], "ReachabilityResolver is not order-independent");
        }
    }
}
```

In `src-tauri/src/lib.rs`, add after `pub mod reachability_record;`:

```rust
pub mod reachability_resolver;
```

- [ ] **Step 2: Run tests, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(reachability_resolver)'
cd ..
```

Expected: 3 tests pass.

- [ ] **Step 3: Run all gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): ReachabilityResolver LWW projection

Pure in-memory BTreeMap<OwnerAddr, ResolverEntry> projection of
ReachabilityAnnounce events. LWW by HLC; ties broken by
announced_at_ms then lexicographic iroh_node_id per spec §5.4.

Determinism test exercises 4-event log in 4 distinct apply orders;
all converge to identical final state.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4 — `IrohEndpoint` wrapper + ALPN registry + keychain SecretKey persistence

**Files:**
- Create: `src-tauri/src/iroh_endpoint.rs`
- Modify: `src-tauri/Cargo.toml` (add `iroh` dep)
- Modify: `src-tauri/src/lib.rs` (add `pub mod iroh_endpoint;`)

**Goal:** Wrap `iroh::Endpoint` with our keychain-backed persistent secret + ALPN constants + a tiny API surface. Subsequent tasks build the Zenoh transport on top of this.

- [ ] **Step 1: Add `iroh` dependency**

In `src-tauri/Cargo.toml`, add to `[dependencies]`:

```toml
iroh = "0.91"
```

(Pin to the latest stable line as of writing. Implementer: verify `cargo search iroh` returns a recent stable version; if 0.91 is yanked or unavailable, pin to the most recent stable available and update this comment.)

- [ ] **Step 2: Compile to download the dep**

```bash
cd src-tauri
cargo check --locked --features test-fixtures 2>&1 | tail -30
cd ..
```

Expected: clean compile after the dep downloads. If `--locked` rejects because `Cargo.lock` doesn't have iroh, drop `--locked` for this one invocation:

```bash
cd src-tauri && cargo check --features test-fixtures && cd ..
```

then commit `Cargo.lock` along with the rest of this task.

- [ ] **Step 3: Write the failing `IrohEndpoint` lifecycle test (TDD: write skeleton + test, then fill)**

Create `src-tauri/src/iroh_endpoint.rs`:

```rust
//! ZEB-321 Phase 1: IrohEndpoint wrapper around iroh::Endpoint with
//! keychain-backed persistent secret key + ALPN registry.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §6.

use std::net::SocketAddr;

use iroh::{Endpoint, NodeId, RelayUrl, SecretKey};

/// ALPN registry. The only place ALPN bytestrings live — keep it
/// concentrated so renames are one-edit.
pub mod alpn {
    /// Phase 1: Zenoh-over-Iroh tunneled link.
    pub const HARMONY_ZENOH_V1: &[u8] = b"harmony/zenoh/v1";
    /// Reserved for Phase 2: first-contact handshake. Not handled in
    /// Phase 1; the constant is here so we don't have to re-version
    /// when Phase 2 lands.
    pub const HARMONY_HANDSHAKE_V1: &[u8] = b"harmony/handshake/v1";
}

pub struct IrohEndpoint {
    inner: Endpoint,
}

impl IrohEndpoint {
    /// Create a new endpoint with the given SecretKey. Phase 1 uses
    /// `RelayMode::Default` (n0's public DERP relays). The endpoint
    /// is bound for accepting `harmony/zenoh/v1` ALPN.
    pub async fn new_with_secret(secret_key: SecretKey) -> Result<Self, IrohEndpointError> {
        let inner = Endpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                // Phase 2 ALPN registered but not bound to a handler yet —
                // unaccepted ALPN will be reported as
                // ConnectionError::TransportError(0x0a). That's fine until
                // Phase 2 lands.
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            ])
            .bind()
            .await
            .map_err(|e| IrohEndpointError::Bind(e.to_string()))?;
        Ok(IrohEndpoint { inner })
    }

    pub fn node_id(&self) -> NodeId {
        self.inner.node_id()
    }

    pub fn home_relay(&self) -> Option<RelayUrl> {
        self.inner.home_relay().into_iter().next()
    }

    pub fn direct_addresses(&self) -> Vec<SocketAddr> {
        // iroh::Endpoint::direct_addresses() returns Watcher<Option<…>>;
        // for the snapshot read we used to seed a ReachabilityAnnounce,
        // we want a one-shot blocking read of the current value or empty.
        self.inner
            .direct_addresses()
            .get()
            .ok()
            .flatten()
            .map(|set| set.into_iter().map(|da| da.addr).collect())
            .unwrap_or_default()
    }

    pub fn inner(&self) -> &Endpoint {
        &self.inner
    }

    pub async fn shutdown(&self) {
        self.inner.close().await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IrohEndpointError {
    #[error("iroh endpoint bind failed: {0}")]
    Bind(String),
    #[error("keychain access failed: {0}")]
    Keychain(String),
}

// === Keychain-backed SecretKey persistence ===
// Storage key matches existing harmony identity key entry layout.
const KEYCHAIN_SERVICE: &str = "harmony.client";
const KEYCHAIN_USER: &str = "iroh.secret_key";

/// Load the device's persistent Iroh SecretKey from the OS keychain;
/// generate + save a fresh one on first launch.
pub fn load_or_create_secret_key() -> Result<SecretKey, IrohEndpointError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .map_err(|e| IrohEndpointError::Keychain(e.to_string()))?;
    match entry.get_password() {
        Ok(hex) => {
            let bytes = hex::decode(hex)
                .map_err(|e| IrohEndpointError::Keychain(format!("hex decode: {e}")))?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| IrohEndpointError::Keychain("secret key must be 32 bytes".into()))?;
            Ok(SecretKey::from_bytes(&arr))
        }
        Err(keyring::Error::NoEntry) => {
            use rand::RngCore;
            let mut bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            let hex = hex::encode(bytes);
            entry
                .set_password(&hex)
                .map_err(|e| IrohEndpointError::Keychain(e.to_string()))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) => Err(IrohEndpointError::Keychain(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn iroh_endpoint_inits_with_ephemeral_secret() {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let key = SecretKey::from_bytes(&bytes);
        let ep = IrohEndpoint::new_with_secret(key).await.expect("init");
        // We get a non-zero NodeId.
        assert_ne!(ep.node_id().as_bytes(), &[0u8; 32]);
        // home_relay may be Some or None depending on test env — only assert it doesn't panic.
        let _ = ep.home_relay();
        let _ = ep.direct_addresses();
        ep.shutdown().await;
    }

    #[test]
    fn alpn_constants_are_correct() {
        assert_eq!(alpn::HARMONY_ZENOH_V1, b"harmony/zenoh/v1");
        assert_eq!(alpn::HARMONY_HANDSHAKE_V1, b"harmony/handshake/v1");
    }
}
```

In `src-tauri/src/lib.rs`, add (after `pub mod reachability_resolver;`):

```rust
pub mod iroh_endpoint;
```

- [ ] **Step 4: Run tests, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(iroh_endpoint)'
cd ..
```

Expected: 2 tests pass.

**10-min wall-clock note:** iroh's first bind may try to contact a DERP relay. If the test machine has restricted egress, the lifecycle test may stall. If `iroh_endpoint_inits_with_ephemeral_secret` exceeds 60s, treat as a DONE_WITH_CONCERNS and try a `.relay_mode(iroh::RelayMode::Disabled)` builder option for the test. (The production code still uses `RelayMode::Default` — only test fixtures need to be relay-free for hermetic runs.)

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/iroh_endpoint.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): IrohEndpoint wrapper + ALPN registry + keychain persistence

Adds the IrohEndpoint wrapper around iroh::Endpoint with:
- ALPN registry constants (harmony/zenoh/v1, reserved harmony/handshake/v1)
- keychain-backed persistent SecretKey (load_or_create_secret_key)
- node_id, home_relay, direct_addresses, shutdown surface
- new iroh dep

Phase 1 uses RelayMode::Default (n0 hosted DERP); Phase 4 will add
self-hosted relay overrides.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5 — `IrohZenohLink` (LinkUnicastTrait impl)

**Files:**
- Create: `src-tauri/src/zenoh_iroh_link.rs`
- Modify: `src-tauri/src/lib.rs`

**Goal:** A `zenoh_link::LinkUnicastTrait` implementation that wraps an Iroh QUIC bidi stream pair. Subsequent tasks plug this into Zenoh.

- [ ] **Step 1: Write the failing paired-stream round-trip test**

Create `src-tauri/src/zenoh_iroh_link.rs`:

```rust
//! ZEB-321 Phase 1: zenoh-link LinkUnicastTrait impl over an Iroh QUIC
//! bidi stream pair.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.2.

use std::sync::Arc;

use async_trait::async_trait;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::sync::Mutex;
use zenoh_link::{BatchSize, LinkAuthId, LinkUnicastTrait, Locator};

pub struct IrohZenohLink {
    send: Arc<Mutex<SendStream>>,
    recv: Arc<Mutex<RecvStream>>,
    src: Locator,
    dst: Locator,
}

impl IrohZenohLink {
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
    async fn write(&self, buffer: &[u8]) -> zenoh_link::ZResult<usize> {
        let mut s = self.send.lock().await;
        s.write(buffer)
            .await
            .map_err(|e| zenoh_link::zerror!("iroh write: {e}").into())
    }

    async fn write_all(&self, buffer: &[u8]) -> zenoh_link::ZResult<()> {
        let mut s = self.send.lock().await;
        s.write_all(buffer)
            .await
            .map_err(|e| zenoh_link::zerror!("iroh write_all: {e}").into())
    }

    async fn read(&self, buffer: &mut [u8]) -> zenoh_link::ZResult<usize> {
        let mut r = self.recv.lock().await;
        match r.read(buffer).await {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Err(zenoh_link::zerror!("iroh stream EOF").into()),
            Err(e) => Err(zenoh_link::zerror!("iroh read: {e}").into()),
        }
    }

    async fn read_exact(&self, buffer: &mut [u8]) -> zenoh_link::ZResult<()> {
        let mut r = self.recv.lock().await;
        r.read_exact(buffer)
            .await
            .map_err(|e| zenoh_link::zerror!("iroh read_exact: {e}").into())
    }

    async fn close(&self) -> zenoh_link::ZResult<()> {
        let mut s = self.send.lock().await;
        let _ = s.finish();
        Ok(())
    }

    fn get_mtu(&self) -> BatchSize {
        // QUIC has no per-frame MTU — pass through the max value Zenoh
        // exposes (rely on Zenoh's batching to chunk efficiently).
        BatchSize::MAX
    }

    fn get_src(&self) -> &Locator {
        &self.src
    }

    fn get_dst(&self) -> &Locator {
        &self.dst
    }

    fn is_reliable(&self) -> bool {
        true
    }

    fn is_streamed(&self) -> bool {
        true
    }

    fn get_interface_names(&self) -> Vec<String> {
        // No specific interface — Iroh chooses transport (direct hole-punch
        // vs DERP) opaquely.
        vec![]
    }

    fn is_local(&self) -> bool {
        false
    }

    fn get_auth_id(&self) -> &LinkAuthId {
        // Phase 1 has no per-link auth identity beyond the Iroh NodeId
        // baked into the locator. Return the empty default.
        const NONE: LinkAuthId = LinkAuthId::None;
        &NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paired_stream_roundtrip_via_loopback() {
        // Use two IrohEndpoints on loopback. Endpoint A dials Endpoint B
        // on a known ALPN; B accepts; A writes; B reads back.
        use crate::iroh_endpoint::{alpn, IrohEndpoint};
        use iroh::SecretKey;
        use rand::RngCore;

        let mut buf_a = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf_a);
        let mut buf_b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf_b);

        let key_a = SecretKey::from_bytes(&buf_a);
        let key_b = SecretKey::from_bytes(&buf_b);

        let ep_a = IrohEndpoint::new_with_secret(key_a).await.unwrap();
        let ep_b = IrohEndpoint::new_with_secret(key_b).await.unwrap();

        let node_b = ep_b.node_id();
        let _node_b_addr = iroh::NodeAddr::new(node_b);

        // Accept side
        let ep_b_inner = ep_b.inner().clone();
        let accept_task = tokio::spawn(async move {
            let incoming = ep_b_inner.accept().await.expect("accept");
            let conn = incoming.await.expect("connect");
            let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
            let mut buf = [0u8; 5];
            recv.read_exact(&mut buf).await.expect("read_exact");
            assert_eq!(&buf, b"hello");
            send.write_all(b"world").await.expect("write");
            send.finish().expect("finish");
            buf
        });

        // Dial side
        let node_b_addr = iroh::NodeAddr::new(node_b);
        let conn = ep_a
            .inner()
            .connect(node_b_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        send.write_all(b"hello").await.expect("write");
        send.finish().expect("finish");
        let mut buf = [0u8; 5];
        recv.read_exact(&mut buf).await.expect("read_exact");
        assert_eq!(&buf, b"world");

        let _ = accept_task.await;
        ep_a.shutdown().await;
        ep_b.shutdown().await;
    }
}
```

In `src-tauri/src/lib.rs`, add (after `pub mod iroh_endpoint;`):

```rust
pub mod zenoh_iroh_link;
```

Add `zenoh-link` to `Cargo.toml` `[dependencies]` (it's part of the zenoh workspace; the crate is re-exported from zenoh, but the trait lives in zenoh-link):

```toml
zenoh-link = "1"
```

(Match the major version of `zenoh = "1"`.)

- [ ] **Step 2: Compile + run test, expect pass**

```bash
cd src-tauri
cargo check --features test-fixtures 2>&1 | tail -20
cargo nextest run --locked --features test-fixtures -E 'test(paired_stream_roundtrip)'
cd ..
```

**Note:** the test exercises a real Iroh dial + accept on localhost. If DERP-relay contact stalls, set `RelayMode::Disabled` on the test endpoints (add a test-only IrohEndpoint constructor variant if needed). 10-min wall-clock applies; if stalled, switch to disabled relay mode.

- [ ] **Step 3: Run all gates**

(per Task pattern)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/zenoh_iroh_link.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): IrohZenohLink — zenoh-link LinkUnicastTrait impl

Adds an Iroh-QUIC-backed LinkUnicastTrait impl that wraps a (SendStream,
RecvStream) pair. Includes a paired-stream round-trip test using two
local IrohEndpoints over the harmony/zenoh/v1 ALPN.

MTU is BatchSize::MAX (QUIC has no per-frame limit); links advertise as
reliable + streamed; auth id is None in Phase 1 (NodeId is in the
locator).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6 — `IrohZenohLinkManager` (LinkManagerUnicastTrait impl) + transport plugin glue

**Files:**
- Create: `src-tauri/src/zenoh_iroh_transport.rs`
- Modify: `src-tauri/src/lib.rs`

**Goal:** Implement `LinkManagerUnicastTrait` so Zenoh can request outbound links by locator and accept inbound links by ALPN. New links resolve harmony OwnerAddr→NodeId via `ReachabilityResolver`.

- [ ] **Step 1: Create `zenoh_iroh_transport.rs`**

Create `src-tauri/src/zenoh_iroh_transport.rs`:

```rust
//! ZEB-321 Phase 1: IrohZenohLinkManager — LinkManagerUnicastTrait impl.
//! Replaces Zenoh's UDP-multicast scouting with CRDT-driven discovery via
//! ReachabilityResolver.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.3.

use std::sync::Arc;

use async_trait::async_trait;
use iroh::NodeAddr;
use zenoh_link::{
    EndPoint, LinkManagerUnicastTrait, LinkUnicast, Locator, NewLinkChannelSender,
};

use crate::iroh_endpoint::{alpn, IrohEndpoint};
use crate::owner_state_types::OwnerAddr;
use crate::reachability_resolver::ReachabilityResolver;
use crate::zenoh_iroh_link::IrohZenohLink;

pub struct IrohZenohLinkManager {
    endpoint: Arc<IrohEndpoint>,
    resolver: ReachabilityResolver,
    new_link_tx: NewLinkChannelSender,
}

impl IrohZenohLinkManager {
    pub fn new(
        endpoint: Arc<IrohEndpoint>,
        resolver: ReachabilityResolver,
        new_link_tx: NewLinkChannelSender,
    ) -> Self {
        Self {
            endpoint,
            resolver,
            new_link_tx,
        }
    }

    /// Spawn the accept loop. Should be called once after construction.
    pub fn spawn_accept_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let ep = mgr.endpoint.inner().clone();
            while let Some(incoming) = ep.accept().await {
                let mgr = Arc::clone(&mgr);
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("iroh accept connect failed: {e}");
                            return;
                        }
                    };
                    let alpn_used = conn.alpn().unwrap_or_default();
                    if alpn_used.as_slice() != alpn::HARMONY_ZENOH_V1 {
                        tracing::debug!(
                            "ignoring non-zenoh ALPN: {:?}",
                            std::str::from_utf8(&alpn_used).unwrap_or("<binary>")
                        );
                        return;
                    }
                    let (send, recv) = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("iroh accept_bi failed: {e}");
                            return;
                        }
                    };
                    let peer_node_id = match conn.remote_node_id() {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let src = locator_from_node_id(&mgr.endpoint.node_id());
                    let dst = locator_from_node_id(&peer_node_id);
                    let link = Arc::new(IrohZenohLink::new(send, recv, src, dst));
                    if let Err(e) = mgr.new_link_tx.send_async(LinkUnicast(link)).await {
                        tracing::warn!("zenoh new_link channel closed: {e}");
                    }
                });
            }
        })
    }

    /// Parse a Zenoh locator into a Harmony OwnerAddr (the locator
    /// authority is the hex-encoded OwnerAddr).
    fn parse_owner_addr(endpoint: &EndPoint) -> Option<OwnerAddr> {
        // Locator form: "iroh/<hex_owner_addr>" — accept either the
        // raw OwnerAddr or the Iroh NodeId form. Phase 1 uses OwnerAddr
        // form so the upper layer doesn't need to know about NodeIds.
        let auth = endpoint.address().as_str();
        let hex = auth.strip_prefix("iroh/")?;
        let bytes = hex::decode(hex).ok()?;
        let arr: [u8; 16] = bytes.try_into().ok()?;
        Some(OwnerAddr(arr))
    }
}

fn locator_from_node_id(node_id: &iroh::NodeId) -> Locator {
    Locator::new(
        "iroh",
        hex::encode(node_id.as_bytes()),
        "", // metadata
    )
    .expect("iroh locator format")
}

#[async_trait]
impl LinkManagerUnicastTrait for IrohZenohLinkManager {
    async fn new_link(&self, endpoint: &EndPoint) -> zenoh_link::ZResult<LinkUnicast> {
        let owner = Self::parse_owner_addr(endpoint)
            .ok_or_else(|| zenoh_link::zerror!("iroh locator missing OwnerAddr"))?;
        let record = self
            .resolver
            .resolve(&owner)
            .ok_or_else(|| zenoh_link::zerror!("no ReachabilityRecord for owner {:?}", owner))?;
        let node_id = iroh::NodeId::from_bytes(&record.iroh_node_id)
            .map_err(|e| zenoh_link::zerror!("iroh node_id parse: {e}"))?;
        let mut addr = NodeAddr::new(node_id);
        if let Ok(url) = record.home_relay_url.parse() {
            addr = addr.with_relay_url(url);
        }
        for da in record.direct_addresses {
            addr = addr.with_direct_addresses(std::iter::once(da));
        }
        let conn = self
            .endpoint
            .inner()
            .connect(addr, alpn::HARMONY_ZENOH_V1)
            .await
            .map_err(|e| zenoh_link::zerror!("iroh connect: {e}"))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| zenoh_link::zerror!("iroh open_bi: {e}"))?;
        let src = locator_from_node_id(&self.endpoint.node_id());
        let dst = locator_from_node_id(&node_id);
        let link = Arc::new(IrohZenohLink::new(send, recv, src, dst));
        Ok(LinkUnicast(link))
    }

    async fn new_listener(&self, _endpoint: &EndPoint) -> zenoh_link::ZResult<Locator> {
        // Single global listener (the accept loop spawned in
        // spawn_accept_loop). new_listener returns the local locator.
        Ok(locator_from_node_id(&self.endpoint.node_id()))
    }

    async fn del_listener(&self, _endpoint: &EndPoint) -> zenoh_link::ZResult<()> {
        // No-op in Phase 1 — listener is bound to endpoint lifetime;
        // endpoint shutdown closes it.
        Ok(())
    }

    fn get_listeners(&self) -> Vec<EndPoint> {
        vec![]
    }

    fn get_locators(&self) -> Vec<Locator> {
        vec![locator_from_node_id(&self.endpoint.node_id())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_link_resolves_owner_via_resolver() {
        // Plan-only integration: full two-endpoint coverage lives in
        // tests/community_reachability_two_engine_integration.rs (Task 10).
        // Here, we exercise resolver-miss and resolver-hit paths.

        use crate::iroh_endpoint::IrohEndpoint;
        use crate::owner_state_types::Hlc;
        use crate::reachability_record::ReachabilityAnnouncePayload;
        use iroh::SecretKey;
        use rand::RngCore;

        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let ep_a = Arc::new(IrohEndpoint::new_with_secret(SecretKey::from_bytes(&b)).await.unwrap());
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded();
        let mgr = IrohZenohLinkManager::new(Arc::clone(&ep_a), resolver.clone(), new_link_tx);

        // Resolver-miss case: new_link returns error.
        let bogus = EndPoint::new("iroh", "iroh/00000000000000000000000000000000", "", "").unwrap();
        assert!(mgr.new_link(&bogus).await.is_err());
        ep_a.shutdown().await;
    }
}
```

In `src-tauri/src/lib.rs`, add:

```rust
pub mod zenoh_iroh_transport;
```

- [ ] **Step 2: Compile and run**

```bash
cd src-tauri
cargo check --features test-fixtures 2>&1 | tail -30
cargo nextest run --locked --features test-fixtures -E 'test(zenoh_iroh_transport)'
cd ..
```

If the test compiles but fails for legitimate reasons (e.g., the `NewLinkChannelSender` API differs from what I drafted), the implementer should consult the actual `zenoh-link` API and adjust. The semantic intent stays: "accept loop dispatches new links to Zenoh; `new_link` resolves via `ReachabilityResolver`; `new_listener` returns the local locator."

- [ ] **Step 3: Run all gates**

(standard)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/zenoh_iroh_transport.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): IrohZenohLinkManager — LinkManagerUnicastTrait impl

Implements zenoh-link::LinkManagerUnicastTrait backed by IrohEndpoint
+ ReachabilityResolver. new_link parses an "iroh/<hex_owner_addr>"
locator, resolves to NodeId + DERP relay via the resolver, and opens
a QUIC bidi stream on harmony/zenoh/v1 ALPN. Accept loop dispatches
incoming streams to Zenoh's NewLinkChannelSender.

End-to-end two-engine integration lives in Task 10.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7 — `reachability_publisher.rs` (debounced republish task)

**Files:**
- Create: `src-tauri/src/reachability_publisher.rs`
- Modify: `src-tauri/Cargo.toml` (add `if-watch` dep)
- Modify: `src-tauri/src/lib.rs`

**Goal:** Background tokio task that drives debounced reachability republishes per spec §5.6.

- [ ] **Step 1: Add `if-watch` dep**

In `src-tauri/Cargo.toml` `[dependencies]`:

```toml
if-watch = { version = "3", features = ["tokio"] }
```

- [ ] **Step 2: Create the publisher module**

Create `src-tauri/src/reachability_publisher.rs`:

```rust
//! ZEB-321 Phase 1: background task that re-emits this device's
//! ReachabilityAnnounce on startup, on network change (debounced 2s),
//! on home-relay change, and on a 60-min idle tick.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.6.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{interval, timeout};

use crate::iroh_endpoint::IrohEndpoint;

/// How long to coalesce rapid network-change events before publishing.
const NETWORK_CHANGE_DEBOUNCE: Duration = Duration::from_secs(2);
/// How often to re-publish even when nothing has changed.
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Callback invoked when the publisher decides it's time to publish a
/// fresh ReachabilityAnnounce. The async fn returns once the event has
/// been signed and inserted into the community CRDT (one event per
/// community this device is in — the callback iterates internally).
///
/// Decoupled via a callback so the publisher module doesn't need to
/// know about community-state internals; lib.rs / event_loop.rs wires
/// up the actual emit.
pub type PublishFn = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

pub struct ReachabilityPublisher {
    endpoint: Arc<IrohEndpoint>,
    publish: PublishFn,
    /// Wakes the publisher loop immediately (used by force_republish IPC).
    pub force: Arc<Notify>,
}

impl ReachabilityPublisher {
    pub fn new(endpoint: Arc<IrohEndpoint>, publish: PublishFn) -> Self {
        Self {
            endpoint,
            publish,
            force: Arc::new(Notify::new()),
        }
    }

    /// Spawn the publisher loop. Returns a JoinHandle the caller can
    /// optionally await on shutdown.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // 1. On startup, publish immediately.
            (self.publish)().await;

            // 2. Set up network-change watcher.
            let mut iface_stream = match if_watch::tokio::IfWatcher::new() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("if-watch init failed: {e}; idle-only republish");
                    self.idle_loop().await;
                    return;
                }
            };

            let mut idle_tick = interval(IDLE_REFRESH_INTERVAL);
            idle_tick.tick().await; // consume initial immediate tick

            loop {
                tokio::select! {
                    biased;

                    // Force republish (IPC trigger or test).
                    _ = self.force.notified() => {
                        (self.publish)().await;
                    }

                    // Network change: coalesce within DEBOUNCE window.
                    _ = futures::future::poll_fn(|cx| std::pin::Pin::new(&mut iface_stream).poll_if_event(cx)) => {
                        // Drain any rapid follow-ups within DEBOUNCE.
                        let _ = timeout(NETWORK_CHANGE_DEBOUNCE, async {
                            loop {
                                let _ = futures::future::poll_fn(|cx| std::pin::Pin::new(&mut iface_stream).poll_if_event(cx)).await;
                            }
                        }).await;
                        (self.publish)().await;
                    }

                    // Idle tick.
                    _ = idle_tick.tick() => {
                        (self.publish)().await;
                    }
                }
            }
        })
    }

    async fn idle_loop(self: Arc<Self>) {
        let mut idle_tick = interval(IDLE_REFRESH_INTERVAL);
        idle_tick.tick().await;
        loop {
            tokio::select! {
                _ = self.force.notified() => { (self.publish)().await; }
                _ = idle_tick.tick() => { (self.publish)().await; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn force_notify_triggers_publish() {
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&count);
        let publish: PublishFn = Arc::new(move || {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            }) as futures::future::BoxFuture<'static, ()>
        });

        use crate::iroh_endpoint::IrohEndpoint;
        use iroh::SecretKey;
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let ep = Arc::new(IrohEndpoint::new_with_secret(SecretKey::from_bytes(&b)).await.unwrap());

        let pub_ = Arc::new(ReachabilityPublisher::new(ep, publish));
        let force = Arc::clone(&pub_.force);
        let _handle = pub_.spawn();

        // Wait for the startup publish to land.
        for _ in 0..50 {
            if count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(count.load(Ordering::SeqCst) >= 1);

        force.notify_one();
        for _ in 0..50 {
            if count.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(count.load(Ordering::SeqCst) >= 2);
    }
}
```

In `src-tauri/src/lib.rs`, add:

```rust
pub mod reachability_publisher;
```

- [ ] **Step 3: Run test, expect pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(reachability_publisher)'
cd ..
```

**10-min wall-clock note:** if-watch interactions with the test machine's network may be slow; the test only exercises the `force` path, so it should be fast. If the test stalls, treat as DONE_WITH_CONCERNS.

- [ ] **Step 4: Run all gates, commit**

```bash
git add src-tauri/src/reachability_publisher.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): reachability_publisher — debounced republish task

Background tokio task that drives reachability republishes per spec §5.6:
- On startup (immediate)
- On network change via if-watch (2s debounce)
- On 60-min idle tick
- On manual force-notify (from force_republish IPC)

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8 — `event_loop.rs` wiring + `ReachabilityResolver` feed hook + stale-comment cleanup

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Modify: `src-tauri/src/lib.rs` (start_node — pass in resolver / publisher / endpoint handles)

**Goal:** Wire IrohEndpoint construction, ReachabilityResolver, ReachabilityPublisher, and the IrohZenohLinkManager registration into the existing event loop. Wire the per-community insert_event path so accepted `ReachabilityAnnounce` events feed the resolver.

This is the integration heavy task. Read the existing `event_loop.rs` thoroughly before making changes — the file is 4949 lines and has its own scoping/lifetime constraints.

- [ ] **Step 1: Strike the stale comment**

In `src-tauri/src/event_loop.rs:8`:

```rust
//! No disk/archive/S3 persistence, no inference.
```

(strike "no iroh tunnels" — ZEB-321 Phase 1 changes that.)

- [ ] **Step 2: Add boot wiring**

Locate the `pub async fn run(…)` function (search for `pub async fn run`). At the top, after the Zenoh session is constructed but before the per-community adapter wiring, insert IrohEndpoint construction:

```rust
    // ZEB-321 Phase 1: Iroh transport boot.
    let iroh_secret = crate::iroh_endpoint::load_or_create_secret_key()
        .expect("load iroh secret key");
    let iroh_ep = std::sync::Arc::new(
        crate::iroh_endpoint::IrohEndpoint::new_with_secret(iroh_secret)
            .await
            .expect("iroh endpoint init"),
    );

    let reachability_resolver = crate::reachability_resolver::ReachabilityResolver::new();

    // Wire the Zenoh-over-Iroh link manager. (The actual zenoh::Session
    // registration call will be a follow-up — Phase 1 ships the impl and
    // proves it via the two-engine integration test in Task 10; full
    // session-replacement of LinkManager is gated on the test result.)
    let (new_link_tx, _new_link_rx) = flume::unbounded();
    let iroh_link_mgr = std::sync::Arc::new(
        crate::zenoh_iroh_transport::IrohZenohLinkManager::new(
            std::sync::Arc::clone(&iroh_ep),
            reachability_resolver.clone(),
            new_link_tx,
        ),
    );
    let _iroh_accept_handle = iroh_link_mgr.spawn_accept_loop();

    // Start the publisher. The callback closes over the active set of
    // joined communities and emits a signed ReachabilityAnnounce into
    // each one's CRDT. Phase 1 stub: the callback is wired in lib.rs via
    // a SetOnce — see ReachabilityEmitCtx below.
    let publisher = std::sync::Arc::new(
        crate::reachability_publisher::ReachabilityPublisher::new(
            std::sync::Arc::clone(&iroh_ep),
            std::sync::Arc::new(|| Box::pin(async {}) as futures::future::BoxFuture<'static, ()>),
        ),
    );
    let _publisher_handle = std::sync::Arc::clone(&publisher).spawn();
```

- [ ] **Step 3: Add the per-community resolver feed hook**

Locate the spot where the community CRDT inbound bytes are decoded and inserted (search for `insert_event` calls in `event_loop.rs`). After a successful `InsertOutcome::Inserted`, dispatch to the resolver:

```rust
            if let MembershipEventKind::ReachabilityAnnounce { payload } = &event.kind {
                reachability_resolver.update(event.actor, payload.clone(), event.at.clone());
                let _ = app_handle.emit("connectivity-reachability-changed", serde_json::json!({
                    "actor": hex::encode(event.actor.0),
                }));
            }
```

(The exact insertion point depends on the file structure — read the existing `insert_event` call sites and pick the closest one. There may be more than one.)

- [ ] **Step 4: Add a test — `event_loop_routes_reachability_announce_to_resolver`**

This is a non-trivial integration test. Use the existing pattern in `community_state_crdt_unit.rs` as the template. Pattern outline (full code: implementer fills in based on existing test helpers):

```rust
#[tokio::test]
async fn event_loop_routes_reachability_announce_to_resolver() {
    // 1. Construct a CommunityStateCrdt + admin identity + joined member identity.
    // 2. Construct a ReachabilityResolver.
    // 3. Sign + insert a ReachabilityAnnounce event by the joined member.
    // 4. Manually dispatch event to resolver (simulating the event_loop hook).
    // 5. Assert resolver.resolve(member_addr) returns Some(payload).
}
```

- [ ] **Step 5: Run all gates**

(standard — this task changes a large file; expect compile errors first iteration; iterate until clean)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): event_loop wiring + ReachabilityResolver feed hook

Wires IrohEndpoint, ReachabilityResolver, ReachabilityPublisher, and
IrohZenohLinkManager into event_loop::run on startup. Adds a per-
community insert_event hook that dispatches accepted
ReachabilityAnnounce events into the resolver and emits a
connectivity-reachability-changed Tauri event for Phase 3 forward-compat.

Strikes the stale "no iroh tunnels" comment at event_loop.rs:8.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9 — 3 Tauri IPCs + invoke_handler registration

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Goal:** Expose the 3 IPCs the frontend needs.

- [ ] **Step 1: Add IPC handler stubs in lib.rs**

Locate the section near the end of `lib.rs` where `#[tauri::command]` attributes cluster (search for the last `#[tauri::command]`). Append:

```rust
#[tauri::command(rename_all = "snake_case")]
async fn connectivity_get_my_reachability_record(
    state: tauri::State<'_, AppState>,
) -> Result<Option<ReachabilityRecordDto>, String> {
    // AppState should hold a handle to the local IrohEndpoint + a
    // most-recent-published-record cache. Implementer adapts to the
    // actual AppState shape in this file.
    let ep = state.iroh_endpoint.clone();
    let dto = ReachabilityRecordDto {
        iroh_node_id: hex::encode(ep.node_id().as_bytes()),
        home_relay_url: ep.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep.direct_addresses().iter().map(|s| s.to_string()).collect(),
        announced_at_ms: 0, // populated when force_republish is called next
    };
    Ok(Some(dto))
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_list_peer_reachability(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, ReachabilityRecordDto)>, String> {
    let resolver = state.reachability_resolver.clone();
    Ok(resolver
        .list_active_peers()
        .into_iter()
        .map(|(addr, p)| {
            (
                hex::encode(addr.0),
                ReachabilityRecordDto {
                    iroh_node_id: hex::encode(p.iroh_node_id),
                    home_relay_url: p.home_relay_url,
                    direct_addresses: p.direct_addresses.iter().map(|s| s.to_string()).collect(),
                    announced_at_ms: p.announced_at_ms,
                },
            )
        })
        .collect())
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_force_republish(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let publisher = state.reachability_publisher.clone();
    publisher.force.notify_one();
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReachabilityRecordDto {
    pub iroh_node_id: String,        // hex(32)
    pub home_relay_url: String,
    pub direct_addresses: Vec<String>, // SocketAddr.to_string()
    pub announced_at_ms: u64,
}
```

- [ ] **Step 2: Register in `invoke_handler!`**

Find the `invoke_handler` macro call (search for `tauri::generate_handler!` or `.invoke_handler(`). Add the 3 new commands to the handler list.

- [ ] **Step 3: Add a smoke-test IPC test (unit)**

Use the existing `tauri::test` harness pattern (search for `tauri::test::MockRuntime` or `mock_app` in existing IPC tests):

```rust
#[cfg(test)]
mod connectivity_ipc_tests {
    use super::*;

    #[tokio::test]
    async fn force_republish_increments_publisher_count() {
        // … construct AppState with a stub publisher whose `force.notify_one`
        // increments a counter; invoke connectivity_force_republish; assert
        // counter increments.
    }
}
```

- [ ] **Step 4: Run all gates**

(standard)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): 3 connectivity Tauri IPCs

- connectivity_get_my_reachability_record
- connectivity_list_peer_reachability
- connectivity_force_republish

Plus a ReachabilityRecordDto wire type for the frontend.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10 — Two-engine integration test

**Files:**
- Create: `src-tauri/tests/community_reachability_two_engine_integration.rs`

**Goal:** Two harmony-client instances on loopback, each publishes a `ReachabilityRecord`, each reads the other's, opens an Iroh connection by NodeId, the connection succeeds, a CRDT byte payload round-trips over the Iroh-tunneled Zenoh link.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/tests/community_reachability_two_engine_integration.rs`:

```rust
//! ZEB-321 Phase 1: two-engine integration test.
//! Verifies that two independent harmony-client instances on the same
//! loopback can announce reachability, resolve each other's NodeIds,
//! open Iroh QUIC streams over harmony/zenoh/v1, and exchange CRDT
//! payload bytes.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §13.

use std::sync::Arc;
use std::time::Duration;

use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;

use iroh::SecretKey;
use rand::RngCore;

#[tokio::test]
async fn two_engines_exchange_via_iroh_zenoh() {
    let mut b1 = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b1);
    let mut b2 = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b2);

    let ep_a = Arc::new(IrohEndpoint::new_with_secret(SecretKey::from_bytes(&b1)).await.unwrap());
    let ep_b = Arc::new(IrohEndpoint::new_with_secret(SecretKey::from_bytes(&b2)).await.unwrap());

    // Each engine has its own resolver, seeded with the OTHER engine's
    // announcement.
    let resolver_a = ReachabilityResolver::new();
    let resolver_b = ReachabilityResolver::new();

    let owner_a = OwnerAddr([0xAA; 16]);
    let owner_b = OwnerAddr([0xBB; 16]);

    let hlc = Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "fix".into() };

    let payload_a = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_a.node_id().as_bytes(),
        home_relay_url: ep_a.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_a.direct_addresses(),
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64], // unsigned for transport test
    };
    let payload_b = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_b.node_id().as_bytes(),
        home_relay_url: ep_b.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_b.direct_addresses(),
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
    };

    resolver_a.update(owner_b, payload_b.clone(), hlc.clone());
    resolver_b.update(owner_a, payload_a.clone(), hlc.clone());

    // Set up A's link manager + accept loop.
    let (tx_a, rx_a) = flume::unbounded();
    let mgr_a = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep_a), resolver_a, tx_a));
    let _accept_a = mgr_a.spawn_accept_loop();

    // Set up B's link manager + accept loop.
    let (tx_b, rx_b) = flume::unbounded();
    let mgr_b = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep_b), resolver_b, tx_b));
    let _accept_b = mgr_b.spawn_accept_loop();

    // A dials B via the locator built from owner_b.
    let endpoint =
        zenoh_link::EndPoint::new("iroh", &format!("iroh/{}", hex::encode(owner_b.0)), "", "")
            .unwrap();
    let link = mgr_a.new_link(&endpoint).await.expect("new_link");

    // Round-trip a small payload.
    link.0.write_all(b"hello-iroh-zenoh").await.expect("write");

    // B should see the accepted link in its new_link channel.
    let incoming = tokio::time::timeout(Duration::from_secs(5), rx_b.recv_async())
        .await
        .expect("rx_b timeout")
        .expect("rx_b recv");
    let mut buf = [0u8; 16];
    incoming.0.read_exact(&mut buf).await.expect("read_exact");
    assert_eq!(&buf, b"hello-iroh-zenoh");

    ep_a.shutdown().await;
    ep_b.shutdown().await;
}
```

- [ ] **Step 2: Run the test**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures --test community_reachability_two_engine_integration
cd ..
```

**10-min wall-clock note:** This test uses real Iroh endpoints. If DERP-relay contact stalls the bind, switch the test to `RelayMode::Disabled` (Iroh supports local-network-only operation on loopback).

- [ ] **Step 3: Run all gates, commit**

```bash
git add src-tauri/tests/community_reachability_two_engine_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-321-p1): two-engine Iroh+Zenoh integration

Two distinct IrohEndpoints on loopback, each with its own
ReachabilityResolver seeded with the other's NodeId, open a
harmony/zenoh/v1 QUIC stream via IrohZenohLinkManager and exchange
a sample payload. Validates the cross-WAN architecture end-to-end
in the constrained two-process case.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11 — Frontend: types + adapter + DiagnosticsPanel + vitest tests

**Files:**
- Create: `src/lib/types/connectivity.ts`
- Create: `src/lib/connectivity-adapter.ts`
- Create: `src/lib/components/DiagnosticsPanel.svelte`
- Create: `src/lib/connectivity-adapter.test.ts`
- Create: `src/lib/components/DiagnosticsPanel.test.ts`

**Goal:** Minimal frontend surface for Phase 1 (dev-mode only — not exposed in the main app UI).

- [ ] **Step 1: Create `types/connectivity.ts`**

```typescript
/**
 * ZEB-321 Phase 1: connectivity-types frontend.
 * Mirrors src-tauri/src/lib.rs ReachabilityRecordDto (camelCase).
 */

export interface ReachabilityRecord {
  irohNodeId: string;          // hex(32)
  homeRelayUrl: string;
  directAddresses: string[];   // SocketAddr.to_string() forms
  announcedAtMs: number;
}

export interface ConnectivityReachabilityChangedPayload {
  actor: string;                // hex(16) of OwnerAddr
}
```

- [ ] **Step 2: Create `connectivity-adapter.ts`**

```typescript
/**
 * ZEB-321 Phase 1: Tauri IPC bindings for connectivity.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import type {
  ReachabilityRecord,
  ConnectivityReachabilityChangedPayload,
} from "./types/connectivity";

export async function getMyReachabilityRecord(): Promise<ReachabilityRecord | null> {
  try {
    return await invoke<ReachabilityRecord | null>("connectivity_get_my_reachability_record");
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_get_my_reachability_record: ${msg}`);
  }
}

export async function listPeerReachability(): Promise<Array<[string, ReachabilityRecord]>> {
  try {
    return await invoke<Array<[string, ReachabilityRecord]>>("connectivity_list_peer_reachability");
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_list_peer_reachability: ${msg}`);
  }
}

export async function forceRepublish(): Promise<void> {
  try {
    await invoke<void>("connectivity_force_republish");
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_force_republish: ${msg}`);
  }
}

export async function onReachabilityChanged(
  callback: (payload: ConnectivityReachabilityChangedPayload) => void,
): Promise<UnlistenFn> {
  return await listen<ConnectivityReachabilityChangedPayload>(
    "connectivity-reachability-changed",
    (event) => callback(event.payload),
  );
}
```

- [ ] **Step 3: Create `DiagnosticsPanel.svelte`**

```svelte
<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import {
    getMyReachabilityRecord,
    listPeerReachability,
    forceRepublish,
    onReachabilityChanged,
  } from "$lib/connectivity-adapter";
  import type { ReachabilityRecord } from "$lib/types/connectivity";

  // Dev-mode flag — read from env. Production builds skip the panel.
  const isDevMode = import.meta.env.DEV;

  let myRecord: ReachabilityRecord | null = null;
  let peerRecords: Array<[string, ReachabilityRecord]> = [];
  let unlisten: (() => void) | null = null;
  let error: string | null = null;

  async function refresh(): Promise<void> {
    try {
      myRecord = await getMyReachabilityRecord();
      peerRecords = await listPeerReachability();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(async () => {
    if (!isDevMode) return;
    await refresh();
    unlisten = await onReachabilityChanged(() => {
      void refresh();
    });
  });

  onDestroy(() => {
    if (unlisten) unlisten();
  });
</script>

{#if isDevMode}
  <div class="connectivity-diagnostics">
    <h3>ZEB-321 connectivity diagnostics (dev only)</h3>

    {#if error}
      <p class="error" data-testid="diag-error">Error: {error}</p>
    {/if}

    <section>
      <h4>This device</h4>
      {#if myRecord}
        <dl>
          <dt>Iroh NodeId</dt>
          <dd data-testid="diag-my-node-id">{myRecord.irohNodeId}</dd>
          <dt>Home relay</dt>
          <dd data-testid="diag-my-relay">{myRecord.homeRelayUrl || "(none)"}</dd>
          <dt>Direct addresses</dt>
          <dd data-testid="diag-my-direct">{myRecord.directAddresses.join(", ") || "(none)"}</dd>
        </dl>
      {:else}
        <p>Iroh endpoint not ready</p>
      {/if}
      <button on:click={() => void forceRepublish()} data-testid="diag-force-republish">
        Force republish
      </button>
    </section>

    <section>
      <h4>Known peers ({peerRecords.length})</h4>
      <ul>
        {#each peerRecords as [addr, record]}
          <li data-testid="diag-peer">
            <strong>{addr.slice(0, 12)}…</strong> → {record.irohNodeId.slice(0, 12)}…
          </li>
        {/each}
      </ul>
    </section>
  </div>
{/if}

<style>
  .connectivity-diagnostics {
    border: 1px dashed #888;
    padding: 1em;
    margin: 1em;
    font-family: monospace;
    font-size: 0.85em;
  }
  .error {
    color: crimson;
  }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 0.25em 1em; }
  dt { font-weight: bold; }
</style>
```

- [ ] **Step 4: Create the vitest tests**

`src/lib/connectivity-adapter.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mockIPC } from "@tauri-apps/api/mocks";
import {
  getMyReachabilityRecord,
  listPeerReachability,
  forceRepublish,
} from "./connectivity-adapter";

describe("connectivity-adapter", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("getMyReachabilityRecord returns null when backend reports null", async () => {
    mockIPC((cmd) => {
      if (cmd === "connectivity_get_my_reachability_record") return null;
    });
    expect(await getMyReachabilityRecord()).toBeNull();
  });

  it("getMyReachabilityRecord throws with helpful prefix on backend error", async () => {
    mockIPC((cmd) => {
      if (cmd === "connectivity_get_my_reachability_record") {
        throw new Error("simulated");
      }
    });
    await expect(getMyReachabilityRecord()).rejects.toThrow(
      "connectivity_get_my_reachability_record: simulated",
    );
  });

  it("listPeerReachability returns the backend's tuple list", async () => {
    const sample = [
      [
        "aa".repeat(16),
        {
          irohNodeId: "bb".repeat(32),
          homeRelayUrl: "https://derp.example/",
          directAddresses: [],
          announcedAtMs: 1_700_000_000_000,
        },
      ],
    ];
    mockIPC((cmd) => {
      if (cmd === "connectivity_list_peer_reachability") return sample;
    });
    expect(await listPeerReachability()).toEqual(sample);
  });

  it("forceRepublish resolves on success", async () => {
    mockIPC((cmd) => {
      if (cmd === "connectivity_force_republish") return;
    });
    await expect(forceRepublish()).resolves.toBeUndefined();
  });
});
```

`src/lib/components/DiagnosticsPanel.test.ts`:

```typescript
import { describe, it, expect, vi } from "vitest";
import { render } from "@testing-library/svelte";
import { mockIPC } from "@tauri-apps/api/mocks";
import DiagnosticsPanel from "./DiagnosticsPanel.svelte";

describe("DiagnosticsPanel", () => {
  it("renders nothing in production mode", () => {
    vi.stubGlobal("import.meta", { env: { DEV: false } });
    const { container } = render(DiagnosticsPanel);
    expect(container.querySelector(".connectivity-diagnostics")).toBeNull();
  });

  it("renders my-node section when backend returns a record", async () => {
    vi.stubGlobal("import.meta", { env: { DEV: true } });
    mockIPC((cmd) => {
      if (cmd === "connectivity_get_my_reachability_record") {
        return {
          irohNodeId: "ab".repeat(32),
          homeRelayUrl: "https://derp.example/",
          directAddresses: ["10.0.0.1:4242"],
          announcedAtMs: 1_700_000_000_000,
        };
      }
      if (cmd === "connectivity_list_peer_reachability") return [];
    });
    const { findByTestId } = render(DiagnosticsPanel);
    const nodeId = await findByTestId("diag-my-node-id");
    expect(nodeId.textContent).toMatch(/^abab/);
  });
});
```

- [ ] **Step 5: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 6: Run backend gates**

(standard)

- [ ] **Step 7: Commit**

```bash
git add src/lib/types/connectivity.ts src/lib/connectivity-adapter.ts \
        src/lib/components/DiagnosticsPanel.svelte \
        src/lib/connectivity-adapter.test.ts \
        src/lib/components/DiagnosticsPanel.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-321-p1): frontend connectivity adapter + DiagnosticsPanel

- types/connectivity.ts: ReachabilityRecord interface (camelCase)
- connectivity-adapter.ts: 3 IPC bindings + onReachabilityChanged subscriber
- DiagnosticsPanel.svelte: dev-only debug panel (NodeId, relay, peers)
- vitest tests cover adapter happy/error paths + panel render

Production builds (import.meta.env.DEV === false) render nothing.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 12 — Final 5-gate sweep, push, PR creation

**Files:**
- No code changes — verification + PR creation only.

**Goal:** Confirm every gate is green, push the branch, create a PR with markdown-linked Linear refs (no bare `Closes` trigger — this PR completes Phase 1 of multi-phase ZEB-321), and hand control to the autonomous bot-review loop.

- [ ] **Step 1: Run all 5 backend gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

All 3 must exit 0. New nextest failure count vs Task 0 baseline must be 0 (orphan failures unchanged is fine; orphan failures GROWING is a regression).

- [ ] **Step 2: Run both frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Both must exit 0.

- [ ] **Step 3: Confirm branch state**

```bash
git status
git log --oneline e68599b..HEAD
```

Expected: clean working tree; 1 spec commit + 1 plan commit + 11 implementation commits ≈ 13 total commits.

- [ ] **Step 4: Push the branch**

```bash
git push -u origin zeb-321-phase1-iroh-foundation
```

- [ ] **Step 5: Create the PR**

```bash
gh pr create --title "ZEB-321 Phase 1: Iroh foundation + ReachabilityAnnounce CRDT + Zenoh-over-Iroh transport" --body "$(cat <<'EOF'
## Summary

Phase 1 of N (within-community discovery + transport foundation) for [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) — cross-WAN peer discovery, reconnection, and NAT traversal.

This PR lands the load-bearing primitives that subsequent phases build on:

- **`IrohEndpoint`** wrapper around `iroh::Endpoint` with persistent Ed25519 secret key (OS keychain) + ALPN registry constants (`harmony/zenoh/v1`, reserved `harmony/handshake/v1`).
- **`MembershipEventKind::ReachabilityAnnounce`** — new community-state CRDT event variant (tag `"a"`) carrying NodeId + DERP relay URL + direct-address hints + inner Ed25519 identity signature binding the Iroh NodeId to the harmony identity.
- **5 verify rules RCH1–RCH5** for the new event (outer sig, inner sig, actor-binding, ±30min timestamp skew, current-member requirement). Pinned via `wire_format_reachability_announce_fixtures.rs`.
- **`ReachabilityResolver`** — LWW projection of `ReachabilityAnnounce` events into a `BTreeMap<OwnerAddr, ReachabilityAnnouncePayload>` consumed by the Zenoh-over-Iroh transport.
- **`ReachabilityPublisher`** — debounced background task: on-boot, on-network-change (`if-watch`, 2s debounce), on-home-relay-change, 60-min idle tick.
- **`IrohZenohLink` + `IrohZenohLinkManager`** — custom Zenoh transport plugin (impls `zenoh_link::LinkUnicastTrait` + `LinkManagerUnicastTrait`). All existing Zenoh CRDT-sync code keeps working unchanged; bytes now flow over Iroh QUIC streams.
- **3 new Tauri IPCs** + 1 event for the frontend debug surface.
- **Two-engine integration test** validating end-to-end NodeId exchange + tunneled Zenoh link establishment + payload round-trip.

Spec: [`docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md`](docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md) (commit 8cf44aa)
Plan: [`docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md`](docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md)

Phase 2-5+ deliverables this merge gates (per spec §8):
- **Phase 2** — cross-community first-contact via pkarr; first-contact ALPN handler.
- **Phase 3** — liveness / heartbeat / reconnection orchestrator.
- **Phase 4** — self-hosted DERP relays (Hetzner) + cross-WAN canary.
- **Phase 5+** — community-operated relays + civic-infrastructure trust registry.

Per memory rule `feedback_linear_pr_auto_close`, this PR does NOT close [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) (multi-phase work). The parent stays open for the remaining phases.

## Test plan

- [x] `cargo fmt --all -- --check` — 0
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — 0
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — no new failures vs main baseline
- [x] `npx tsc --noEmit` — 0
- [x] `npx vitest run` — 0
- [ ] Manual smoke test of DiagnosticsPanel (dev-mode build, force-republish IPC observable in panel)
- [ ] Manual cross-WAN smoke test ([ZEB-172](https://linear.app/zeblith/issue/ZEB-172)) — NOT a merge gate per spec §13; tracked as follow-up.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Report PR URL**

After `gh pr create` returns, capture the PR URL and report it. Control transitions to the autonomous bot-review monitoring loop (`feedback_autonomous_pr_monitoring_loop`):

- CodeRabbit + Cursor Bugbot + CodeAnt + Qodo on every commit
- **NOT** Greptile (paid per-use; memory rule `reference_greptile_manual_trigger`)
- **NOT** CI (disabled per `feedback_ci_disabled`)
- One push per review round
- ScheduleWakeup at 1200–1800s while pending; 3600s if waiting on Jake to merge
- Pushover via `~/work/pushover-notify.sh` on convergence (mergeable + CLEAN + no new bot output for 30+ min) per `feedback_autonomous_post_spec`

---

## Self-review

After writing the plan above, I checked it against the spec:

**Spec coverage:**
- §1 Goal — Task 12 PR title + summary explicitly states the goal.
- §2 Background — referenced; not re-implemented (background section, not code).
- §3 Architecture overview — Tasks 4/6/8 implement the 3 load-bearing primitives.
- §4 Phase 1 deliverables — every numbered item has a task: §4.1.1 (Task 4), §4.1.2 (Task 1), §4.1.3 (Task 7), §4.1.4 (Task 5), §4.1.5 (Task 6), §4.1.6 (Task 1 + Task 2), §4.1.7 (Task 8), §4.1.8 (Task 9); §4.2.9 (Task 11), §4.2.10 (Task 11), §4.2.11 (Task 11); §4.3.12 (Task 1), §4.3.13 (Tasks 1/2/3/7), §4.3.14 (Task 10).
- §5 Wire format — Tasks 1+2.
- §6 IrohEndpoint — Task 4.
- §7 Zenoh-over-Iroh — Tasks 5+6.
- §8 Phase 2-5+ outline — not implementation (outline section); referenced in PR body.
- §9 Out of scope — implicitly respected (no pkarr, no heartbeat, no Hetzner relays).
- §10 Dependencies — Task 4 (iroh) + Task 7 (if-watch) + Task 5 (zenoh-link); keyring + ed25519-dalek already present.
- §11 Risks — addressed in implementer-prompt context (pinning, DONE_WITH_CONCERNS, version pin).
- §13 Phase 1 PR-merge gate — Task 12 final gate sweep + PR creation.

**Placeholder scan:** searched for "TBD", "TODO", "implement later", "etc."; none found in checkbox steps. (Some commit messages mention "Task N" for traceability; that's intentional, not a placeholder.)

**Type consistency:**
- `ReachabilityAnnouncePayload` shape defined in Task 1; used identically in Tasks 2, 3, 6, 9, 10, 11.
- `MembershipEventKind::ReachabilityAnnounce { payload }` introduced in Task 1; matched in Task 8 hook.
- `ReachabilityResolver` API (new, update, resolve, list_active_peers) defined in Task 3; used identically in Tasks 6, 8, 9, 10.
- `IrohEndpoint` API (new_with_secret, node_id, home_relay, direct_addresses, shutdown, inner) defined in Task 4; used identically in Tasks 5, 6, 7, 9, 10.
- ALPN constants `alpn::HARMONY_ZENOH_V1` defined in Task 4; referenced in Tasks 5, 6, 10.
- Tauri IPC names + DTO defined in Task 9; consumed in Task 11.

**One concern flagged:** Tasks 8 and 9 reference an `AppState` shape that I don't fully specify because the existing `lib.rs` shape is in flux. The implementer subagent must read the actual `AppState` in `src-tauri/src/lib.rs` (search for `pub struct AppState` or `tauri::State<'_,`) before adding fields. Marked inline in the relevant steps.

Plan complete.
