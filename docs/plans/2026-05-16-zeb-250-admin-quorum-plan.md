# ZEB-250 — M-of-N Admin Quorum Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land M-of-N admin governance for Sub-C v1 communities by generalizing ZEB-254's PendingJoin+JoinCountersign pattern into AdminProposal+AdminCountersign — including a `admin_quorum` field on `CommunityState` / `MaterializedMembership`, two new CRDT variants with verify+materialize semantics, modified verify gates on direct `SetPower`/`Kick`, an auto-routing IPC layer, and a new admin-only `PendingAdminProposalsPanel` + `ChangeQuorumDialog`.

**Architecture:** Accretive countersignatures — proposer self-signs (counts as 1), then `admin_quorum - 1` `AdminCountersign` events lift the proposal to "effective". 30-day expiry. Materialize uses **single-pass-with-running-state**: the pre-pass collects raw per-proposal signatures; the main pass walks events in HLC order maintaining a running `admin_quorum` so a quorum-reached `ChangeQuorum` mutates the threshold mid-iteration. Forking (ZEB-285) remains the universal escape-hatch for stuck quorums.

**Tech Stack:** Rust 1.x (`src-tauri/`), Tauri IPC, Svelte 5 (`src/`), `ciborium` for canonical CBOR, `cargo nextest` + `vitest`, `Ed25519` signatures via `ed25519-dalek`. Branch `zeb-250-admin-quorum` is already cut from `origin/main` at `91cb3b2`; spec at HEAD (`c1d73cd`).

**Spec:** `docs/specs/2026-05-16-zeb-250-admin-quorum-design.md`

---

## Reference: file map

| File | Role |
|---|---|
| `src-tauri/src/community_membership.rs` | Hosts `MembershipEventKind`, `MaterializedMembership`, `VerifyError`, `materialize`, `verify_event`, `POWER_THRESHOLDS`. Primary surface for new variants + verify + materialize work. |
| `src-tauri/src/community_state_crdt.rs` | Hosts `CommunityState` + `InsertOutcome`. Receives `admin_quorum` field. |
| `src-tauri/src/lib.rs` | Hosts Tauri IPC commands. Receives auto-routing in `set_power_level` / `kick_from_community`, plus three new commands. |
| `src-tauri/tests/wire_format_zeb250_fixtures.rs` | NEW. Byte-pinned canonical-CBOR fixtures for the three new variants + the new field. |
| `src-tauri/tests/community_admin_quorum_integration.rs` | NEW. End-to-end multi-engine scenarios. |
| `src/lib/components/PendingAdminProposalsPanel.svelte` | NEW. Admin-only panel mounted in CommunitySettingsPanel. |
| `src/lib/components/ChangeQuorumDialog.svelte` | NEW. Slider+number-input dialog for raising/lowering quorum. |
| `src/lib/components/CommunitySettingsPanel.svelte` | Add "Admin governance" section + member-list badges. |
| `src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts` | NEW. Vitest spec for the new panel. |
| `src/lib/components/__tests__/ChangeQuorumDialog.test.ts` | NEW. Vitest spec for the new dialog. |
| `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` | Augment with admin-governance section coverage. |

---

## Verification commands (used throughout)

Every implementer task ends with these five gates. Run them in this order — `cargo fmt` first (cheap, formats), then `clippy` (catches lints), then `nextest` (correctness), then frontend tsc + vitest.

Run from repo root:

```bash
# Rust gates (run from src-tauri/)
( cd src-tauri && cargo fmt --all -- --check ) && \
( cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings ) && \
( cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures ) && \
# Frontend gates (run from repo root)
npx tsc --noEmit && \
npx vitest run
```

**Pipe exit codes lie** — never use `cmd | tail/grep` to check pass/fail. If you must page output, use `set -o pipefail` first or check `${PIPESTATUS[0]}`.

---

## Task 0: Pre-flight verification (no commit)

**Files:** none modified — verification only.

- [ ] **Step 1: Confirm branch state**

```bash
git status
git log --oneline -3
```

Expected: branch `zeb-250-admin-quorum`, HEAD `c1d73cd docs(zeb-250): M-of-N admin quorum design spec`, working tree clean.

- [ ] **Step 2: Confirm green baseline**

Run all five CI gates (commands block above). Expected: all pass.

Cargo nextest test count after ZEB-287 merge: 1401. Vitest count: 1755. Record actual numbers; these are the baseline for Task 17.

- [ ] **Step 3: Skim spec section anchors**

Open `docs/specs/2026-05-16-zeb-250-admin-quorum-design.md`. Quickly locate:

- §3.1 — `CommunityState.admin_quorum` (CBOR `aq`)
- §3.2-3.4 — `AdminProposal` / `ProposalKind` / `AdminCountersign` variant definitions
- §3.5 — variant tag table
- §4.1-4.6 — verify gates AP1-AP5 + AC1-AC3 + modified SetPower/Kick gates
- §5.1-5.2 — pre-pass + main-pass-with-running-state algorithm
- §6.1-6.4 — IPC shapes (`AdminActionResult` + three new commands)
- §7.1-7.5 — UI surface (panel + dialog + badges)

No commit. Task 0 is a checkpoint.

---

## Task 1: New CRDT variants (`AdminProposal`, `ProposalKind`, `AdminCountersign`)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — extend `MembershipEventKind` + add `ProposalKind`.
- Modify: `src-tauri/src/community_membership.rs` — update variant-tag comment block (if any inline reference table exists).

**Background:** §3.2 / §3.3 / §3.4 of the spec. Variant tags `q` (AdminProposal) and `n` (AdminCountersign). `ProposalKind` is a 3-arm enum using `#[serde(tag = "kd", content = "bd")]`.

- [ ] **Step 1: Read current `MembershipEventKind` definition**

Read lines ~135-230 of `src-tauri/src/community_membership.rs` to locate where existing variants end (you'll need to know the exact insertion site).

- [ ] **Step 2: Add `ProposalKind` enum above `MembershipEventKind`**

Add at the spot just before `pub enum MembershipEventKind {` (after the surrounding type imports):

```rust
/// ZEB-250: shape of the proposed admin-affecting action wrapped by
/// [`MembershipEventKind::AdminProposal`]. Mirrors existing
/// single-signed event variants but gated through M-of-N quorum
/// approval.
///
/// Same-length-keys invariant: 1-char variant tags (`s`/`k`/`c`),
/// 2-char inner-field keys. Tagged-union representation with `kd`
/// (kind) discriminator + `bd` (body) container so the CBOR encoding
/// has explicit discriminator + body keys at the ProposalKind level.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kd", content = "bd")]
pub enum ProposalKind {
    /// SetPower whose target IS currently an admin (level was 100) OR
    /// whose new level IS 100 (promoting to admin).
    #[serde(rename = "s")]
    SetPower {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "lv")]
        level: u8,
    },
    /// Kick of a target who is currently an admin (level == 100).
    #[serde(rename = "k")]
    Kick {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// Change `CommunityState.admin_quorum`. `new_quorum >= 1`,
    /// practical cap enforced at verify_event AP5.
    #[serde(rename = "c")]
    ChangeQuorum {
        #[serde(rename = "nq")]
        new_quorum: u8,
    },
}
```

- [ ] **Step 3: Add new arms to `MembershipEventKind`**

Locate the end of `MembershipEventKind`'s arm list (just before the closing `}` of the enum). Add:

```rust
    /// ZEB-250: a power-100 admin proposes an admin-affecting action.
    /// Becomes effective only when the proposal accumulates >=
    /// admin_quorum total admin signatures (proposer counts as 1;
    /// remainder come from AdminCountersign events targeting this
    /// event_id).
    ///
    /// 30-day expiry: if quorum isn't reached within 30 days of the
    /// proposal's HLC wall_ms, the proposal is dead (pure-function
    /// check at materialize time). Late countersigns to expired
    /// proposals are no-ops.
    ///
    /// Variant tag "q" (1-char value, lowercase, unused before this).
    /// Inner field key "pk" (proposal_kind) per same-length-keys
    /// invariant.
    #[serde(rename = "q")]
    AdminProposal {
        #[serde(rename = "pk")]
        proposal_kind: ProposalKind,
    },

    /// ZEB-250: admin-tier countersignature on a target AdminProposal.
    /// Lenient forward-ref — verify_event doesn't require target to be
    /// present yet. Pairing happens at materialize time.
    ///
    /// Variant tag "n" (1-char value, lowercase, unused before this).
    #[serde(rename = "n")]
    AdminCountersign {
        #[serde(rename = "ti")]
        target_event_id: EventId,
    },
```

- [ ] **Step 4: Add minimal verify shim returning a placeholder error**

We need `cargo check` to compile before we write the full verify/materialize logic. Add a temporary catch-all in `verify_event` to silence the non-exhaustive-match warning, and a no-op in `materialize`.

In `verify_event` (locate it around line 1787), find the existing match block on `event.kind` and add — at the END, just before the closing `}` of the match — a placeholder arm:

```rust
        MembershipEventKind::AdminProposal { .. } | MembershipEventKind::AdminCountersign { .. } => {
            // ZEB-250 Task 4 / Task 5 will replace this stub with the
            // real verify gates.
            Ok(())
        }
```

Similarly in `materialize` (line 1034) and `prior_state_at_event` (look for the inner match block), add the same catch-all stub.

The intent is: Task 1 commits get the variants compiling cleanly. Subsequent tasks layer in verify + materialize semantics one piece at a time.

- [ ] **Step 5: Verify compilation**

```bash
cd src-tauri && cargo check --features test-fixtures
```

Expected: no errors. Warnings about unused `target` / `level` / `new_quorum` fields are acceptable (Task 4/8 will use them).

- [ ] **Step 6: Run round-trip unit test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `community_membership.rs`:

```rust
    #[test]
    fn admin_proposal_setpower_roundtrip() {
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::SetPower {
                target: OwnerAddr([0x11; 16]),
                level: 100,
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_proposal_kick_roundtrip() {
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::Kick {
                target: OwnerAddr([0x22; 16]),
                reason: Some("breach".to_string()),
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_proposal_change_quorum_roundtrip() {
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_countersign_roundtrip() {
        let kind = MembershipEventKind::AdminCountersign {
            target_event_id: [0x33; 16],
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }
```

Make sure `canonical_cbor_encode` is in scope — check the `use` statements at the top of the test module; add `use crate::owner_state_crypto::canonical_cbor_encode;` if needed.

- [ ] **Step 7: Run the new tests**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(admin_proposal_) + test(admin_countersign_)'
```

Expected: all four tests pass.

- [ ] **Step 8: Run full gates**

Run the five CI gates from the verification commands block at the top.

Expected: all green. Baseline test count grows by 4.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): add AdminProposal + ProposalKind + AdminCountersign CRDT variants

Wire-form scaffolding for ZEB-250's M-of-N admin quorum. Tags 'q' and
'n' (per spec §3.5). ProposalKind nested enum tagged with 'kd'/'bd'.
Verify + materialize stubs land in subsequent tasks.

Spec: docs/specs/2026-05-16-zeb-250-admin-quorum-design.md (commit
c1d73cd, §3.2-§3.4)."
```

---

## Task 2: Byte-pinned wire-format fixtures (`tests/wire_format_zeb250_fixtures.rs`)

**Files:**
- Create: `src-tauri/tests/wire_format_zeb250_fixtures.rs`

**Background:** Mirror `tests/wire_format_zeb254_fixtures.rs` (read it first; it's the closest precedent). Deterministic byte values; tests pin BYTE LAYOUT only (no crypto validity).

Five fixtures, per spec §8.1:

1. `admin_proposal_setpower_canonical_cbor`
2. `admin_proposal_kick_canonical_cbor`
3. `admin_proposal_change_quorum_canonical_cbor`
4. `admin_countersign_canonical_cbor`
5. `community_state_with_admin_quorum_canonical_cbor`
6. `community_state_default_quorum_omits_aq_key`

Fixtures 5/6 depend on `CommunityState.admin_quorum` existing — they'll be added in Task 3. In this task, write fixtures 1–4.

- [ ] **Step 1: Create the fixture file scaffold**

```rust
//! ZEB-250: Byte-pinned canonical CBOR fixtures for AdminProposal,
//! ProposalKind, AdminCountersign, CommunityState.admin_quorum.
//!
//! These tests lock the canonical-CBOR wire encoding for the new
//! ZEB-250 types. Any failure here is a wire-protocol break — review
//! carefully before updating the pinned bytes (cross-version compat,
//! peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte values) so the
//! encoded bytes are byte-stable across runs. The tests do NOT verify
//! cryptographic validity — they pin BYTE LAYOUT only.

use harmony_app::community_membership::{MembershipEventKind, ProposalKind};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::OwnerAddr;

const FIXTURE_TARGET_ADDR: OwnerAddr = OwnerAddr([0x11; 16]);
const FIXTURE_PROPOSER_ADDR: OwnerAddr = OwnerAddr([0x22; 16]);
const FIXTURE_TARGET_EVENT_ID: [u8; 16] = [0x66; 16];

// EXPECTED_*_HEX constants are populated by running the test once with
// "FILL_AFTER" as the value; the panic message prints the actual hex
// to paste back in. Regen-on-first-run pattern from ZEB-254.

const EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX: &str = "FILL_AFTER";
const EXPECTED_ADMIN_PROPOSAL_KICK_HEX: &str = "FILL_AFTER";
const EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX: &str = "FILL_AFTER";
const EXPECTED_ADMIN_COUNTERSIGN_HEX: &str = "FILL_AFTER";

#[test]
fn admin_proposal_setpower_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::SetPower {
            target: FIXTURE_TARGET_ADDR,
            level: 100,
        },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX,
        "AdminProposal+SetPower wire format changed"
    );

    // Structural sanity: top-level keys are tg+lv inside the bd
    // container, with kd+bd at the ProposalKind level, with tg+pk
    // wrapping at the MembershipEventKind level.
    let value: ciborium::Value =
        ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let outer_map = value.as_map().expect("outer is map");
    assert!(
        outer_map
            .iter()
            .any(|(k, _)| k.as_text() == Some("tg")),
        "outer envelope missing 'tg' (MembershipEventKind tag key)"
    );
    assert!(
        outer_map
            .iter()
            .any(|(k, _)| k.as_text() == Some("pk")),
        "outer envelope missing 'pk' (proposal_kind container)"
    );
}

#[test]
fn admin_proposal_kick_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::Kick {
            target: FIXTURE_TARGET_ADDR,
            reason: Some("violated rules".to_string()),
        },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_KICK_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_KICK_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_KICK_HEX,
        "AdminProposal+Kick wire format changed"
    );
}

#[test]
fn admin_proposal_change_quorum_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX,
        "AdminProposal+ChangeQuorum wire format changed"
    );
}

#[test]
fn admin_countersign_canonical_cbor() {
    let kind = MembershipEventKind::AdminCountersign {
        target_event_id: FIXTURE_TARGET_EVENT_ID,
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_COUNTERSIGN_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_COUNTERSIGN_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_COUNTERSIGN_HEX,
        "AdminCountersign wire format changed"
    );
}

// Reference: the `FIXTURE_PROPOSER_ADDR` constant is defined for use
// by integration tests in tests/community_admin_quorum_integration.rs
// later in the plan; suppress unused-const lint here.
#[allow(dead_code)]
const _: OwnerAddr = FIXTURE_PROPOSER_ADDR;
```

- [ ] **Step 2: Run the tests in regen mode**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(admin_proposal_) + test(admin_countersign_canonical)' --no-fail-fast 2>&1 | grep -E "REGENERATE|test_name" | head -20
```

Each failing test prints `REGENERATE EXPECTED_..._HEX = "<hex>";`. Copy each constant value into the corresponding `const EXPECTED_..._HEX` declaration.

- [ ] **Step 3: Replace `FILL_AFTER` with the actual hex strings**

Edit the file, replacing each `FILL_AFTER` placeholder with the real hex string captured in Step 2.

- [ ] **Step 4: Re-run the tests**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(admin_proposal_) + test(admin_countersign_canonical)'
```

Expected: 4/4 passing.

- [ ] **Step 5: Run full gates**

```bash
( cd src-tauri && cargo fmt --all -- --check ) && \
( cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings ) && \
( cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures ) && \
npx tsc --noEmit && npx vitest run
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/tests/wire_format_zeb250_fixtures.rs
git commit -m "test(zeb-250): pin canonical CBOR for AdminProposal / AdminCountersign

Byte-pinned fixtures for the four new ZEB-250 variant shapes. Locks
wire format ahead of verify + materialize implementation so any
accidental encoding drift is caught at the boundary.

Spec: docs/specs/2026-05-16-zeb-250-admin-quorum-design.md §8.1."
```

---

## Task 3: `admin_quorum` field on `CommunityState` and `MaterializedMembership`

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs` — add `admin_quorum: u8` to `CommunityState`.
- Modify: `src-tauri/src/community_membership.rs` — add `admin_quorum: u8` to `MaterializedMembership`.
- Modify: `src-tauri/tests/wire_format_zeb250_fixtures.rs` — add the two CommunityState fixtures.

**Background:** Spec §3.1. CBOR key `aq`, default `1`, skip-if-default → byte-compatible with pre-ZEB-250 blobs. The field lives in BOTH locations:

- `CommunityState.admin_quorum`: persisted source-of-truth, serialized.
- `MaterializedMembership.admin_quorum`: derived view (recomputed by `materialize` from `ChangeQuorum` proposals).

The materialize pass mutates the `MaterializedMembership` running state; after the materialize completes, the call site (e.g., `insert_event` or `materialized`) writes the result back to `CommunityState.admin_quorum`.

- [ ] **Step 1: Add `admin_quorum` to `MaterializedMembership`**

In `src-tauri/src/community_membership.rs`, modify the `MaterializedMembership` struct definition (around line 882):

```rust
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    pub power_levels: BTreeMap<OwnerAddr, u8>,

    #[serde(default)]
    pub channels: BTreeMap<ChannelId, ChannelInfo>,

    #[serde(default)]
    pub current_epoch: Option<u64>,

    #[serde(default)]
    pub pending_rotation_for: BTreeSet<OwnerAddr>,

    #[serde(default)]
    pub pending_catchup_for: BTreeSet<OwnerAddr>,

    /// ZEB-250: number of admin-tier signatures required for an
    /// admin-affecting action (SetPower to/from 100, Kick of an admin,
    /// or change of admin_quorum itself). Default 1 (current
    /// single-admin behavior); communities opt into multi-sig by
    /// raising it via a successful ChangeQuorum proposal.
    ///
    /// Materialized from events: the materialize pass walks
    /// AdminProposal events in HLC order and updates this field
    /// when a ChangeQuorum proposal reaches quorum (single-pass-with-
    /// running-state, spec §5.2). Byte-compat with pre-ZEB-250 cached
    /// snapshots — the `default = "default_admin_quorum"` decode
    /// produces 1.
    #[serde(
        rename = "aq",
        default = "default_admin_quorum",
        skip_serializing_if = "is_default_admin_quorum"
    )]
    pub admin_quorum: u8,
}

pub(crate) fn default_admin_quorum() -> u8 {
    1
}

pub(crate) fn is_default_admin_quorum(q: &u8) -> bool {
    *q == 1
}
```

If `MaterializedMembership` doesn't already use `Default`, the helper functions above let the derive expand cleanly.

Verify: the existing `#[derive(Default)]` on `MaterializedMembership` still compiles because `u8` defaults to 0 via `Default`, but our explicit `default_admin_quorum` (returning 1) is the right semantic. Use a manual `Default` impl to keep this explicit:

Replace the existing `#[derive(... Default ...)]` line above the struct with an explicit impl:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedMembership { ... }

impl Default for MaterializedMembership {
    fn default() -> Self {
        Self {
            members: BTreeMap::new(),
            power_levels: BTreeMap::new(),
            channels: BTreeMap::new(),
            current_epoch: None,
            pending_rotation_for: BTreeSet::new(),
            pending_catchup_for: BTreeSet::new(),
            admin_quorum: 1,
        }
    }
}
```

(Remove `Default` from the `#[derive(...)]` line if it was there.)

- [ ] **Step 2: Add `admin_quorum` to `CommunityState`**

In `src-tauri/src/community_state_crdt.rs`, modify the struct (the `pub struct CommunityState { ... }` block starting at line 28). Add the new field BEFORE the `events: BTreeMap<EventId, SignedMembershipEvent>` field (so it appears in CBOR key order — `aq` sorts after `fl` but before `ev` lexicographically):

```rust
    /// ZEB-250: M-of-N admin quorum. Number of admin-tier signatures
    /// required for admin-affecting actions (SetPower to/from 100,
    /// Kick of an admin, change of admin_quorum itself).
    ///
    /// Default 1 (single-admin governance — the proposer's signature
    /// alone suffices). When raised >= 2, admin-affecting actions
    /// must arrive as AdminProposal (with >= N-1 AdminCountersigns)
    /// instead of direct SetPower/Kick events. Backwards-compatible:
    /// pre-ZEB-250 blobs lack this field and decode as default 1.
    ///
    /// Cache of materialize-derived state — `materialize` walks
    /// ChangeQuorum proposals to compute the current value, and
    /// `insert_event` writes the result back here so fast-load
    /// (deserialize from disk) has the right value without
    /// re-materializing.
    #[serde(
        rename = "aq",
        default = "crate::community_membership::default_admin_quorum",
        skip_serializing_if = "crate::community_membership::is_default_admin_quorum"
    )]
    pub admin_quorum: u8,
```

- [ ] **Step 3: Update `CommunityState::new`, `Clone`, `PartialEq`, and (if present) any builder**

In the same file, modify `Clone for CommunityState` (around line 121):

```rust
impl Clone for CommunityState {
    fn clone(&self) -> Self {
        Self {
            community_id: self.community_id,
            forked_from: self.forked_from,
            forked_at_wall_ms: self.forked_at_wall_ms,
            parent_lineage: self.parent_lineage.clone(),
            admin_quorum: self.admin_quorum,
            events: self.events.clone(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
            bootstrap_hint: std::sync::Mutex::new(
                self.bootstrap_hint.lock().ok().and_then(|g| g.clone()),
            ),
        }
    }
}
```

`PartialEq` (around line 137):

```rust
impl PartialEq for CommunityState {
    fn eq(&self, other: &Self) -> bool {
        self.community_id == other.community_id
            && self.forked_from == other.forked_from
            && self.forked_at_wall_ms == other.forked_at_wall_ms
            && self.parent_lineage == other.parent_lineage
            && self.admin_quorum == other.admin_quorum
            && self.events == other.events
    }
}
```

`new` (around line 168):

```rust
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            forked_from: None,
            forked_at_wall_ms: None,
            parent_lineage: Vec::new(),
            admin_quorum: 1,
            events: BTreeMap::new(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
            bootstrap_hint: std::sync::Mutex::new(None),
        }
    }
```

- [ ] **Step 4: Update `insert_event` to write back admin_quorum after materialize**

In the same file, locate `insert_event` (around line 269). After the successful `self.events.insert(event.id, event)` + cache version bump, add a writeback:

```rust
        self.events.insert(event.id, event);
        // Invalidate cache by bumping version. Lazy re-mat happens on
        // the next `materialized` call.
        self.cache.lock().expect("cache mutex poisoned").version += 1;

        // ZEB-250: synchronize CommunityState.admin_quorum with the
        // freshly-recomputed materialized view. `materialize` is the
        // source of truth (walks ChangeQuorum proposals in HLC order);
        // we write the result back to the persistent field so fast-load
        // doesn't need to re-materialize.
        let derived = self.materialize_now(ctx.admin_addr).admin_quorum;
        self.admin_quorum = derived;

        InsertOutcome::Inserted
    }
```

- [ ] **Step 5: Add the two remaining wire-format fixtures (5 + 6)**

In `src-tauri/tests/wire_format_zeb250_fixtures.rs`, append (and update the imports):

```rust
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::owner_state_types::SpaceId;

const FIXTURE_COMMUNITY_ID: SpaceId = SpaceId([0x77; 16]);

const EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX: &str = "FILL_AFTER";

#[test]
fn community_state_with_admin_quorum_canonical_cbor() {
    let mut state = CommunityState::new(FIXTURE_COMMUNITY_ID);
    state.admin_quorum = 3;
    let encoded = canonical_cbor_encode(&state).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX,
        "CommunityState with admin_quorum != 1 wire format changed"
    );

    // Structural sanity: the "aq" key must appear when admin_quorum != 1.
    let value: ciborium::Value =
        ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");
    assert!(
        map.iter().any(|(k, _)| k.as_text() == Some("aq")),
        "CommunityState with non-default admin_quorum should emit 'aq' key"
    );
}

#[test]
fn community_state_default_quorum_omits_aq_key() {
    // Byte-compat with pre-ZEB-250 communities: admin_quorum == 1 must
    // NOT emit the "aq" key. Encoding is byte-identical to a state
    // serialized before ZEB-250 existed.
    let state = CommunityState::new(FIXTURE_COMMUNITY_ID);
    assert_eq!(state.admin_quorum, 1, "default admin_quorum must be 1");

    let encoded = canonical_cbor_encode(&state).expect("encode");
    let value: ciborium::Value =
        ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");
    assert!(
        !map.iter().any(|(k, _)| k.as_text() == Some("aq")),
        "CommunityState with default admin_quorum=1 must omit 'aq' key (byte-compat)"
    );
}
```

- [ ] **Step 6: Regen the `community_state_with_admin_quorum_canonical_cbor` fixture**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(community_state_with_admin_quorum_canonical_cbor)' --no-fail-fast 2>&1 | grep REGENERATE
```

Copy the printed hex string into the `EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX` constant.

- [ ] **Step 7: Confirm `default` fixture is byte-compat**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(community_state_default_quorum_omits_aq_key)'
```

Expected: pass. The CBOR for a fresh `CommunityState::new(...)` MUST be byte-identical to a pre-ZEB-250 encoding.

- [ ] **Step 8: Run full gates**

Run the five CI gates. Expected: all green. Test count grows by 2 (Rust).

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs \
        src-tauri/src/community_membership.rs \
        src-tauri/tests/wire_format_zeb250_fixtures.rs
git commit -m "feat(zeb-250): admin_quorum field on CommunityState + MaterializedMembership

Adds the persistent (CommunityState) + derived (MaterializedMembership)
quorum field. CBOR key 'aq', default 1, skip-if-default for byte-compat
with pre-ZEB-250 blobs.

insert_event writes back the materialize-derived admin_quorum so
fast-load on disk reload has the right value without re-materializing.

Spec: §3.1, §5.6."
```

---

## Task 4: AdminProposal verify (AP1-AP5)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — replace the AdminProposal stub in `verify_event` with the full 5-gate check; add 5 new `VerifyError` variants.

**Background:** Spec §4.1-§4.3. Five gates: AP1 (actor Joined), AP2 (actor power ≥ 100), AP3 (proposal_kind well-formed), AP4 (admin-affecting per §4.3), AP5 (ChangeQuorum range).

- [ ] **Step 1: Add VerifyError variants**

Locate the `pub enum VerifyError {` block (around line 500). Add after the last existing JoinCountersign variant:

```rust
    /// ZEB-250 AP1: AdminProposal actor is not currently Joined.
    AdminProposalActorNotJoined,
    /// ZEB-250 AP2: AdminProposal actor's power is below 100.
    AdminProposalActorNotAdmin,
    /// ZEB-250 AP3: proposal_kind is malformed (target absent, level
    /// out of range, reason empty-string, etc.).
    AdminProposalKindInvalid,
    /// ZEB-250 AP4: proposal_kind is well-formed but doesn't qualify
    /// as admin-affecting per §4.3 — wrapping a routine SetPower or
    /// Kick in AdminProposal is a category error.
    AdminProposalNotAdminAffecting,
    /// ZEB-250 AP5: ChangeQuorum new_quorum is < 1 or exceeds current
    /// admin count.
    AdminProposalQuorumOutOfRange,
```

Then add the matching `Display` arms in the existing `impl Display for VerifyError` block:

```rust
            VerifyError::AdminProposalActorNotJoined => {
                write!(f, "ZEB-250 AdminProposal actor is not Joined")
            }
            VerifyError::AdminProposalActorNotAdmin => {
                write!(f, "ZEB-250 AdminProposal actor power < 100 (admin tier)")
            }
            VerifyError::AdminProposalKindInvalid => {
                write!(f, "ZEB-250 AdminProposal proposal_kind is malformed")
            }
            VerifyError::AdminProposalNotAdminAffecting => {
                write!(f, "ZEB-250 AdminProposal proposal_kind is not admin-affecting")
            }
            VerifyError::AdminProposalQuorumOutOfRange => {
                write!(f, "ZEB-250 AdminProposal ChangeQuorum new_quorum out of range [1, admin_count]")
            }
```

- [ ] **Step 2: Replace AdminProposal verify stub with the real gates**

Locate the placeholder stub from Task 1 Step 4 in `verify_event`. Replace the `AdminProposal { .. }` arm with:

```rust
        MembershipEventKind::AdminProposal { proposal_kind } => {
            let actor_state = prior_state.members.get(&signed_event.actor);
            // AP1: actor Joined.
            if !matches!(
                actor_state.map(|s| s.status),
                Some(MemberStatus::Joined)
            ) {
                return Err(VerifyError::AdminProposalActorNotJoined);
            }
            // AP2: actor power >= 100.
            let actor_power = prior_state
                .power_levels
                .get(&signed_event.actor)
                .copied()
                .unwrap_or(0);
            if actor_power < 100 {
                return Err(VerifyError::AdminProposalActorNotAdmin);
            }
            // AP3 + AP4: well-formedness + admin-affecting.
            match proposal_kind {
                ProposalKind::SetPower { target, level } => {
                    if !prior_state.members.contains_key(target) {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    if *level > POWER_THRESHOLDS.max {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP4: admin-affecting iff level == 100 OR target was admin.
                    let target_power = prior_state
                        .power_levels
                        .get(target)
                        .copied()
                        .unwrap_or(0);
                    let admin_affecting =
                        *level == 100 || target_power == 100;
                    if !admin_affecting {
                        return Err(VerifyError::AdminProposalNotAdminAffecting);
                    }
                }
                ProposalKind::Kick { target, reason } => {
                    // AP3 part 1: target exists.
                    let target_state = prior_state.members.get(target);
                    if target_state.is_none() {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP3 part 2: target is Joined (banned/left don't make sense to kick).
                    if !matches!(
                        target_state.map(|s| s.status),
                        Some(MemberStatus::Joined)
                    ) {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP3 part 3: reason is None or non-empty.
                    if let Some(r) = reason {
                        if r.is_empty() {
                            return Err(VerifyError::AdminProposalKindInvalid);
                        }
                    }
                    // AP4: admin-affecting iff target is admin.
                    let target_power = prior_state
                        .power_levels
                        .get(target)
                        .copied()
                        .unwrap_or(0);
                    if target_power != 100 {
                        return Err(VerifyError::AdminProposalNotAdminAffecting);
                    }
                }
                ProposalKind::ChangeQuorum { new_quorum } => {
                    // AP3: new_quorum >= 1.
                    if *new_quorum < 1 {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP5: new_quorum <= current admin count.
                    let admin_count = prior_state
                        .power_levels
                        .values()
                        .filter(|p| **p == 100)
                        .count() as u32;
                    if (*new_quorum as u32) > admin_count {
                        return Err(VerifyError::AdminProposalQuorumOutOfRange);
                    }
                    // ChangeQuorum is always admin-affecting; no AP4 distinction.
                }
            }
            Ok(())
        }
```

Note: this still leaves the `AdminCountersign` stub from Task 1 in place — Task 5 will replace that.

- [ ] **Step 3: Add unit tests**

In the existing `#[cfg(test)] mod tests` block, add the 10 tests from spec §8.2 (AdminProposal verify, 10 tests). Each test:

1. Constructs a `MaterializedMembership` representing the prior state.
2. Builds a `SignedMembershipEvent` carrying the proposal.
3. Calls `verify_event` and asserts the expected `Ok` / `Err(VerifyError::...)`.

Use the existing test helpers in the file (search for `fn make_setpower_event`, `fn synth_addr`, etc.). If a helper doesn't exist for AdminProposal, add a thin one:

```rust
    fn make_admin_proposal_event(
        id_byte: u8,
        actor: OwnerAddr,
        proposal_kind: ProposalKind,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            id: [id_byte; 16],
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".to_string(),
            },
            kind: MembershipEventKind::AdminProposal { proposal_kind },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        }
    }
```

Then the 10 tests:

```rust
    #[test]
    fn admin_proposal_accepted_when_actor_admin() {
        let admin = OwnerAddr([0x01; 16]);
        let other = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(other, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "o".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(other, 0);
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::SetPower { target: other, level: 100 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_proposal_rejected_when_actor_not_joined() {
        let actor = OwnerAddr([0x01; 16]);
        let prior = MaterializedMembership::default();
        let evt = make_admin_proposal_event(
            0x10,
            actor,
            ProposalKind::SetPower {
                target: OwnerAddr([0x02; 16]),
                level: 100,
            },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: actor };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalActorNotJoined)
        );
    }

    #[test]
    fn admin_proposal_rejected_when_actor_power_below_100() {
        let actor = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(actor, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(target, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "t".into() },
            left_at: None,
        });
        prior.power_levels.insert(actor, 50); // mod tier, not admin
        prior.power_levels.insert(target, 0);
        let evt = make_admin_proposal_event(
            0x10,
            actor,
            ProposalKind::SetPower { target, level: 100 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: actor };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalActorNotAdmin)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_target_not_in_members() {
        let admin = OwnerAddr([0x01; 16]);
        let ghost = OwnerAddr([0xfe; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::SetPower { target: ghost, level: 100 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_level_out_of_range() {
        let admin = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(target, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "t".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(target, 100);
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::SetPower { target, level: 200 }, // out of range
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_not_admin_affecting() {
        let admin = OwnerAddr([0x01; 16]);
        let regular = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(regular, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "r".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(regular, 0);
        // SetPower regular -> 50 (mod tier) — not admin-affecting.
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::SetPower { target: regular, level: 50 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_kick_rejected_when_target_not_admin() {
        let admin = OwnerAddr([0x01; 16]);
        let mod_user = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(mod_user, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "m".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(mod_user, 50);
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::Kick { target: mod_user, reason: None },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_below_one() {
        let admin = OwnerAddr([0x01; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::ChangeQuorum { new_quorum: 0 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_exceeds_admin_count() {
        let admin = OwnerAddr([0x01; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        // Only 1 admin, so new_quorum = 2 exceeds.
        let evt = make_admin_proposal_event(
            0x10,
            admin,
            ProposalKind::ChangeQuorum { new_quorum: 2 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalQuorumOutOfRange)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_accepted_when_equals_admin_count() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin1, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a1".into() },
            left_at: None,
        });
        prior.members.insert(admin2, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a2".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin1, 100);
        prior.power_levels.insert(admin2, 100);
        let evt = make_admin_proposal_event(
            0x10,
            admin1,
            ProposalKind::ChangeQuorum { new_quorum: 2 },
            1_000,
        );
        let ctx = VerifyContext { admin_addr: admin1 };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }
```

Note: the `VerifyContext` struct is the existing argument type used throughout the file — if the field name is `admin_addr`, the tests above will match. If you find the struct uses a different field name, adapt accordingly (search for `struct VerifyContext` in the file).

- [ ] **Step 4: Run new tests in isolation**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(admin_proposal_)' 
```

Expected: all 10 + the 3 Task-1 roundtrip tests = 13 pass.

- [ ] **Step 5: Run full gates**

Run the five CI gates. Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): AdminProposal verify gates AP1-AP5

Replaces the Task 1 stub with the real 5-gate verify per spec §4.1-§4.3:
- AP1 actor Joined
- AP2 actor power >= 100
- AP3 proposal_kind well-formedness
- AP4 admin-affecting criteria
- AP5 ChangeQuorum range check

10 new unit tests covering accepts + each rejection path."
```

---

## Task 5: AdminCountersign verify (AC1-AC3, lenient forward-ref)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — replace AdminCountersign stub; add AC VerifyError variants.

**Background:** Spec §4.4. Three gates. **Lenient forward-ref**: AC does NOT require the target AdminProposal to be present in the event log — pairing happens at materialize time, matching ZEB-254's JoinCountersign semantics.

- [ ] **Step 1: Add VerifyError variants**

In `pub enum VerifyError`:

```rust
    /// ZEB-250 AC1: AdminCountersign actor is not currently Joined.
    AdminCountersignActorNotJoined,
    /// ZEB-250 AC2: AdminCountersign actor's power is below 100.
    AdminCountersignActorNotAdmin,
    /// ZEB-250 AC3: target_event_id is malformed (e.g., all-zero).
    AdminCountersignTargetIdMalformed,
```

Display arms:

```rust
            VerifyError::AdminCountersignActorNotJoined => {
                write!(f, "ZEB-250 AdminCountersign actor is not Joined")
            }
            VerifyError::AdminCountersignActorNotAdmin => {
                write!(f, "ZEB-250 AdminCountersign actor power < 100 (admin tier)")
            }
            VerifyError::AdminCountersignTargetIdMalformed => {
                write!(f, "ZEB-250 AdminCountersign target_event_id is malformed")
            }
```

- [ ] **Step 2: Replace AdminCountersign verify stub**

In `verify_event`, replace the catch-all stub for `AdminCountersign` with:

```rust
        MembershipEventKind::AdminCountersign { target_event_id } => {
            // AC1: actor Joined.
            let actor_state = prior_state.members.get(&signed_event.actor);
            if !matches!(
                actor_state.map(|s| s.status),
                Some(MemberStatus::Joined)
            ) {
                return Err(VerifyError::AdminCountersignActorNotJoined);
            }
            // AC2: actor power >= 100.
            let actor_power = prior_state
                .power_levels
                .get(&signed_event.actor)
                .copied()
                .unwrap_or(0);
            if actor_power < 100 {
                return Err(VerifyError::AdminCountersignActorNotAdmin);
            }
            // AC3: target_event_id non-zero.
            if target_event_id.iter().all(|b| *b == 0) {
                return Err(VerifyError::AdminCountersignTargetIdMalformed);
            }
            // Note: AC verify does NOT require the target proposal to
            // be in the event log yet. Lenient forward-ref semantics
            // mirror ZEB-254's JoinCountersign — out-of-order DAG-sync
            // delivery is normal. Pairing happens at materialize time.
            Ok(())
        }
```

Also remove the now-dead combined-arm catch-all stub at the bottom of the match block (the `AdminProposal { .. } | AdminCountersign { .. } => Ok(())` placeholder from Task 1 should no longer match anything; delete it).

- [ ] **Step 3: Add 4 unit tests**

```rust
    #[test]
    fn admin_countersign_accepted_when_actor_admin() {
        let admin = OwnerAddr([0x01; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::AdminCountersign {
                target_event_id: [0x55; 16],
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_countersign_rejected_when_actor_not_joined() {
        let actor = OwnerAddr([0x01; 16]);
        let prior = MaterializedMembership::default();
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::AdminCountersign {
                target_event_id: [0x55; 16],
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: actor };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminCountersignActorNotJoined)
        );
    }

    #[test]
    fn admin_countersign_rejected_when_actor_power_below_100() {
        let mod_user = OwnerAddr([0x01; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(mod_user, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "m".into() },
            left_at: None,
        });
        prior.power_levels.insert(mod_user, 50);
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: mod_user,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "m".into() },
            kind: MembershipEventKind::AdminCountersign {
                target_event_id: [0x55; 16],
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: mod_user };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminCountersignActorNotAdmin)
        );
    }

    #[test]
    fn admin_countersign_accepted_when_target_not_present_yet() {
        // Lenient forward-ref: AC must verify even when the target
        // AdminProposal is not yet in the log. prior_state has no
        // record of [0x55; 16] — and that's fine.
        let admin = OwnerAddr([0x01; 16]);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        let evt = SignedMembershipEvent {
            id: [0x11; 16],
            actor: admin,
            at: Hlc { wall_ms: 5_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::AdminCountersign {
                target_event_id: [0x55; 16], // target absent from prior_state
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }
```

- [ ] **Step 4: Run new tests + full gates**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(admin_countersign_)'
```

Expected: 4/4 pass. Then run all five CI gates.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): AdminCountersign verify gates AC1-AC3 with lenient forward-ref

Per spec §4.4. Three gates:
- AC1 actor Joined
- AC2 actor power >= 100
- AC3 target_event_id non-zero

Lenient forward-ref: AC verify does NOT require the target proposal
to be present in the event log. Pairing happens at materialize time.
Mirrors ZEB-254 JoinCountersign semantics."
```

---

## Task 6: Modified verify for direct `SetPower` and `Kick` (admin_quorum gate)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — add quorum-aware rejection to the existing `SetPower` and `Kick` arms of `verify_event`.

**Background:** Spec §4.5-§4.6. When `admin_quorum > 1` AND the action is admin-affecting, direct events are rejected. Forces the protocol path. Backwards-compat: `admin_quorum == 1` → existing behavior.

`prior_state` is a `MaterializedMembership`, so `prior_state.admin_quorum` is what we check.

- [ ] **Step 1: Add VerifyError variants**

```rust
    /// ZEB-250: direct SetPower of an admin-affecting target was
    /// submitted while `prior_state.admin_quorum > 1`. Action must
    /// arrive as AdminProposal instead.
    SetPowerRequiresQuorum,
    /// ZEB-250: direct Kick of an admin target was submitted while
    /// `prior_state.admin_quorum > 1`. Action must arrive as
    /// AdminProposal instead.
    KickRequiresQuorum,
```

Display arms:

```rust
            VerifyError::SetPowerRequiresQuorum => {
                write!(f, "ZEB-250: direct admin-affecting SetPower rejected (admin_quorum > 1 — use AdminProposal)")
            }
            VerifyError::KickRequiresQuorum => {
                write!(f, "ZEB-250: direct Kick of an admin rejected (admin_quorum > 1 — use AdminProposal)")
            }
```

- [ ] **Step 2: Locate the existing `SetPower` verify arm**

Around line 2063 in `verify_event` (the existing `MembershipEventKind::SetPower { level, .. } =>` arm). After the existing power-gate checks pass, BEFORE returning `Ok(())`, add the quorum gate:

```rust
        MembershipEventKind::SetPower { target, level } => {
            // ... existing power-gate checks (actor_power >= POWER_THRESHOLDS.set_power,
            // *level <= POWER_THRESHOLDS.max) — keep as-is.

            // ZEB-250: when admin_quorum > 1 and action is admin-affecting,
            // direct SetPower is rejected. Must arrive as AdminProposal.
            if prior_state.admin_quorum > 1 {
                let target_power = prior_state
                    .power_levels
                    .get(target)
                    .copied()
                    .unwrap_or(0);
                let admin_affecting = *level == 100 || target_power == 100;
                if admin_affecting {
                    return Err(VerifyError::SetPowerRequiresQuorum);
                }
            }

            Ok(())
        }
```

(Keep the existing checks; just append the quorum gate at the end of the arm before the final `Ok(())`. Look at the existing arm in the file to see exactly which variables are in scope; you may need to destructure differently. Pattern match the spec — admin_affecting iff promoting to admin OR demoting an admin.)

- [ ] **Step 3: Locate the existing `Kick` verify arm**

Around line 2038. Similarly add:

```rust
        MembershipEventKind::Kick { target, reason } => {
            // ... existing power-gate checks (actor_power >= POWER_THRESHOLDS.kick) — keep.

            // ZEB-250: when admin_quorum > 1 and target is admin,
            // direct Kick is rejected. Must arrive as AdminProposal.
            if prior_state.admin_quorum > 1 {
                let target_power = prior_state
                    .power_levels
                    .get(target)
                    .copied()
                    .unwrap_or(0);
                if target_power == 100 {
                    return Err(VerifyError::KickRequiresQuorum);
                }
            }

            // ... existing remainder of the arm — keep as-is.
            Ok(())
        }
```

- [ ] **Step 4: Add 6 unit tests (spec §8.2 modified-verify block)**

```rust
    #[test]
    fn direct_setpower_to_100_rejected_when_admin_quorum_above_1() {
        let admin = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 2;
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(target, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "t".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(target, 0);
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::SetPower { target, level: 100 },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerRequiresQuorum)
        );
    }

    #[test]
    fn direct_setpower_demote_admin_rejected_when_admin_quorum_above_1() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 2;
        for a in &[admin1, admin2] {
            prior.members.insert(*a, MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "x".into() },
                left_at: None,
            });
            prior.power_levels.insert(*a, 100);
        }
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin1,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::SetPower {
                target: admin2,
                level: 0,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin1 };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerRequiresQuorum)
        );
    }

    #[test]
    fn direct_setpower_to_non_admin_accepted_regardless_of_quorum() {
        let admin = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 3;
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(target, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "t".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(target, 0);
        // Promote target to mod (50) — not admin-affecting.
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::SetPower {
                target,
                level: 50,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn direct_kick_of_admin_rejected_when_admin_quorum_above_1() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 2;
        for a in &[admin1, admin2] {
            prior.members.insert(*a, MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "x".into() },
                left_at: None,
            });
            prior.power_levels.insert(*a, 100);
        }
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin1,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::Kick {
                target: admin2,
                reason: None,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin1 };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::KickRequiresQuorum)
        );
    }

    #[test]
    fn direct_kick_of_mod_accepted_regardless_of_quorum() {
        let admin = OwnerAddr([0x01; 16]);
        let mod_user = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 5;
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(mod_user, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "m".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(mod_user, 50);
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::Kick {
                target: mod_user,
                reason: None,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn direct_setpower_admin_actions_accepted_when_admin_quorum_equals_1() {
        // Backwards-compat: admin_quorum=1 (default) means direct SetPower
        // is allowed even for admin-affecting actions. Single-admin
        // governance preserved.
        let admin = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x02; 16]);
        let mut prior = MaterializedMembership::default();
        prior.admin_quorum = 1;
        prior.members.insert(admin, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "a".into() },
            left_at: None,
        });
        prior.members.insert(target, MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc { wall_ms: 0, logical: 0, device_id: "t".into() },
            left_at: None,
        });
        prior.power_levels.insert(admin, 100);
        prior.power_levels.insert(target, 0);
        let evt = SignedMembershipEvent {
            id: [0x10; 16],
            actor: admin,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            kind: MembershipEventKind::SetPower {
                target,
                level: 100,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let ctx = VerifyContext { admin_addr: admin };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }
```

- [ ] **Step 5: Run new tests + full gates**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(direct_setpower_) + test(direct_kick_)'
```

Expected: 6/6 pass. Run all five CI gates — they MUST pass; especially relevant is that all existing direct-SetPower/Kick tests still pass (they implicitly run under `admin_quorum = 1`).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): direct SetPower/Kick reject when admin_quorum > 1

Per spec §4.5 + §4.6:
- Direct SetPower whose target IS admin OR whose level == 100 is rejected
  when prior_state.admin_quorum > 1 (forces AdminProposal path).
- Direct Kick of an admin (target_power == 100) is rejected when
  prior_state.admin_quorum > 1.
- Non-admin-affecting moderation (Kick of mod, SetPower to mod) remains
  single-signed regardless of admin_quorum.
- Backwards-compat: admin_quorum == 1 preserves existing single-admin
  behavior."
```

---

## Task 7: Materialize pre-pass — collect raw signature data

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — extend the pre-pass in `materialize` (and `materialize_with_now`).

**Background:** Spec §5.1. The pre-pass collects three new HashMap structures alongside the existing `countersigned_pending_ids`:

- `quorum_signers: HashMap<EventId, HashSet<OwnerAddr>>` — per-proposal admin signers (proposer auto-included; AdminCountersign actors add).
- `proposals_index: HashMap<EventId, (ProposalKind, OwnerAddr, u64)>` — proposer + wall_ms metadata.
- `proposal_signing_hlcs: HashMap<EventId, Vec<(u64, OwnerAddr)>>` — wall_ms per signer for nth-signer expiry math.

Pre-pass is unordered (HashMap traversal); ordering happens in the main pass.

- [ ] **Step 1: Locate the existing pre-pass**

Around line 1098 in `materialize`, find the existing `countersigned_pending_ids` construction:

```rust
    let countersigned_pending_ids: std::collections::HashSet<EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::JoinCountersign { target_event_id } => Some(*target_event_id),
            _ => None,
        })
        .collect();
```

Below this (BEFORE the main HLC-ordered iteration begins), add the new pre-pass structures:

```rust
    // ZEB-250 Pre-Pass: collect per-proposal raw signature data.
    // - quorum_signers[event_id]: set of admin OwnerAddrs who have
    //   signed the proposal (proposer auto-included + each
    //   AdminCountersign actor).
    // - proposals_index[event_id]: (proposal_kind, proposer_addr,
    //   proposer_wall_ms).
    // - proposal_signing_hlcs[event_id]: per-signing-event (wall_ms,
    //   actor). Used by the main pass to find when the Nth signature
    //   was contributed (for 30-day expiry).
    //
    // Raw collection only; quorum-reached evaluation happens in the
    // main pass (§5.2) because `admin_quorum` itself is a function
    // of prior ChangeQuorum proposals (single-pass-with-running-state).
    let mut quorum_signers: std::collections::HashMap<
        EventId,
        std::collections::HashSet<OwnerAddr>,
    > = std::collections::HashMap::new();
    let mut proposals_index: std::collections::HashMap<
        EventId,
        (ProposalKind, OwnerAddr, u64),
    > = std::collections::HashMap::new();
    let mut proposal_signing_hlcs: std::collections::HashMap<
        EventId,
        Vec<(u64, OwnerAddr)>,
    > = std::collections::HashMap::new();

    for signed_event in events.iter() {
        match &signed_event.kind {
            MembershipEventKind::AdminProposal { proposal_kind } => {
                proposals_index.insert(
                    signed_event.id,
                    (
                        proposal_kind.clone(),
                        signed_event.actor,
                        signed_event.at.wall_ms,
                    ),
                );
                quorum_signers
                    .entry(signed_event.id)
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(signed_event.actor);
                proposal_signing_hlcs
                    .entry(signed_event.id)
                    .or_insert_with(Vec::new)
                    .push((signed_event.at.wall_ms, signed_event.actor));
            }
            MembershipEventKind::AdminCountersign { target_event_id } => {
                quorum_signers
                    .entry(*target_event_id)
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(signed_event.actor);
                proposal_signing_hlcs
                    .entry(*target_event_id)
                    .or_insert_with(Vec::new)
                    .push((signed_event.at.wall_ms, signed_event.actor));
            }
            _ => {}
        }
    }
```

Mark the bindings `mut` even if they're later only read by the main pass — they're modified during the for-loop.

Important note: the structures must be `mut` and accessible to the main-pass match arms. If `materialize` is currently structured so the main loop closes over read-only bindings, refactor to make these mutable bindings available throughout. They are NOT mutated again after the pre-pass — but the main pass needs read access.

To prevent unused-variable warnings if Task 8 isn't yet wired up, mark them with `#[allow(unused_variables)]` temporarily — Task 8 will consume them.

- [ ] **Step 2: Replicate for `materialize_with_now`**

If `materialize_with_now` (around line 1072) duplicates the pre-pass, add the same structures there. (If it delegates to `materialize`, no change needed — read it to confirm.)

- [ ] **Step 3: Add a unit test that the pre-pass collects correctly**

```rust
    #[test]
    fn materialize_prepass_collects_admin_proposal_signers() {
        // Construct a log with one AdminProposal + one AdminCountersign.
        // Run materialize; the result should have admin_quorum == 1
        // (default) but internally the pre-pass should have collected
        // both signers. We can't directly inspect the pre-pass — instead
        // verify via a subsequent test once Task 8 lands. For now,
        // confirm the materialize doesn't crash when these events are
        // present.
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = vec![];

        // Bootstrap: target joins as regular member, admin1+admin2 join
        // as admins via SetPower under quorum=1 (default).
        // (Use existing test helpers; this is sketched — adapt to
        // existing patterns in the file.)

        let prop_id = [0xAA; 16];
        events.push(SignedMembershipEvent {
            id: prop_id,
            actor: admin1,
            at: Hlc { wall_ms: 10_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower {
                    target,
                    level: 100,
                },
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        });
        events.push(SignedMembershipEvent {
            id: [0xBB; 16],
            actor: admin2,
            at: Hlc { wall_ms: 11_000, logical: 0, device_id: "a2".into() },
            kind: MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        });

        // No assertion on effect yet — Task 8 wires the main-pass
        // application. This test just exercises the pre-pass without
        // panicking.
        let _m = materialize(&events, admin1);
    }
```

This is a placeholder test ensuring the pre-pass compiles and doesn't panic. Task 8 will add the assertions on effect application.

- [ ] **Step 4: Run gates**

Run the five CI gates. Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): materialize pre-pass collects AdminProposal/Countersign signers

Per spec §5.1. Raw signature collection only (quorum_signers,
proposals_index, proposal_signing_hlcs). Quorum-reached evaluation
deferred to main pass (§5.2) so the running admin_quorum can mutate
mid-iteration via ChangeQuorum proposals."
```

---

## Task 8: Materialize main pass — apply AdminProposal effects with running admin_quorum

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — replace AdminProposal/Countersign main-pass stubs with effect application logic.

**Background:** Spec §5.2 + §5.3 + §5.6. The main pass walks events in HLC order. When it encounters an `AdminProposal`, it consults `quorum_signers[id]` and the running `admin_quorum`. If `signers >= admin_quorum` AND the Nth signer landed within 30 days of the proposal, apply the effect.

The effect application mirrors what direct `SetPower` / `Kick` events do — except for `ChangeQuorum`, which mutates `materialized_state.admin_quorum` (NOT a member).

The `AdminCountersign` arm in the main pass is a no-op (consumed by pre-pass).

- [ ] **Step 1: Locate the main-pass HLC-ordered iteration**

In `materialize` (after the pre-pass blocks added in Task 7), find the HLC-sorted loop that builds the materialized state. Look for an existing `MembershipEventKind::PendingJoin { .. }` arm (around line 1553) — this gives you the right pattern + the place to insert.

- [ ] **Step 2: Add a constant for 30-day expiry**

Near other constants at the top of `community_membership.rs` (after `POWER_THRESHOLDS`), add:

```rust
/// ZEB-250: AdminProposal expiry window. A proposal that reaches
/// quorum more than 30 days after its proposer's HLC is dead — late
/// countersigns to expired proposals are no-ops at materialize time.
///
/// Mirrors ZEB-254's PendingJoin 30-day expiry. Same constant value;
/// kept as a separate const for clarity at the call site.
pub const ADMIN_PROPOSAL_EXPIRY_MS: u64 = 30 * 24 * 60 * 60 * 1000;
```

If ZEB-254 already exposes a similar constant (search `30 * 24 * 60 * 60`), reuse it. Otherwise add the constant above.

- [ ] **Step 3: Add the AdminProposal main-pass arm**

Inside the HLC-ordered iteration, add an arm:

```rust
            MembershipEventKind::AdminProposal { proposal_kind: _ } => {
                // ZEB-250 main-pass §5.2: evaluate the proposal against
                // the *running* admin_quorum. If signers >= admin_quorum
                // and the Nth signer landed within 30 days of the
                // proposer's HLC, apply the effect.
                let signers = quorum_signers
                    .get(&event.id)
                    .map(|s| s.len())
                    .unwrap_or(0);
                let admin_quorum_now = materialized.admin_quorum as usize;
                if signers >= admin_quorum_now {
                    // Find the Nth signer's wall_ms (sorted by wall_ms ascending).
                    let mut sorted_hlcs: Vec<(u64, OwnerAddr)> = proposal_signing_hlcs
                        .get(&event.id)
                        .cloned()
                        .unwrap_or_default();
                    sorted_hlcs.sort_by_key(|(wall_ms, _)| *wall_ms);
                    // 1-indexed: the (admin_quorum_now - 1)-th entry is the
                    // signer that pushed the count over the threshold.
                    if let Some((nth_signer_wall_ms, _)) =
                        sorted_hlcs.get(admin_quorum_now - 1)
                    {
                        let age_when_reached =
                            nth_signer_wall_ms.saturating_sub(event.at.wall_ms);
                        if age_when_reached <= ADMIN_PROPOSAL_EXPIRY_MS {
                            // Apply effect: ChangeQuorum updates running
                            // admin_quorum; SetPower mutates power_levels +
                            // members; Kick mutates members + power_levels.
                            if let Some((kind, _proposer, _proposer_wall_ms)) =
                                proposals_index.get(&event.id).cloned()
                            {
                                apply_admin_proposal_effect(
                                    &mut materialized,
                                    &kind,
                                    event,
                                );
                            }
                        }
                        // else: quorum reached too late; no effect.
                    }
                }
                // else: insufficient signatures; pending. No mutation.
            }
            MembershipEventKind::AdminCountersign { .. } => {
                // ZEB-250: countersigns are consumed by the pre-pass.
                // Main-pass arm is a no-op.
            }
```

`materialized` is whatever the existing main-pass loop calls the in-progress materialized state — adapt the variable name to match the surrounding code.

- [ ] **Step 4: Add the `apply_admin_proposal_effect` helper**

In the same file, near the bottom (after `materialize` / `materialize_with_now` definitions):

```rust
/// ZEB-250: apply an admin-proposal's effect to the running
/// materialized state when the proposal has reached quorum within the
/// 30-day window. Translates the wrapped ProposalKind into the same
/// mutation that a direct SetPower / Kick / ChangeQuorum would produce.
fn apply_admin_proposal_effect(
    materialized: &mut MaterializedMembership,
    proposal_kind: &ProposalKind,
    proposal_event: &SignedMembershipEvent,
) {
    match proposal_kind {
        ProposalKind::SetPower { target, level } => {
            materialized.power_levels.insert(*target, *level);
        }
        ProposalKind::Kick { target, .. } => {
            // Mirror the existing Kick mutation in the materialize main pass.
            // Set target's status to Left (kicked); reset their power to 0.
            if let Some(ms) = materialized.members.get_mut(target) {
                ms.status = MemberStatus::Left;
                ms.left_at = Some(proposal_event.at.clone());
            }
            materialized.power_levels.insert(*target, 0);
        }
        ProposalKind::ChangeQuorum { new_quorum } => {
            materialized.admin_quorum = *new_quorum;
        }
    }
}
```

**Important**: study the existing `MembershipEventKind::Kick { target, .. }` arm (line 1235) and the `SetPower { target, level }` arm (line 1278) in the main pass — the actual mutations they perform may include other side effects (channels, pending_rotation_for, etc.). The `apply_admin_proposal_effect` helper must mirror those EXACTLY so a SetPower-via-quorum produces the same materialized state as a direct SetPower would. Adapt the helper above accordingly.

- [ ] **Step 5: Add unit tests for the main pass**

Spec §8.2 lists 9 materialize tests. Add them. Example:

```rust
    #[test]
    fn materialize_proposal_without_countersigns_pending_when_quorum_above_1() {
        // 2-admin community at quorum=2. admin1 proposes admin2 demotion.
        // No countersigns → effect not applied; admin2 stays admin.
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        // Bootstrap helper: a Join + bootstrap admin1 as the Space's
        // admin_addr (power=100 via the bootstrap rule in materialize),
        // then a direct SetPower under default quorum=1 promoting
        // admin2 to admin.
        let join_admin2 = SignedMembershipEvent {
            id: [0x80; 16],
            actor: admin2,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a2".into() },
            kind: MembershipEventKind::Join,
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        let promote_admin2 = SignedMembershipEvent {
            id: [0x81; 16],
            actor: admin1,
            at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::SetPower {
                target: admin2,
                level: 100,
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        // Raise quorum to 2 via AdminProposal{ChangeQuorum} — sole-signer
        // satisfies under the still-prevailing quorum=1.
        let raise_quorum = SignedMembershipEvent {
            id: [0xCC; 16],
            actor: admin1,
            at: Hlc { wall_ms: 10_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };
        // Now under quorum=2. Propose demote admin2 — no countersign.
        let demote_admin2 = SignedMembershipEvent {
            id: [0xDD; 16],
            actor: admin1,
            at: Hlc { wall_ms: 11_000, logical: 0, device_id: "a1".into() },
            kind: MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower {
                    target: admin2,
                    level: 0,
                },
            },
            sig: [0; 64],
            actor_identity_pub: [0; 64],
        };

        let events = vec![join_admin2, promote_admin2, raise_quorum, demote_admin2];
        let m = materialize(&events, admin1);

        // Effect should NOT have applied — admin2 still admin (power 100).
        assert_eq!(m.power_levels.get(&admin2).copied().unwrap_or(0), 100);
        // admin_quorum updated to 2 (sole-signer ChangeQuorum self-satisfies under prior quorum=1).
        assert_eq!(m.admin_quorum, 2);
    }

    // Pattern: the rest of the materialize tests follow this same scaffold.
    // Bootstrap = (join_admin2, promote_admin2) under default quorum=1, then
    // raise_quorum brings the running state to quorum=2, then construct
    // the AdminProposal under test + optional AdminCountersigns from a
    // third admin (admin3) to flip the effect on/off.
```

Add the remaining 8 tests from spec §8.2 following the same pattern. Key cases:

- `materialize_proposal_effective_when_one_countersign_reaches_quorum_2`
- `materialize_proposal_effective_when_two_countersigns_reach_quorum_3`
- `materialize_proposal_dedups_duplicate_countersigns_by_same_actor`
- `materialize_proposal_expires_at_30_days_without_quorum`
- `materialize_proposal_late_countersign_after_expiry_is_noop`
- `materialize_quorum_reached_within_30d_then_aged_past_30d_remains_effective`
- `materialize_change_quorum_proposal_updates_admin_quorum_field`
- `materialize_setpower_via_quorum_matches_direct_setpower_effect_at_quorum_1`

For each, construct a log, materialize, assert the expected state. Copy the bootstrap pattern from one of the existing multi-admin tests in the file. Use deterministic test bytes (`[0xNN; 16]`).

- [ ] **Step 6: Run all 9 main-pass tests**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(materialize_proposal_) + test(materialize_change_quorum_) + test(materialize_setpower_via_quorum_) + test(materialize_quorum_)'
```

Expected: 9/9 pass.

- [ ] **Step 7: Run full gates**

Expected: all green. Test count grows by ~9-10 (Rust).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-250): materialize main pass applies AdminProposal effects

Per spec §5.2 + §5.3 + §5.6. Single-pass-with-running-state algorithm:
- On AdminProposal event, evaluate signers >= running admin_quorum.
- Find Nth-signer wall_ms via sort-by-wall_ms ascending.
- 30-day expiry: age_when_reached <= ADMIN_PROPOSAL_EXPIRY_MS → apply.
- ChangeQuorum effect mutates the running admin_quorum so subsequent
  proposals in iteration order see the new threshold.
- AdminCountersign main-pass arm is no-op (consumed by pre-pass).

9 new unit tests covering pending/effective/expired permutations,
ChangeQuorum self-satisfaction under quorum=1 → quorum=2 transition,
and dedup of duplicate countersigns."
```

---

## Task 9: `AdminActionResult` enum + auto-route `set_power_level` + `kick_from_community`

**Files:**
- Modify: `src-tauri/src/lib.rs` — add `AdminActionResult` enum; modify `set_power_level` and `kick_from_community` to branch on `admin_quorum`.

**Background:** Spec §6.1. Existing IPCs returned `Result<(), String>` or similar. Now they return `Result<AdminActionResult, String>`. The handler reads `admin_quorum` from CommunityState, decides direct vs proposal, mints the appropriate event.

- [ ] **Step 1: Add the `AdminActionResult` enum**

Near the top of `src-tauri/src/lib.rs` (or in a logical location near other shared DTOs), add:

```rust
/// ZEB-250: discriminated result of an admin moderation IPC. The
/// handler auto-routes based on the target community's admin_quorum:
/// - `Completed` if admin_quorum == 1 OR action is not admin-affecting.
/// - `Pending` if admin_quorum > 1 AND action is admin-affecting — an
///   AdminProposal was minted instead and is awaiting countersignatures.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum AdminActionResult {
    Completed,
    Pending {
        proposal_event_id: String,
        signers_so_far: u8,
        quorum_required: u8,
    },
}
```

- [ ] **Step 2: Update `set_power_level` to auto-route**

Locate the existing `async fn set_power_level(...)` (around line 14418). Modify the signature to return `Result<AdminActionResult, String>` and add auto-routing:

```rust
#[tauri::command]
async fn set_power_level(
    community_id: String,
    target_addr: String,
    level: u8,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<AdminActionResult, String> {
    // ... existing decode community_id, target_addr, generation+registry fence checks ...

    // Read community's admin_quorum + current materialized view.
    let community = /* existing lookup */;
    let materialized = community.materialized(/* admin_addr */);

    let target_power_now = materialized
        .power_levels
        .get(&target_addr_parsed)
        .copied()
        .unwrap_or(0);
    let admin_affecting = level == 100 || target_power_now == 100;
    let admin_quorum = materialized.admin_quorum;

    if admin_quorum > 1 && admin_affecting {
        // Route via AdminProposal.
        let proposal_event = mint_admin_proposal_set_power(
            &state, /* community_id_parsed, */
            target_addr_parsed,
            level,
        )
        .map_err(|e| e.to_string())?;
        return Ok(AdminActionResult::Pending {
            proposal_event_id: hex::encode(proposal_event.id),
            signers_so_far: 1,
            quorum_required: admin_quorum,
        });
    }

    // ... existing direct-SetPower mint + insert path ...
    Ok(AdminActionResult::Completed)
}
```

`mint_admin_proposal_set_power` is a new helper — see Step 4. Adapt variable names + the surrounding error-extraction patterns to match the existing function body.

**Caution**: the existing function performs generation+registry fence checks, error handling, and rotation logic. Don't remove any of that — only add the branch at the top that diverts to AdminProposal when applicable.

- [ ] **Step 3: Update `kick_from_community` to auto-route**

Same pattern. Around line 14115 (`async fn kick_from_community(...)`):

```rust
    // Read community state + materialized view.
    let materialized = community.materialized(/* admin_addr */);

    let target_power_now = materialized
        .power_levels
        .get(&target_addr_parsed)
        .copied()
        .unwrap_or(0);
    let admin_affecting = target_power_now == 100;
    let admin_quorum = materialized.admin_quorum;

    if admin_quorum > 1 && admin_affecting {
        let proposal_event = mint_admin_proposal_kick(
            &state, /* community_id_parsed, */
            target_addr_parsed,
            reason.clone(),
        )
        .map_err(|e| e.to_string())?;
        return Ok(AdminActionResult::Pending {
            proposal_event_id: hex::encode(proposal_event.id),
            signers_so_far: 1,
            quorum_required: admin_quorum,
        });
    }

    // ... existing direct-Kick mint + insert + rotation path ...
    Ok(AdminActionResult::Completed)
```

- [ ] **Step 4: Add minting helpers**

Locate where existing `mint_*` helpers are defined (search for `fn mint_` in `src-tauri/src/`). Add adjacent:

```rust
/// ZEB-250: mint a signed AdminProposal carrying a SetPower
/// proposal_kind, sign with the caller's identity, insert into the
/// community CRDT, and return the inserted event.
fn mint_admin_proposal_set_power(
    state: &Mutex<NodeState>,
    community_id: SpaceId,
    target: OwnerAddr,
    level: u8,
) -> Result<SignedMembershipEvent, MembershipMintError> {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::SetPower { target, level },
    };
    mint_and_insert_membership_event(state, community_id, kind)
}

/// ZEB-250: mint a signed AdminProposal carrying a Kick proposal_kind.
fn mint_admin_proposal_kick(
    state: &Mutex<NodeState>,
    community_id: SpaceId,
    target: OwnerAddr,
    reason: Option<String>,
) -> Result<SignedMembershipEvent, MembershipMintError> {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::Kick { target, reason },
    };
    mint_and_insert_membership_event(state, community_id, kind)
}

/// ZEB-250: mint a signed AdminProposal carrying a ChangeQuorum
/// proposal_kind. Used by `propose_change_quorum` (Task 12).
fn mint_admin_proposal_change_quorum(
    state: &Mutex<NodeState>,
    community_id: SpaceId,
    new_quorum: u8,
) -> Result<SignedMembershipEvent, MembershipMintError> {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::ChangeQuorum { new_quorum },
    };
    mint_and_insert_membership_event(state, community_id, kind)
}
```

The `mint_and_insert_membership_event` helper either already exists (look for the ZEB-254 PendingJoin minting code) OR will need to be factored out from an existing inline mint. If you factor it out, do so in a single tight refactor and verify all callers still produce identical events.

`MembershipMintError` may be `String` or a dedicated enum; match the surrounding code's pattern.

- [ ] **Step 5: Update IPC tests + adjust frontend invokes**

The IPC return type changed. Search frontend for `invoke('set_power_level', ...)` and `invoke('kick_from_community', ...)`:

```bash
grep -rn "invoke.*set_power_level\|invoke.*kick_from_community" src/
```

Each invoke site must handle the new discriminated return type. For now (in this task) update them to:

```typescript
const result = await adapter.invoke<AdminActionResult>('set_power_level', {
  communityId,
  targetAddr,
  level,
});
if (result.kind === 'Pending') {
  // show "Proposal submitted, 1 of N signatures" toast
} else {
  // show "Done" toast (existing behavior)
}
```

Add the matching TypeScript type in `src/lib/types.ts` (or wherever IPC DTOs live):

```typescript
export type AdminActionResult =
  | { kind: 'Completed' }
  | {
      kind: 'Pending';
      proposal_event_id: string;
      signers_so_far: number;
      quorum_required: number;
    };
```

- [ ] **Step 6: Add IPC unit tests**

Per spec §8.4. Add to whichever existing test module hosts IPC tests (search `#[tokio::test]` in the file for examples):

```rust
    #[tokio::test]
    async fn set_power_level_returns_completed_when_quorum_1() {
        // Backwards-compat: admin_quorum=1 (default) -> Completed.
        // ... bootstrap a community with single admin, target as regular member ...
        let result = set_power_level(/* args */).await;
        match result {
            Ok(AdminActionResult::Completed) => {} // expected
            other => panic!("expected Completed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn set_power_level_routes_to_proposal_when_quorum_above_1_and_target_becomes_admin() {
        // ... bootstrap a 2-admin community at quorum=2, then promote a regular user ...
        let result = set_power_level(/* args */).await;
        match result {
            Ok(AdminActionResult::Pending { signers_so_far, quorum_required, .. }) => {
                assert_eq!(signers_so_far, 1);
                assert_eq!(quorum_required, 2);
            }
            other => panic!("expected Pending, got {:?}", other),
        }
    }
```

- [ ] **Step 7: Run full gates**

Expected: all green. Both Rust and frontend tests pass.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src/lib/
git commit -m "feat(zeb-250): auto-route set_power_level / kick_from_community based on admin_quorum

Per spec §6.1. Existing IPCs now return AdminActionResult{Completed|Pending}.
When admin_quorum > 1 AND action is admin-affecting, the handler mints
an AdminProposal (signers_so_far=1) instead of the direct event.
Otherwise routes to the existing direct-event path (backwards-compat).

Frontend invoke sites updated to handle the discriminated return."
```

---

## Task 10: `list_pending_admin_proposals` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` — add command, DTO, helper.

**Background:** Spec §6.2. Returns `Vec<PendingAdminProposalDto>` filtered to pending + recent (effective) + expired sections, each card resolves proposer + target display names.

- [ ] **Step 1: Define the DTO**

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingAdminProposalDto {
    pub event_id: String,
    pub proposer_addr: String,
    pub proposer_display_name: Option<String>,
    pub proposal_kind: ProposalKindDto,
    pub proposed_at_wall_ms: u64,
    pub signers_so_far: u8,
    pub quorum_required: u8,
    pub expired: bool,
    pub effective: bool,
    pub self_has_signed: bool,
    pub signer_display_names: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum ProposalKindDto {
    SetPower {
        target_addr: String,
        target_display_name: Option<String>,
        level: u8,
    },
    Kick {
        target_addr: String,
        target_display_name: Option<String>,
        reason: Option<String>,
    },
    ChangeQuorum {
        new_quorum: u8,
    },
}
```

- [ ] **Step 2: Add the IPC**

```rust
#[tauri::command]
async fn list_pending_admin_proposals(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<PendingAdminProposalDto>, String> {
    let community_id_parsed: SpaceId = community_id
        .parse()
        .map_err(|e: SpaceIdParseError| e.to_string())?;

    let state_g = state.lock().await;
    let runtime = state_g
        .runtime
        .as_ref()
        .ok_or("list_pending_admin_proposals: node not started")?;
    let registry = runtime
        .community_registry()
        .ok_or("list_pending_admin_proposals: community_registry detached")?;
    let community = registry
        .get(&community_id_parsed)
        .ok_or("list_pending_admin_proposals: community not joined")?;

    let admin_addr = /* lookup community's admin_addr — same pattern as other IPCs */;
    let caller_addr = runtime.identity_addr();
    let materialized = community.lock().materialized(admin_addr);

    // F1 authorization: caller is Joined + power >= 100.
    let caller_status = materialized.members.get(&caller_addr).map(|s| s.status);
    if !matches!(caller_status, Some(MemberStatus::Joined)) {
        return Err("list_pending_admin_proposals: caller is not a Joined member".to_string());
    }
    let caller_power = materialized.power_levels.get(&caller_addr).copied().unwrap_or(0);
    if caller_power < 100 {
        return Err(format!(
            "list_pending_admin_proposals: caller power {} below admin threshold 100",
            caller_power
        ));
    }

    // Walk event log, filtering for AdminProposal.
    let community_locked = community.lock();
    let admin_quorum = materialized.admin_quorum;
    let now_wall_ms = current_wall_ms();

    let mut dtos: Vec<PendingAdminProposalDto> = Vec::new();
    for (_, event) in &community_locked.events {
        if let MembershipEventKind::AdminProposal { proposal_kind } = &event.kind {
            // Compute signers via materialize's pre-pass logic.
            let signers: Vec<OwnerAddr> = community_locked
                .events
                .values()
                .filter_map(|e| match &e.kind {
                    MembershipEventKind::AdminProposal { .. } if e.id == event.id => {
                        Some(e.actor)
                    }
                    MembershipEventKind::AdminCountersign { target_event_id }
                        if *target_event_id == event.id =>
                    {
                        Some(e.actor)
                    }
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let signers_so_far = signers.len() as u8;

            // Has the proposal expired? — proposer wall_ms + 30d < now.
            let expired = now_wall_ms.saturating_sub(event.at.wall_ms)
                > ADMIN_PROPOSAL_EXPIRY_MS
                && signers_so_far < admin_quorum;

            // Is it effective? — at materialize time, quorum reached within expiry.
            let effective = signers_so_far >= admin_quorum && {
                // Find the Nth-signer wall_ms.
                let mut signing_hlcs: Vec<u64> = community_locked
                    .events
                    .values()
                    .filter(|e| match &e.kind {
                        MembershipEventKind::AdminProposal { .. } => e.id == event.id,
                        MembershipEventKind::AdminCountersign { target_event_id } => {
                            *target_event_id == event.id
                        }
                        _ => false,
                    })
                    .map(|e| e.at.wall_ms)
                    .collect();
                signing_hlcs.sort();
                signing_hlcs
                    .get((admin_quorum as usize).saturating_sub(1))
                    .map(|ms| ms.saturating_sub(event.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS)
                    .unwrap_or(false)
            };

            let self_has_signed = signers.iter().any(|a| *a == caller_addr);
            let signer_display_names = signers
                .iter()
                .filter_map(|addr| resolve_display_name(&state_g, *addr))
                .collect();

            let kind_dto = match proposal_kind {
                ProposalKind::SetPower { target, level } => ProposalKindDto::SetPower {
                    target_addr: hex::encode(target.0),
                    target_display_name: resolve_display_name(&state_g, *target),
                    level: *level,
                },
                ProposalKind::Kick { target, reason } => ProposalKindDto::Kick {
                    target_addr: hex::encode(target.0),
                    target_display_name: resolve_display_name(&state_g, *target),
                    reason: reason.clone(),
                },
                ProposalKind::ChangeQuorum { new_quorum } => {
                    ProposalKindDto::ChangeQuorum { new_quorum: *new_quorum }
                }
            };

            dtos.push(PendingAdminProposalDto {
                event_id: hex::encode(event.id),
                proposer_addr: hex::encode(event.actor.0),
                proposer_display_name: resolve_display_name(&state_g, event.actor),
                proposal_kind: kind_dto,
                proposed_at_wall_ms: event.at.wall_ms,
                signers_so_far,
                quorum_required: admin_quorum,
                expired,
                effective,
                self_has_signed,
                signer_display_names,
            });
        }
    }

    // Sort: pending first (chronological), then effective, then expired.
    dtos.sort_by_key(|d| {
        let bucket = if !d.expired && !d.effective {
            0u8
        } else if d.effective {
            1
        } else {
            2
        };
        (bucket, d.proposed_at_wall_ms)
    });

    Ok(dtos)
}
```

`resolve_display_name` is a helper that you may need to factor out — see how `list_pending_joins` (line 14789) resolves names; reuse that pattern. `current_wall_ms()` is likewise a helper that already exists.

- [ ] **Step 3: Register the command in the Tauri builder**

In the `tauri::Builder::default()` chain (search `.invoke_handler`), add `list_pending_admin_proposals` to the list of registered commands.

- [ ] **Step 4: Add IPC unit tests**

Spec §8.4 tests 1-3:

- `list_pending_admin_proposals_rejects_non_admin_caller`
- `list_pending_admin_proposals_returns_pending_and_recent_sections`
- `list_pending_admin_proposals_resolves_proposer_and_signer_names`

For each, bootstrap a community state in-memory, build the prerequisites, call the IPC, assert the returned DTO list.

- [ ] **Step 5: Run gates + commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-250): list_pending_admin_proposals IPC + DTO

Per spec §6.2. Admin-only IPC walks community event log, computes
signers/expired/effective/self_has_signed per AdminProposal, resolves
proposer+signer display names, sorts pending → effective → expired.

3 new unit tests covering authorization + ordering + name resolution."
```

---

## Task 11: `countersign_admin_proposal` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` — add command, return type, mint helper.

**Background:** Spec §6.3.

- [ ] **Step 1: Define return type**

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct CountersignResult {
    pub signers_after: u8,
    pub quorum_required: u8,
    pub reached_quorum: bool,
}
```

- [ ] **Step 2: Add the IPC**

```rust
#[tauri::command]
async fn countersign_admin_proposal(
    community_id: String,
    proposal_event_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CountersignResult, String> {
    let community_id_parsed: SpaceId =
        community_id.parse().map_err(|e: SpaceIdParseError| e.to_string())?;
    let proposal_event_id_bytes: [u8; 16] = hex::decode(&proposal_event_id)
        .map_err(|e| format!("countersign_admin_proposal: invalid event_id hex: {}", e))?
        .as_slice()
        .try_into()
        .map_err(|_| "countersign_admin_proposal: event_id must be 16 bytes".to_string())?;

    let state_g = state.lock().await;
    let runtime = state_g
        .runtime
        .as_ref()
        .ok_or("countersign_admin_proposal: node not started")?;
    let registry = runtime
        .community_registry()
        .ok_or("countersign_admin_proposal: community_registry detached")?;
    let community = registry
        .get(&community_id_parsed)
        .ok_or("countersign_admin_proposal: community not joined")?;

    let admin_addr = /* same lookup as elsewhere */;
    let caller_addr = runtime.identity_addr();
    let mut community_locked = community.lock();
    let materialized = community_locked.materialized(admin_addr);

    let caller_status = materialized.members.get(&caller_addr).map(|s| s.status);
    if !matches!(caller_status, Some(MemberStatus::Joined)) {
        return Err("countersign_admin_proposal: caller is not Joined".to_string());
    }
    let caller_power = materialized
        .power_levels
        .get(&caller_addr)
        .copied()
        .unwrap_or(0);
    if caller_power < 100 {
        return Err(format!(
            "countersign_admin_proposal: caller power {} below admin threshold 100",
            caller_power
        ));
    }

    // Look up the target proposal.
    let target_event = community_locked
        .events
        .get(&proposal_event_id_bytes)
        .ok_or_else(|| {
            format!(
                "countersign_admin_proposal: proposal {} not found",
                proposal_event_id
            )
        })?;
    if !matches!(target_event.kind, MembershipEventKind::AdminProposal { .. }) {
        return Err(format!(
            "countersign_admin_proposal: event {} is not an AdminProposal",
            proposal_event_id
        ));
    }
    // Expiry check.
    let now_wall_ms = current_wall_ms();
    let age = now_wall_ms.saturating_sub(target_event.at.wall_ms);
    if age > ADMIN_PROPOSAL_EXPIRY_MS {
        return Err("countersign_admin_proposal: proposal has expired".to_string());
    }

    // Already signed? — idempotent: return current state.
    let already = community_locked.events.values().any(|e| match &e.kind {
        MembershipEventKind::AdminProposal { .. } => {
            e.id == proposal_event_id_bytes && e.actor == caller_addr
        }
        MembershipEventKind::AdminCountersign { target_event_id } => {
            *target_event_id == proposal_event_id_bytes && e.actor == caller_addr
        }
        _ => false,
    });
    if already {
        // Idempotent — just report current state.
        let signers_after = count_signers(&community_locked, proposal_event_id_bytes);
        let quorum_required = materialized.admin_quorum;
        return Ok(CountersignResult {
            signers_after,
            quorum_required,
            reached_quorum: signers_after >= quorum_required,
        });
    }

    // Mint AdminCountersign.
    let kind = MembershipEventKind::AdminCountersign {
        target_event_id: proposal_event_id_bytes,
    };
    let event = mint_and_insert_membership_event_locked(
        &mut community_locked,
        &state_g,
        community_id_parsed,
        kind,
    )
    .map_err(|e| e.to_string())?;
    drop(event); // unused locally; the insert was the side effect

    // Recompute signers.
    let signers_after = count_signers(&community_locked, proposal_event_id_bytes);
    let post_materialized = community_locked.materialized(admin_addr);
    let quorum_required = post_materialized.admin_quorum;
    Ok(CountersignResult {
        signers_after,
        quorum_required,
        reached_quorum: signers_after >= quorum_required,
    })
}

fn count_signers(community: &CommunityState, proposal_id: [u8; 16]) -> u8 {
    community
        .events
        .values()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::AdminProposal { .. } if e.id == proposal_id => Some(e.actor),
            MembershipEventKind::AdminCountersign { target_event_id }
                if *target_event_id == proposal_id =>
            {
                Some(e.actor)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len() as u8
}
```

`mint_and_insert_membership_event_locked` mints + signs + inserts, returning the inserted event. Refactor from existing minting code if no such helper exists.

- [ ] **Step 3: Register the command + add 4 IPC unit tests**

Per spec §8.4 tests 4-7:

- `countersign_admin_proposal_idempotent_when_already_signed`
- `countersign_admin_proposal_rejects_non_admin_caller`
- `countersign_admin_proposal_rejects_expired_proposal`
- `countersign_admin_proposal_returns_reached_quorum_true_on_threshold_tip`

- [ ] **Step 4: Run gates + commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-250): countersign_admin_proposal IPC

Per spec §6.3. Admin-only IPC mints AdminCountersign event targeting
the given proposal. Idempotent on re-sign (returns current state).
Rejects expired (>30d) or non-existent proposals.

4 new unit tests."
```

---

## Task 12: `propose_change_quorum` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` — add command.

**Background:** Spec §6.4. Validates `new_quorum` per AP5, mints AdminProposal with ChangeQuorum kind, returns `AdminActionResult` (Completed if quorum=1 self-satisfies; Pending otherwise).

- [ ] **Step 1: Add the IPC**

```rust
#[tauri::command]
async fn propose_change_quorum(
    community_id: String,
    new_quorum: u8,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<AdminActionResult, String> {
    let community_id_parsed: SpaceId =
        community_id.parse().map_err(|e: SpaceIdParseError| e.to_string())?;
    if new_quorum < 1 {
        return Err("propose_change_quorum: new_quorum must be >= 1".to_string());
    }

    let state_g = state.lock().await;
    let runtime = state_g
        .runtime
        .as_ref()
        .ok_or("propose_change_quorum: node not started")?;
    let registry = runtime
        .community_registry()
        .ok_or("propose_change_quorum: community_registry detached")?;
    let community = registry
        .get(&community_id_parsed)
        .ok_or("propose_change_quorum: community not joined")?;

    let admin_addr = /* same lookup */;
    let caller_addr = runtime.identity_addr();
    let materialized = community.lock().materialized(admin_addr);

    // Auth: caller power >= 100.
    let caller_status = materialized.members.get(&caller_addr).map(|s| s.status);
    if !matches!(caller_status, Some(MemberStatus::Joined)) {
        return Err("propose_change_quorum: caller is not Joined".to_string());
    }
    let caller_power = materialized
        .power_levels
        .get(&caller_addr)
        .copied()
        .unwrap_or(0);
    if caller_power < 100 {
        return Err(format!(
            "propose_change_quorum: caller power {} below admin threshold 100",
            caller_power
        ));
    }

    // Range: new_quorum <= current admin count.
    let admin_count = materialized
        .power_levels
        .values()
        .filter(|p| **p == 100)
        .count() as u32;
    if (new_quorum as u32) > admin_count {
        return Err(format!(
            "propose_change_quorum: new_quorum {} exceeds current admin count {}",
            new_quorum, admin_count
        ));
    }

    let current_quorum = materialized.admin_quorum;

    // Mint the proposal. Under quorum=1, proposer's sole signature
    // self-satisfies; materialize will apply effect immediately on
    // next read.
    let proposal_event = mint_admin_proposal_change_quorum(
        &state,
        community_id_parsed,
        new_quorum,
    )
    .map_err(|e| e.to_string())?;

    if current_quorum == 1 {
        Ok(AdminActionResult::Completed)
    } else {
        Ok(AdminActionResult::Pending {
            proposal_event_id: hex::encode(proposal_event.id),
            signers_so_far: 1,
            quorum_required: current_quorum,
        })
    }
}
```

- [ ] **Step 2: Register + add 1 IPC test**

Per spec §8.4 test 10:

- `propose_change_quorum_rejects_out_of_range_values`

- [ ] **Step 3: Run gates + commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-250): propose_change_quorum IPC

Per spec §6.4. Mints AdminProposal{ChangeQuorum}. Validates new_quorum
in [1, current_admin_count]. Under quorum=1 proposer's signature
self-satisfies (Completed); else returns Pending."
```

---

## Task 13: `PendingAdminProposalsPanel.svelte` component

**Files:**
- Create: `src/lib/components/PendingAdminProposalsPanel.svelte`
- Create: `src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts`

**Background:** Spec §7.1. Admin-only mount. Three sections: pending / recently approved / expired. Per-card "Countersign" button. Svelte 5 `$props()` destructuring + `$effect` + `latestCallId` / `latestWatchId` patterns from ZEB-287.

- [ ] **Step 1: Read PendingJoinsPanel.svelte for the reference pattern**

```bash
cat src/lib/components/PendingJoinsPanel.svelte
```

It demonstrates Svelte 5 idioms used throughout the codebase: `$props()` destructuring, `$effect` registration, `latestCallId` for stale-response discarding, IPC invocation with proper error handling.

- [ ] **Step 2: Build the panel**

Structure:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { adapter } from '../adapter';

  type ProposalKindDto =
    | { kind: 'SetPower'; target_addr: string; target_display_name: string | null; level: number }
    | { kind: 'Kick'; target_addr: string; target_display_name: string | null; reason: string | null }
    | { kind: 'ChangeQuorum'; new_quorum: number };

  type PendingAdminProposalDto = {
    event_id: string;
    proposer_addr: string;
    proposer_display_name: string | null;
    proposal_kind: ProposalKindDto;
    proposed_at_wall_ms: number;
    signers_so_far: number;
    quorum_required: number;
    expired: boolean;
    effective: boolean;
    self_has_signed: boolean;
    signer_display_names: string[];
  };

  type CountersignResult = {
    signers_after: number;
    quorum_required: number;
    reached_quorum: boolean;
  };

  let { communityId, canAdmin, selfOwnerAddr }: {
    communityId: string;
    canAdmin: boolean;
    selfOwnerAddr: string;
  } = $props();

  let proposals: PendingAdminProposalDto[] = $state([]);
  let loading = $state(false);
  let errorMessage: string | null = $state(null);
  let latestCallId = 0;
  let latestWatchId = 0;
  let unsubConverged: (() => void) | null = null;

  async function refresh() {
    if (!canAdmin) {
      // Bump latestCallId so any in-flight refresh from before canAdmin
      // flipped to false is discarded.
      latestCallId++;
      proposals = [];
      return;
    }
    const myCallId = ++latestCallId;
    loading = true;
    errorMessage = null;
    try {
      const result = await adapter.invoke<PendingAdminProposalDto[]>(
        'list_pending_admin_proposals',
        { communityId }
      );
      if (myCallId !== latestCallId) return; // stale
      proposals = result;
    } catch (e) {
      if (myCallId !== latestCallId) return;
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    } finally {
      if (myCallId === latestCallId) loading = false;
    }
  }

  async function countersign(eventId: string) {
    try {
      const result = await adapter.invoke<CountersignResult>(
        'countersign_admin_proposal',
        { communityId, proposalEventId: eventId }
      );
      // Optimistic refresh.
      await refresh();
      return result;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
      return null;
    }
  }

  $effect(() => {
    const myWatchId = ++latestWatchId;
    void communityId;
    void canAdmin;
    refresh();

    // Listen to community-state-sync-converged for live updates.
    if (canAdmin) {
      const handler = () => {
        if (myWatchId !== latestWatchId) return;
        refresh();
      };
      const cleanup = adapter.listen('community-state-sync-converged', handler);
      unsubConverged = () => {
        Promise.resolve(cleanup).then((unsub) => unsub?.());
      };
    } else {
      unsubConverged?.();
      unsubConverged = null;
    }
  });

  onDestroy(() => {
    unsubConverged?.();
    unsubConverged = null;
  });

  // Bucket sort: pending → effective → expired.
  let pendingProposals = $derived(
    proposals.filter((p) => !p.expired && !p.effective)
  );
  let effectiveProposals = $derived(proposals.filter((p) => p.effective));
  let expiredProposals = $derived(proposals.filter((p) => p.expired));

  function proposalSummary(p: PendingAdminProposalDto): string {
    const kind = p.proposal_kind;
    switch (kind.kind) {
      case 'SetPower': {
        const name = kind.target_display_name ?? kind.target_addr.slice(0, 8);
        if (kind.level === 100) return `Promote @${name} to admin`;
        if (kind.level === 0) return `Demote @${name} from admin`;
        return `Change @${name}'s power to ${kind.level}`;
      }
      case 'Kick': {
        const name = kind.target_display_name ?? kind.target_addr.slice(0, 8);
        return `Kick @${name}`;
      }
      case 'ChangeQuorum':
        return `Change quorum to ${kind.new_quorum}`;
    }
  }

  function daysRemaining(wall_ms: number): number {
    const elapsed_ms = Date.now() - wall_ms;
    const remaining_ms = 30 * 24 * 60 * 60 * 1000 - elapsed_ms;
    return Math.max(0, Math.ceil(remaining_ms / (24 * 60 * 60 * 1000)));
  }
</script>

{#if canAdmin}
  <section aria-label="Admin actions" class="admin-proposals-panel">
    <h3>Admin actions</h3>
    {#if loading}
      <p>Loading...</p>
    {/if}
    {#if errorMessage}
      <p class="error">{errorMessage}</p>
    {/if}

    {#if pendingProposals.length > 0}
      <h4>Pending — {pendingProposals.length} awaiting signatures</h4>
      <ul role="list">
        {#each pendingProposals as p (p.event_id)}
          <li aria-label={`Pending admin proposal: ${proposalSummary(p)}`}>
            <div class="proposal-card">
              <div class="summary">{proposalSummary(p)}</div>
              <div class="meta">
                Proposed by @{p.proposer_display_name ?? p.proposer_addr.slice(0, 8)}
                · Signed {p.signers_so_far} of {p.quorum_required}
                · {daysRemaining(p.proposed_at_wall_ms)} days remaining
              </div>
              {#if p.proposal_kind.kind === 'Kick' && p.proposal_kind.reason}
                <div class="reason">Reason: {p.proposal_kind.reason}</div>
              {/if}
              <button
                disabled={p.self_has_signed || p.expired || p.effective}
                aria-label={`Countersign: ${proposalSummary(p)}`}
                onclick={() => countersign(p.event_id)}
              >
                {p.self_has_signed ? 'Already signed ✓' : 'Countersign'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}

    {#if effectiveProposals.length > 0}
      <details>
        <summary>Recently approved ({effectiveProposals.length})</summary>
        <ul role="list">
          {#each effectiveProposals as p (p.event_id)}
            <li><div class="proposal-card effective">{proposalSummary(p)}</div></li>
          {/each}
        </ul>
      </details>
    {/if}

    {#if expiredProposals.length > 0}
      <details>
        <summary>Expired without quorum ({expiredProposals.length})</summary>
        <ul role="list">
          {#each expiredProposals as p (p.event_id)}
            <li><div class="proposal-card expired">{proposalSummary(p)}</div></li>
          {/each}
        </ul>
      </details>
    {/if}

    {#if pendingProposals.length === 0 && effectiveProposals.length === 0 && expiredProposals.length === 0 && !loading}
      <p>No admin proposals yet.</p>
    {/if}
  </section>
{/if}

<style>
  .admin-proposals-panel { margin-block: 1rem; }
  .proposal-card { border: 1px solid var(--border, #ccc); padding: 0.75rem; margin-block: 0.5rem; }
  .summary { font-weight: 600; }
  .meta { font-size: 0.9em; color: var(--muted, #666); margin-block: 0.25rem; }
  .reason { font-style: italic; margin-block: 0.25rem; }
  .error { color: var(--error, #c00); }
  .effective { opacity: 0.7; }
  .expired { opacity: 0.5; }
</style>
```

Note: Svelte 5 syntax — `$props()`, `$state()`, `$derived()`, `$effect()`. Match the codebase's existing component conventions.

`selfOwnerAddr` is currently unused in the JS body but kept on the prop interface for consistency with spec §7.1 (it's used to mark `self_has_signed` — that's already computed backend-side in `PendingAdminProposalDto.self_has_signed`, so the prop is technically unused; keep it for future use OR remove it from the prop interface — pick one and stay consistent).

Actually — looking at the DTO, `self_has_signed` is computed server-side. `selfOwnerAddr` is unused. Remove it from the prop interface:

```typescript
let { communityId, canAdmin }: {
  communityId: string;
  canAdmin: boolean;
} = $props();
```

- [ ] **Step 3: Add vitest spec**

Create `src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts`. Per spec §8.5:

```typescript
import { render, screen } from '@testing-library/svelte';
import { vi, describe, it, expect, beforeEach } from 'vitest';
import PendingAdminProposalsPanel from '../PendingAdminProposalsPanel.svelte';
import { adapter } from '../../adapter';

vi.mock('../../adapter', () => ({
  adapter: {
    invoke: vi.fn(),
    listen: vi.fn().mockReturnValue(() => {}),
  },
}));

describe('PendingAdminProposalsPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('non_admin_skips_fetch_and_listen_registration', () => {
    render(PendingAdminProposalsPanel, {
      props: { communityId: 'community-x', canAdmin: false },
    });
    expect(adapter.invoke).not.toHaveBeenCalled();
    expect(adapter.listen).not.toHaveBeenCalled();
  });

  it('renders_pending_proposal_cards_with_signers_count', async () => {
    vi.mocked(adapter.invoke).mockResolvedValueOnce([
      {
        event_id: 'aa'.repeat(16),
        proposer_addr: '11'.repeat(16),
        proposer_display_name: 'alice',
        proposal_kind: { kind: 'SetPower', target_addr: '22'.repeat(16), target_display_name: 'bob', level: 100 },
        proposed_at_wall_ms: Date.now(),
        signers_so_far: 1,
        quorum_required: 2,
        expired: false,
        effective: false,
        self_has_signed: false,
        signer_display_names: ['alice'],
      },
    ]);
    render(PendingAdminProposalsPanel, {
      props: { communityId: 'community-x', canAdmin: true },
    });
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.getByText(/Promote @bob to admin/)).toBeTruthy();
    expect(screen.getByText(/Signed 1 of 2/)).toBeTruthy();
  });

  // ... add remaining 6 tests from spec §8.5
});
```

Add all 8 tests from spec §8.5:

1. `non_admin_skips_fetch_and_listen_registration`
2. `renders_pending_proposal_cards_with_signers_count`
3. `countersign_button_disabled_when_self_already_signed`
4. `countersign_button_disabled_for_expired_proposals`
5. `recently_approved_section_renders_separately_when_collapsed_by_default`
6. `countersign_click_invokes_ipc_and_updates_optimistically`
7. `community_state_sync_converged_event_triggers_refresh`
8. `stale_async_response_after_communityid_change_is_discarded`

- [ ] **Step 4: Run frontend gates**

```bash
npx tsc --noEmit && npx vitest run --reporter=verbose src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts
```

Expected: all 8 pass.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PendingAdminProposalsPanel.svelte src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts
git commit -m "feat(zeb-250): PendingAdminProposalsPanel.svelte (admin-only)

Per spec §7.1. Svelte 5 idioms: \$props destructuring, \$effect for
refresh-on-mount + listen registration, latestCallId for stale-response
discarding. Three sections — pending / recently approved / expired —
with per-card Countersign button gated on self_has_signed / expired /
effective.

8 new vitest specs."
```

---

## Task 14: `ChangeQuorumDialog.svelte` component

**Files:**
- Create: `src/lib/components/ChangeQuorumDialog.svelte`
- Create: `src/lib/components/__tests__/ChangeQuorumDialog.test.ts`

**Background:** Spec §7.3. Slider + paired number input (per `feedback_slider_pair_with_number_input` memory). Range `[1, current_admin_count]`. Invokes `propose_change_quorum`.

- [ ] **Step 1: Build the dialog**

```svelte
<script lang="ts">
  import { adapter } from '../adapter';

  type AdminActionResult =
    | { kind: 'Completed' }
    | { kind: 'Pending'; proposal_event_id: string; signers_so_far: number; quorum_required: number };

  let {
    communityId,
    currentQuorum,
    currentAdminCount,
    onClose,
  }: {
    communityId: string;
    currentQuorum: number;
    currentAdminCount: number;
    onClose: () => void;
  } = $props();

  let proposedQuorum = $state(currentQuorum);
  let submitting = $state(false);
  let errorMessage: string | null = $state(null);

  // Bidirectional sync: slider + number input share the same $state.
  // Both bind to proposedQuorum.

  async function propose() {
    if (proposedQuorum < 1 || proposedQuorum > currentAdminCount) {
      errorMessage = `Quorum must be between 1 and ${currentAdminCount}.`;
      return;
    }
    submitting = true;
    errorMessage = null;
    try {
      const result = await adapter.invoke<AdminActionResult>('propose_change_quorum', {
        communityId,
        newQuorum: proposedQuorum,
      });
      if (result.kind === 'Completed') {
        // Quorum=1 self-satisfied; close.
        onClose();
      } else {
        // Pending; close — pending will appear in PendingAdminProposalsPanel.
        onClose();
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    } finally {
      submitting = false;
    }
  }
</script>

<dialog open class="change-quorum-dialog" aria-label="Change admin quorum">
  <h2>Change admin quorum</h2>
  <p>
    With quorum of {proposedQuorum}, admin actions need {proposedQuorum} signatures from
    current admins. We recommend at least N+1 admins for survivability.
  </p>

  <label>
    <input
      type="range"
      min={1}
      max={currentAdminCount}
      bind:value={proposedQuorum}
      aria-label="Quorum slider"
    />
  </label>
  <label>
    Quorum:
    <input
      type="number"
      min={1}
      max={currentAdminCount}
      bind:value={proposedQuorum}
      aria-label="Quorum number"
    />
    of {currentAdminCount} admins
  </label>

  {#if errorMessage}
    <p class="error">{errorMessage}</p>
  {/if}

  <div class="actions">
    <button onclick={onClose} disabled={submitting}>Cancel</button>
    <button
      onclick={propose}
      disabled={submitting || proposedQuorum < 1 || proposedQuorum > currentAdminCount || proposedQuorum === currentQuorum}
    >
      Propose
    </button>
  </div>
</dialog>

<style>
  .change-quorum-dialog { padding: 1.5rem; min-width: 24rem; }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem; }
  .error { color: var(--error, #c00); }
</style>
```

- [ ] **Step 2: Add vitest spec**

Per spec §8.5 ChangeQuorumDialog block — 4 tests:

1. `slider_and_number_input_sync_bidirectionally`
2. `propose_button_disabled_when_quorum_outside_valid_range`
3. `propose_invokes_propose_change_quorum_ipc_with_new_value`
4. `explainer_text_present_for_survivability_recommendation`

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { vi, describe, it, expect, beforeEach } from 'vitest';
import ChangeQuorumDialog from '../ChangeQuorumDialog.svelte';
import { adapter } from '../../adapter';

vi.mock('../../adapter', () => ({
  adapter: { invoke: vi.fn() },
}));

describe('ChangeQuorumDialog', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('slider_and_number_input_sync_bidirectionally', async () => {
    render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 1,
        currentAdminCount: 3,
        onClose: vi.fn(),
      },
    });
    const slider = screen.getByLabelText('Quorum slider') as HTMLInputElement;
    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '2' } });
    expect(number.value).toBe('2');
    await fireEvent.input(number, { target: { value: '3' } });
    expect(slider.value).toBe('3');
  });

  // ... add remaining 3 tests
});
```

- [ ] **Step 3: Run frontend gates + commit**

```bash
npx tsc --noEmit && npx vitest run --reporter=verbose src/lib/components/__tests__/ChangeQuorumDialog.test.ts
```

Expected: 4/4 pass.

```bash
git add src/lib/components/ChangeQuorumDialog.svelte src/lib/components/__tests__/ChangeQuorumDialog.test.ts
git commit -m "feat(zeb-250): ChangeQuorumDialog.svelte (slider + paired number)

Per spec §7.3. Bidirectional slider + number input syncing via shared
\$state. Range [1, current_admin_count]. Propose disabled when outside
range OR equal to current. Invokes propose_change_quorum on confirm.

4 new vitest specs."
```

---

## Task 15: Mount in `CommunitySettingsPanel.svelte` + member-list badges

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` — add "Admin governance" section + member-list badges.
- Modify: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` — augment.

**Background:** Spec §7.2 + §7.3 + §7.4. The "Admin governance" section appears just above PendingAdminProposalsPanel; the "Change quorum…" button opens ChangeQuorumDialog. Member-list rows show pending-state badges for members targeted by an active AdminProposal.

- [ ] **Step 1: Add Admin governance section to CommunitySettingsPanel**

Read the file first; the existing pattern is well established. Find a logical placement (after the existing "Members" section, before "Forks"). Add:

```svelte
{#if canAdmin}
  <section aria-label="Admin governance" class="admin-governance">
    <h2>Admin governance</h2>
    <p>
      Current admin quorum: {currentAdminQuorum} of {currentAdminCount} admins required for
      admin-affecting actions.
    </p>
    <button onclick={() => (showChangeQuorumDialog = true)}>Change quorum…</button>

    <PendingAdminProposalsPanel {communityId} {canAdmin} />
  </section>

  {#if showChangeQuorumDialog}
    <ChangeQuorumDialog
      {communityId}
      currentQuorum={currentAdminQuorum}
      currentAdminCount={currentAdminCount}
      onClose={() => (showChangeQuorumDialog = false)}
    />
  {/if}
{/if}
```

`currentAdminQuorum` + `currentAdminCount` come from the parent's materialized view of the community state. Hook into the existing materialized-state derivation in the file (search for `power_levels` references). If the existing component doesn't read `admin_quorum`, add a derived getter.

Imports at the top of the script block:

```typescript
import PendingAdminProposalsPanel from './PendingAdminProposalsPanel.svelte';
import ChangeQuorumDialog from './ChangeQuorumDialog.svelte';

let showChangeQuorumDialog = $state(false);
```

- [ ] **Step 2: Compute `currentAdminQuorum` + `currentAdminCount` from state**

The existing CommunitySettingsPanel already fetches the materialized view (look for invocations of `list_community_members` or similar). Add:

```typescript
let pendingProposalsByTarget = $state<Map<string, PendingAdminProposalDto>>(new Map());
// Refresh when proposals load via the panel — see Step 4.

// Derive from existing materialized state:
let currentAdminQuorum = $derived(materializedState.admin_quorum ?? 1);
let currentAdminCount = $derived(
  Object.values(materializedState.power_levels ?? {}).filter((p) => p === 100).length
);
```

The exact shape depends on what the existing component already has. Adapt.

- [ ] **Step 3: Member-list pending-state badges**

In the existing member-row rendering (search for the `{#each members as member}` loop), add:

```svelte
{#each members as member (member.addr)}
  <li>
    <span>@{member.display_name ?? member.addr.slice(0, 8)}</span>
    <span class="power">— {memberLabel(member.power)}</span>
    {#if pendingProposalsByTarget.has(member.addr)}
      {@const pending = pendingProposalsByTarget.get(member.addr)}
      <span class="pending-badge" aria-label={pendingBadgeLabel(pending)}>
        ⏳ {pendingBadgeText(pending)}
      </span>
    {/if}
  </li>
{/each}

<script lang="ts">
  function pendingBadgeText(p: PendingAdminProposalDto): string {
    const kind = p.proposal_kind;
    if (kind.kind === 'SetPower' && kind.level === 100) return 'pending promotion to admin';
    if (kind.kind === 'SetPower' && kind.level === 0) return 'pending demotion';
    if (kind.kind === 'Kick') return 'pending kick';
    return 'pending action';
  }
  function pendingBadgeLabel(p: PendingAdminProposalDto): string {
    return `Pending: ${pendingBadgeText(p)}`;
  }
</script>
```

The `pendingProposalsByTarget` map is built by reading `list_pending_admin_proposals` once and indexing by `target_addr` (when the proposal_kind is SetPower or Kick).

- [ ] **Step 4: Augment vitest spec**

Per spec §8.5 CommunitySettingsPanel block — 3 tests:

1. `admin_governance_section_renders_for_admin`
2. `admin_governance_section_hidden_for_non_admin`
3. `pending_promotion_badge_renders_on_target_member_row`

- [ ] **Step 5: Run frontend gates + commit**

```bash
npx tsc --noEmit && npx vitest run
```

```bash
git add src/lib/components/CommunitySettingsPanel.svelte \
        src/lib/components/__tests__/CommunitySettingsPanel.test.ts
git commit -m "feat(zeb-250): Admin governance section + member-list pending badges

Per spec §7.2 + §7.3 + §7.4. Mounts PendingAdminProposalsPanel +
ChangeQuorumDialog in CommunitySettingsPanel under admin-only gate.
Member rows show ⏳ pending-X badges for members targeted by an
active AdminProposal.

3 new + augmented vitest specs."
```

---

## Task 16: End-to-end integration tests

**Files:**
- Create: `src-tauri/tests/community_admin_quorum_integration.rs`

**Background:** Spec §8.3. Multi-engine scenarios using existing test-fixture infrastructure. Mirror the pattern in `tests/wire_format_zeb254_fixtures.rs` and other community-CRDT integration tests.

- [ ] **Step 1: Create the test file**

```rust
//! ZEB-250: integration tests for M-of-N admin quorum.
//!
//! Multi-engine scenarios exercising end-to-end semantics —
//! AdminProposal verify → materialize → effect application — under
//! realistic conditions (HLC-shuffled event ordering, two/three
//! admins, quorum bootstrap path).

use harmony_app::community_membership::{
    materialize, MaterializedMembership, MemberStatus, MembershipEventKind, ProposalKind,
    SignedMembershipEvent,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
// ... other imports as needed.

#[test]
fn single_admin_community_unaffected_by_zeb250() {
    // Backwards-compat regression. Build a 1-admin community with a
    // historical SetPower/Kick sequence. Materialize matches what it
    // did pre-ZEB-250.
    // ...
}

#[test]
fn two_admin_community_set_power_requires_countersign() {
    // 2-admin community at quorum=2. admin1 proposes promoting bob.
    // No countersign → bob stays regular. admin2 countersigns → bob
    // becomes admin.
    // ...
}

#[test]
fn three_admin_community_kick_admin_requires_two_signatures() {
    // 3-admin community at quorum=2. admin1 proposes kicking admin3.
    // admin2 countersigns → admin3 is kicked. Without admin2's
    // countersign, no effect.
    // ...
}

#[test]
fn change_quorum_bootstrap_path() {
    // Lone admin → promotes second admin (direct, quorum=1) →
    // proposes ChangeQuorum=2 (self-satisfies under quorum=1) →
    // quorum becomes 2. Subsequent direct SetPower of a third
    // admin would now be rejected; AdminProposal route required.
    // ...
}

#[test]
fn lone_admin_loses_keys_community_unrecoverable_except_via_fork() {
    // Single admin, no countersign capability. Community is functional
    // but ungovernable — verify SetPower from a non-admin still
    // rejects (no quorum primitive yet at quorum=1). Fork (out of
    // scope here) is the only recovery; tested in ZEB-285 suites.
    // ...
}

#[test]
fn quorum_reached_late_countersign_is_noop() {
    // Proposal proposed at t=0; admin1 self-signs; quorum=2.
    // admin2 countersigns at t = 31 days. age_when_reached > 30d
    // → no effect.
    // ...
}

#[test]
fn quorum_reached_within_30d_then_aged_past_remains_effective() {
    // Proposal at t=0; quorum reached at t=29d; events continue past
    // 30d. Effect applied. Permanence guaranteed.
    // ...
}

#[test]
fn two_admin_community_admin_leaves_drops_below_quorum() {
    // 2-admin community at quorum=2. admin2 self-leaves (Leave is
    // self-determined, no quorum). Now only admin1 remains. New
    // AdminProposals stuck unless admin1 lowers quorum or community
    // forks.
    // ...
}

#[test]
fn fork_of_quorum_community_resets_to_quorum_1() {
    // Community A at quorum=2. User forks → community B starts at
    // quorum=1 (fresh sovereign per spec §1.4). User is sole admin.
    // ...
}
```

Each test:
1. Constructs a `Vec<SignedMembershipEvent>` representing the scenario.
2. Calls `materialize`.
3. Asserts on the resulting `MaterializedMembership` (members, power_levels, admin_quorum).

Build helpers as needed for clarity. Match the existing patterns in `tests/wire_format_zeb254_fixtures.rs` and any existing `community_*_integration.rs` files for fixture style.

- [ ] **Step 2: Implement the 9 tests**

Each is a 30-80 line test. Use deterministic byte addresses (`[0xNN; 16]`) so tests are debuggable. Use `Hlc { wall_ms: T, logical: 0, device_id: "X".into() }` with explicit wall_ms values rather than `now()` for determinism.

- [ ] **Step 3: Run integration tests + full gates**

```bash
cd src-tauri && cargo nextest run --features test-fixtures --test community_admin_quorum_integration
```

Expected: 9/9 pass. Then run all five CI gates.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/community_admin_quorum_integration.rs
git commit -m "test(zeb-250): integration tests for M-of-N quorum scenarios

9 multi-engine scenarios per spec §8.3:
- single-admin backwards-compat
- 2-admin SetPower requires countersign
- 3-admin Kick requires 2-of-3
- ChangeQuorum bootstrap path (1 → 2)
- lone-admin unrecoverable absent fork
- late countersign is noop
- quorum-reached + aged-past stays effective
- admin leave drops below quorum
- fork resets to quorum=1"
```

---

## Task 17: Final 5-gate sweep + push + PR creation

**Files:** none — verification, push, PR creation only.

- [ ] **Step 1: Run all five gates from a clean state**

```bash
( cd src-tauri && cargo fmt --all -- --check ) && \
( cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings ) && \
( cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures ) && \
npx tsc --noEmit && \
npx vitest run
```

Expected: all green. Test count should be ~+30-44 Rust tests and ~+15-25 vitest tests above the Task 0 baseline (per spec §8.7).

- [ ] **Step 2: Check commit history is clean**

```bash
git log --oneline origin/main..HEAD
```

Expected: spec commit (HEAD~17 from where we'll be) + plan commit (HEAD~16) + 16 implementation commits.

If any commit messages are unclear or duplicates exist, consider a squash or rewording — but only if the diff stays identical. Prefer no rewrites; the per-task commits are the audit trail.

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-250-admin-quorum
```

- [ ] **Step 4: Create the PR**

```bash
gh pr create --title "ZEB-250: M-of-N admin quorum for community governance" --body "$(cat <<'EOF'
## Summary

Closes [ZEB-250](https://linear.app/zeblith/issue/ZEB-250). Generalizes the [ZEB-254](https://linear.app/zeblith/issue/ZEB-254) PendingJoin + JoinCountersign pattern into AdminProposal + AdminCountersign for M-of-N admin governance:

- New per-community `admin_quorum: u8` field on `CommunityState` + `MaterializedMembership` (default 1, CBOR-skip-if-default → byte-compatible with pre-ZEB-250 blobs).
- Two new CRDT event variants `MembershipEventKind::AdminProposal { proposal_kind }` (tag `q`) and `MembershipEventKind::AdminCountersign { target_event_id }` (tag `n`).
- `ProposalKind` enum carrying SetPower / Kick / ChangeQuorum bodies (tagged-union `kd`/`bd`).
- 5-gate verify (AP1-AP5) on AdminProposal + 3-gate verify (AC1-AC3) on AdminCountersign with lenient forward-ref semantics.
- Modified verify on direct SetPower/Kick — rejected when `admin_quorum > 1` AND admin-affecting.
- Materialize **single-pass-with-running-state**: pre-pass collects raw signature data, main pass walks HLC-ordered events maintaining a running `admin_quorum` so a quorum-reached ChangeQuorum mutates the threshold mid-iteration. 30-day expiry on AdminProposals.
- IPC: `set_power_level` and `kick_from_community` auto-route via discriminated `AdminActionResult` (Completed | Pending). New IPCs: `list_pending_admin_proposals`, `countersign_admin_proposal`, `propose_change_quorum`.
- UI: `PendingAdminProposalsPanel.svelte` (admin-only) with pending / recently approved / expired sections + per-card Countersign button. `ChangeQuorumDialog.svelte` with paired slider + number input. Member-list rows show ⏳ pending-X badges for members targeted by an active AdminProposal.

Recovery model: forking ([ZEB-285](https://linear.app/zeblith/issue/ZEB-285)) remains the universal escape-hatch per polycentric-governance memory. Documentation recommends N ≥ M+1 for survivability.

## Spec & plan

- Spec: `docs/specs/2026-05-16-zeb-250-admin-quorum-design.md`
- Plan: `docs/plans/2026-05-16-zeb-250-admin-quorum-plan.md`

## Test plan

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` passes (+30-44 new tests vs. baseline)
- [ ] `npx tsc --noEmit` clean
- [ ] `npx vitest run` passes (+15-25 new tests vs. baseline)
- [ ] Manual smoke test (two-engine, spec §8.6):
  - [ ] Engine A creates community C, invites Engine B
  - [ ] A promotes B to admin (quorum=1 → direct event)
  - [ ] A proposes ChangeQuorum=2 → self-satisfies → quorum=2 effective
  - [ ] A proposes promoting D to admin → returns Pending (1 of 2)
  - [ ] B countersigns via PendingAdminProposalsPanel → D promoted
  - [ ] A tries to demote B directly → IPC returns Pending (proposal path enforced)
  - [ ] After 30d, an unfilled proposal shows Expired

## Related

- Generalizes [ZEB-254](https://linear.app/zeblith/issue/ZEB-254) (PendingJoin + JoinCountersign)
- Parent epic [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1)
- Recovery primitive [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (community forking)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture PR URL**

`gh pr create` prints the PR URL on success. Record it for the calling agent's autonomous bot-review loop.

- [ ] **Step 6: Verify PR is open + sane**

```bash
gh pr view --json url,state,baseRefName,headRefName
```

Expected: state `OPEN`, baseRefName `main`, headRefName `zeb-250-admin-quorum`.

**Task 17 ends here.** Hand control back to the calling agent for the autonomous bot-review monitoring loop (CodeRabbit, Cursor, CodeAnt, Qodo — NOT Greptile, NOT CI).
