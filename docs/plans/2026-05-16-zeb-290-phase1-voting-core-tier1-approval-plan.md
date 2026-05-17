# ZEB-290 Phase 1: voting_core + Tier 1 Approval Voting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Phase 1 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella — the shared voting infrastructure (`voting_core.rs`, `voting_log.rs`) plus the simplest tier (Tier 1 Approval voting) as a chat-native MVP.

**Architecture:** New per-community voting event log parallel to `community_channel_log` ([ZEB-248](https://linear.app/zeblith/issue/ZEB-248) pattern). Shared `voting_core` owns wire envelope, eligibility verifier, lifecycle state machine, IPC dispatcher, audit log. `voting_approval` owns Tier 1 mechanism (Approval ballot, HLC-LWW tally with optional quorum/threshold/multi-winner). Chat-native poll UI embeds as a new message-kind in any channel where the author has post permission.

**Tech Stack:** Rust (Tauri backend) + Svelte 5 (frontend) + ciborium for canonical CBOR + Ed25519 signatures + HLC for ordering + Iroh transport (Zenoh pubsub) + nextest for tests.

**Spec:** [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](../specs/2026-05-16-zeb-289-voting-polling-design.md) (commit `afee940`).

---

## File Structure

### Created in this plan

| File | Responsibility | Approx LOC |
|---|---|---|
| `src-tauri/src/community_voting_core.rs` | Shared types (`PollId`, `Tier`, `Eligibility`, `PollMeta`, `Lifecycle`), `SignedVotingEvent` envelope, `PollEventKindCode` enum, eligibility verifier, lifecycle state machine, audit-log primitives | ~700 |
| `src-tauri/src/community_voting_log.rs` | Per-community in-memory voting event log; Zenoh sync topic registration; auto-close-on-window-expiry tick; persist hooks | ~500 |
| `src-tauri/src/community_voting_approval.rs` | Tier 1 mechanism — `Tier1PollConfig`, `Tier1Ballot`, validate functions, materialize (HLC LWW + approval tally), result variants (Winners / NoQuorum / NoMajority) | ~400 |
| `src-tauri/tests/community_voting_tier1_integration.rs` | Two-engine integration test: create poll, both engines cast ballots, both converge on identical tally | ~300 |
| `src-tauri/tests/wire_format_zeb290_fixtures.rs` | Byte-pinned canonical CBOR fixtures for 6 Phase 1 event kinds (PollCreate / PollOpen / PollExtend / PollClose / BallotCast / PollResult) | ~200 |
| `src/lib/types/voting.ts` | TypeScript types matching backend wire format (snake_case → camelCase across IPC boundary) | ~100 |
| `src/lib/voting-adapter.ts` | Thin adapter for the 4 IPC commands + 3 Tauri events; type-safe wrapper around `invoke` and `listen` | ~120 |
| `src/lib/components/PollMessage.svelte` | Chat-native embedded poll card — clickable options, live tally bars, voter-selection highlighting, window countdown, auto-result reveal | ~250 |
| `src/lib/components/PollMessage.test.ts` | Vitest unit tests for PollMessage rendering, click handling, tally bar updates | ~180 |

### Modified in this plan

| File | Change |
|---|---|
| `src-tauri/src/lib.rs` | Register 4 new Tauri IPC commands (`voting_create_tier1_poll`, `voting_cast_tier1_ballot`, `voting_list_active_polls`, `voting_get_poll`); register `NodeState` field for voting log; wire archive sweep into the existing periodic tick loop |
| `src-tauri/Cargo.toml` | No new dependencies (ciborium, ed25519, serde already present) |
| `src/lib/components/MessageList.svelte` (or wherever message rendering dispatches by kind) | Add poll-message kind dispatch to render `PollMessage.svelte` for embedded polls |
| `src/lib/types/index.ts` (or central types module) | Re-export voting types |

---

## Task 0: Pre-flight verification (no commit)

**Purpose:** Confirm the working tree is clean, on the correct branch, with the umbrella spec in place, and that all five CI gates are currently green on the spec-only state. Establishes the green baseline so any regression introduced during this plan is unambiguously the implementer's fault (per `feedback_test_drift_is_our_fault`).

**Files:** none modified; verification only.

- [ ] **Step 1: Verify branch and clean working tree**

Run:
```bash
git branch --show-current
git status --short
```

Expected:
```
zeb-290-phase1-voting-core-tier1-approval
(no output from git status)
```

If branch is wrong, abort and switch via `git checkout zeb-290-phase1-voting-core-tier1-approval` (do NOT create worktree per `feedback_no_worktrees`).

- [ ] **Step 2: Verify spec is present on branch**

Run:
```bash
ls docs/specs/2026-05-16-zeb-289-voting-polling-design.md
git log --oneline -3
```

Expected: spec file exists; recent commits include `afee940 docs(zeb-289): link Phase N table to filed sub-tickets ZEB-290..296` and `cb87b8f docs(zeb-289): umbrella spec for voting/polling primitive`.

- [ ] **Step 3: Verify branch is on latest origin/main lineage**

Run:
```bash
git fetch origin
git merge-base --is-ancestor origin/main HEAD && echo "branch contains origin/main"
```

Expected: `branch contains origin/main`. If not, abort and consult on rebasing (per `feedback_pull_before_work`).

- [ ] **Step 4: Verify green baseline — Rust gates**

Run from `src-tauri/`:
```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
```

Expected: fmt produces no output; clippy emits no warnings; nextest reports `PASS` for the full suite (test count varies — current main as of 2026-05-16 is ~1772 passed).

If any gate fails on this clean state, STOP and report — it's an unrelated test drift to file as a follow-up ticket (per `feedback_unrelated_test_failures`), not something this plan should absorb.

- [ ] **Step 5: Verify green baseline — frontend gates**

Run from repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: both pass (vitest current main as of 2026-05-16 is ~1772 passed).

- [ ] **Step 6: No commit**

Task 0 is verification only. Do NOT commit anything. Proceed to Task 1.

---

## Task 1: Core scalar types and Lifecycle enum

**Purpose:** Define the foundational types that the rest of the voting module hangs off of. These are pure data with no behavior, so they're a safe first step that establishes the namespace.

**Files:**
- Create: `src-tauri/src/community_voting_core.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_voting_core;`)

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/community_voting_core.rs` with a `tests` module first:

```rust
//! ZEB-290 Phase 1: shared voting infrastructure (types + lifecycle + envelope).
//!
//! See spec `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §2 + §3.
//!
//! This module owns wire-stable types used by all voting tiers
//! (`voting_approval.rs`, future `voting_conviction.rs`, `voting_sortition.rs`).

use serde::{Deserialize, Serialize};

use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// Globally-unique identifier for a poll, derived from
/// `H(community_id || poll_create_event_hash)`.
///
/// 32 bytes (SHA-256 output). Newtype wrapper keeps type-safety —
/// callers cannot accidentally pass a raw `[u8; 32]` like a `ChannelId`
/// or `EventId` of the same length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PollId(pub [u8; 32]);

/// The three voting tiers. Wire-encoded as u8 (`tr` field of envelope).
/// See spec §1 + §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tier {
    Approval = 1,
    Conviction = 2,
    Sortition = 3,
}

/// Per-poll eligibility predicate. Spec §1 Goal 5 + §7.
///
/// `min_power`: required member power level (verified against
/// community_membership at the poll's eligibility-snapshot HLC).
///
/// `min_vouching_depth`: optional Sybil filter; voter must be vouched
/// for by at least this many other members. None = no vouching gate.
///
/// `sortition_size`: Tier 3 only; ignored for Tier 1/2. Reserved here
/// so the type is wire-stable across all tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eligibility {
    #[serde(rename = "mp")]
    pub min_power: u8,
    #[serde(rename = "mv", skip_serializing_if = "Option::is_none", default)]
    pub min_vouching_depth: Option<u8>,
    #[serde(rename = "sz", skip_serializing_if = "Option::is_none", default)]
    pub sortition_size: Option<u16>,
}

/// Poll lifecycle state. Spec §2 (poll lifecycle diagram).
///
/// Transitions: Draft → Open → Closed → Finalized → Archived.
/// (Draft is implementation-only — never on the wire; PollCreate
/// events publish directly into Open.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Draft,
    Open,
    Closed,
    Finalized,
    Archived,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_id_round_trip() {
        let pid = PollId([0x42; 32]);
        let encoded = ciborium::ser::into_vec(&pid).expect("encode");
        let decoded: PollId = ciborium::de::from_reader(&encoded[..]).expect("decode");
        assert_eq!(pid, decoded);
    }

    #[test]
    fn tier_is_u8_repr() {
        assert_eq!(Tier::Approval as u8, 1);
        assert_eq!(Tier::Conviction as u8, 2);
        assert_eq!(Tier::Sortition as u8, 3);
    }

    #[test]
    fn eligibility_minimal_omits_optional_fields() {
        let e = Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        };
        let encoded = ciborium::ser::into_vec(&e).expect("encode");
        // Should be a 1-field map: just "mp" → 0.
        let value: ciborium::Value =
            ciborium::de::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 1, "optional None fields must be skipped");
        assert!(map.iter().any(|(k, _)| k.as_text() == Some("mp")));
    }

    #[test]
    fn lifecycle_round_trip() {
        for state in &[
            Lifecycle::Draft,
            Lifecycle::Open,
            Lifecycle::Closed,
            Lifecycle::Finalized,
            Lifecycle::Archived,
        ] {
            let encoded = ciborium::ser::into_vec(state).expect("encode");
            let decoded: Lifecycle =
                ciborium::de::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*state, decoded);
        }
    }
}
```

Add `pub mod community_voting_core;` to `src-tauri/src/lib.rs` near the other `pub mod community_*` lines.

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_core)' && cd ..
```

Expected: compile error initially because `community_voting_core` module doesn't exist; after the `pub mod` line is added, the tests in the file should compile and run, all passing (this is a green-from-the-start task — types have no behavior to fail on).

- [ ] **Step 3: Verify all five CI gates remain green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

Expected: no fmt diff, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_voting_core.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add voting_core scalar types + Lifecycle enum

Foundation types for Phase 1 of the voting/polling umbrella
(ZEB-289). PollId is a 32-byte newtype derived from community + create
event hash; Tier is u8-repr for wire encoding; Eligibility carries
min_power + optional min_vouching_depth + optional sortition_size (the
last reserved for Tier 3); Lifecycle is the poll state-machine.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: SignedVotingEvent envelope + canonical CBOR encoding

**Purpose:** Define the wire envelope (per spec §3) and verify its canonical CBOR layout matches the spec. This is the load-bearing wire-format commitment — once shipped, changing the envelope keys breaks peer interop.

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_core.rs`:

```rust
/// Discriminator for the kind of voting event (cr/op/xt/cl/bl/rs for
/// Phase 1; sg/dg/ud added in Phase 2; ss/ds/dv/dc/rb/ts added in
/// Phase 4-6). Wire-encoded as a 2-char string in the envelope's `kd`
/// field.
///
/// Spec §3 event-kind catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PollEventKindCode {
    #[serde(rename = "cr")]
    PollCreate,
    #[serde(rename = "op")]
    PollOpen,
    #[serde(rename = "xt")]
    PollExtend,
    #[serde(rename = "cl")]
    PollClose,
    #[serde(rename = "bl")]
    BallotCast,
    #[serde(rename = "rs")]
    PollResult,
    // Phase 2 (Tier 2): Signal/Delegate/Undelegate.
    // Phase 4+ (Tier 3): SortitionSelection/DeliberationStatement/etc.
    // Unknown variants on the wire fail-soft at verify V4 (see voting_log).
}

/// The wire envelope for every voting event. Spec §3.
///
/// All 8 fields use 2-char keys to satisfy the same-length-keys
/// invariant. The `pd` field is opaque tier+kind-specific CBOR bytes;
/// per-tier modules decode `pd` into typed payloads.
///
/// `sg` (signature) is computed over the canonical CBOR of all fields
/// EXCEPT `sg` itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVotingEvent {
    /// Event family tag — always 'p' for poll/voting.
    #[serde(rename = "tg")]
    pub tag: char,
    /// Schema version (currently 1).
    #[serde(rename = "vr")]
    pub version: u8,
    /// Tier (1/2/3).
    #[serde(rename = "tr")]
    pub tier: Tier,
    /// Event kind discriminator.
    #[serde(rename = "kd")]
    pub kind: PollEventKindCode,
    /// Hybrid Logical Clock timestamp.
    #[serde(rename = "hc")]
    pub hlc: Hlc,
    /// Signer's owner address (derived from Ed25519 pubkey).
    #[serde(rename = "ac")]
    pub actor: OwnerAddr,
    /// Opaque tier+kind-specific CBOR-encoded payload.
    #[serde(rename = "pd", with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Ed25519 signature over canonical CBOR of all fields except `sg`.
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
}

impl SignedVotingEvent {
    /// Compute the canonical CBOR bytes that the signature covers
    /// (all fields except `sg`).
    ///
    /// Used at sign time (to compute `sg`) and at verify time (to
    /// verify `sg` against `ac`).
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        // Construct a temporary struct with the 7 fields excluding sg.
        // Same field names + serde renames → byte-stable signing.
        #[derive(Serialize)]
        struct SigInput<'a> {
            #[serde(rename = "tg")]
            tag: char,
            #[serde(rename = "vr")]
            version: u8,
            #[serde(rename = "tr")]
            tier: Tier,
            #[serde(rename = "kd")]
            kind: PollEventKindCode,
            #[serde(rename = "hc")]
            hlc: &'a Hlc,
            #[serde(rename = "ac")]
            actor: &'a OwnerAddr,
            #[serde(rename = "pd", with = "serde_bytes")]
            payload: &'a [u8],
        }
        let inp = SigInput {
            tag: self.tag,
            version: self.version,
            tier: self.tier,
            kind: self.kind,
            hlc: &self.hlc,
            actor: &self.actor,
            payload: &self.payload,
        };
        let mut out = Vec::new();
        ciborium::ser::into_writer(&inp, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;
    use crate::owner_state_types::DeviceId;

    fn make_event(kind: PollEventKindCode) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind,
            hlc: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: DeviceId("test".into()),
            },
            actor: OwnerAddr([0xaa; 16]),
            payload: vec![0xde, 0xad],
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn envelope_round_trips() {
        let ev = make_event(PollEventKindCode::PollCreate);
        let encoded = ciborium::ser::into_vec(&ev).expect("encode");
        let decoded: SignedVotingEvent =
            ciborium::de::from_reader(&encoded[..]).expect("decode");
        assert_eq!(ev, decoded);
    }

    #[test]
    fn envelope_has_eight_top_level_keys() {
        let ev = make_event(PollEventKindCode::BallotCast);
        let encoded = ciborium::ser::into_vec(&ev).expect("encode");
        let value: ciborium::Value =
            ciborium::de::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("top-level is a CBOR map");
        assert_eq!(map.len(), 8, "envelope must have exactly 8 fields");
        for expected in &["tg", "vr", "tr", "kd", "hc", "ac", "pd", "sg"] {
            assert!(
                map.iter().any(|(k, _)| k.as_text() == Some(*expected)),
                "envelope missing key {expected:?}"
            );
        }
    }

    #[test]
    fn envelope_keys_all_two_char() {
        // Same-length-keys invariant.
        let ev = make_event(PollEventKindCode::PollOpen);
        let encoded = ciborium::ser::into_vec(&ev).expect("encode");
        let value: ciborium::Value =
            ciborium::de::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        for (k, _) in map.iter() {
            let s = k.as_text().expect("key is text");
            assert_eq!(s.len(), 2, "envelope key {s:?} violates 2-char invariant");
        }
    }

    #[test]
    fn signing_bytes_exclude_sig() {
        let mut ev = make_event(PollEventKindCode::PollResult);
        let sb1 = ev.signing_bytes().expect("signing bytes");

        // Mutating sig should not change signing_bytes.
        ev.sig = vec![0xff; 64];
        let sb2 = ev.signing_bytes().expect("signing bytes");

        assert_eq!(sb1, sb2, "signing_bytes must be independent of sig field");
    }

    #[test]
    fn signing_bytes_have_seven_top_level_keys() {
        let ev = make_event(PollEventKindCode::PollClose);
        let sb = ev.signing_bytes().expect("signing bytes");
        let value: ciborium::Value = ciborium::de::from_reader(&sb[..]).expect("decode");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 7, "signing bytes must exclude sg field");
        assert!(!map.iter().any(|(k, _)| k.as_text() == Some("sg")));
    }

    #[test]
    fn kind_code_round_trip() {
        for kind in &[
            PollEventKindCode::PollCreate,
            PollEventKindCode::PollOpen,
            PollEventKindCode::PollExtend,
            PollEventKindCode::PollClose,
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollResult,
        ] {
            let encoded = ciborium::ser::into_vec(kind).expect("encode");
            let decoded: PollEventKindCode =
                ciborium::de::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*kind, decoded);
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails initially**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(envelope_tests)' && cd ..
```

Expected: tests compile and run successfully (this is a pure types task with no behavior to fail on). If they fail, the most likely cause is missing `serde_bytes` dep — verify in `Cargo.toml` (should already be present from other modules).

- [ ] **Step 3: Verify all five CI gates remain green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_voting_core.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add SignedVotingEvent envelope + PollEventKindCode

Wire envelope per spec §3: 8 fields all 2-char keys (tg/vr/tr/kd/hc/ac/
pd/sg), same-length-keys invariant locked in by test. signing_bytes()
returns canonical CBOR of the 7 fields the signature covers (excludes
sg itself). PollEventKindCode covers all 6 Phase 1 event kinds; Phase
2/3 kinds will extend the enum and fail-soft on the wire for nodes
that don't yet support them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: PollMeta + helpers for poll-id derivation

**Purpose:** Define `PollMeta` — the materialized state attached to each `PollId` — plus the deterministic `derive_poll_id()` helper that produces a `PollId` from a community-id + create-event-hash. PollMeta is what `voting_list_active_polls` / `voting_get_poll` IPCs return.

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_core.rs`:

```rust
use sha2::{Digest, Sha256};

/// Materialized metadata for a single poll. Returned by
/// `voting_get_poll` / `voting_list_active_polls` IPCs.
///
/// `created_at`, `opens_at`, `closes_at` are HLC timestamps;
/// `extends_at` is the most recent PollExtend event's HLC (or None
/// if no extend has occurred).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollMeta {
    pub poll_id: PollId,
    pub community_id: SpaceId,
    pub creator: OwnerAddr,
    pub tier: Tier,
    pub eligibility: Eligibility,
    pub lifecycle: Lifecycle,
    pub created_at: Hlc,
    pub opens_at: Hlc,
    pub closes_at: Hlc,
    pub extends_at: Option<Hlc>,
    /// Channel where the poll was created (Tier 1 only; Tier 2/3 may
    /// not be channel-scoped). For Tier 1 chat-native polls this is
    /// the channel where the poll-message card appears.
    pub channel_id: Option<crate::owner_state_types::ChannelId>,
}

/// Deterministically derive a PollId from the community + the
/// PollCreate event's signing-bytes hash.
///
/// `PollId = SHA-256(community_id_bytes || create_event_signing_bytes)`.
///
/// Two nodes that independently observe the same PollCreate event
/// derive the same PollId. Re-derivable at any time; never stored
/// inside the event itself (would be circular).
pub fn derive_poll_id(community_id: &SpaceId, create_signing_bytes: &[u8]) -> PollId {
    let mut hasher = Sha256::new();
    hasher.update(community_id.0);
    hasher.update(create_signing_bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    PollId(out)
}

#[cfg(test)]
mod poll_meta_tests {
    use super::*;
    use crate::owner_state_types::{ChannelId, DeviceId};

    #[test]
    fn derive_poll_id_is_deterministic() {
        let cid = SpaceId([0x11; 16]);
        let sb = vec![1, 2, 3, 4, 5];
        let pid1 = derive_poll_id(&cid, &sb);
        let pid2 = derive_poll_id(&cid, &sb);
        assert_eq!(pid1, pid2);
    }

    #[test]
    fn derive_poll_id_differs_by_community() {
        let sb = vec![1, 2, 3];
        let pid_a = derive_poll_id(&SpaceId([0x11; 16]), &sb);
        let pid_b = derive_poll_id(&SpaceId([0x22; 16]), &sb);
        assert_ne!(pid_a, pid_b);
    }

    #[test]
    fn derive_poll_id_differs_by_event_bytes() {
        let cid = SpaceId([0x33; 16]);
        let pid_a = derive_poll_id(&cid, &[1, 2, 3]);
        let pid_b = derive_poll_id(&cid, &[1, 2, 4]);
        assert_ne!(pid_a, pid_b);
    }

    #[test]
    fn poll_meta_round_trip() {
        let meta = PollMeta {
            poll_id: PollId([0xab; 32]),
            community_id: SpaceId([0x11; 16]),
            creator: OwnerAddr([0xcc; 16]),
            tier: Tier::Approval,
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            lifecycle: Lifecycle::Open,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: DeviceId("a".into()),
            },
            opens_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: DeviceId("a".into()),
            },
            closes_at: Hlc {
                wall_ms: 3700,
                logical: 0,
                device_id: DeviceId("a".into()),
            },
            extends_at: None,
            channel_id: Some(ChannelId([0xdd; 16])),
        };
        let encoded = ciborium::ser::into_vec(&meta).expect("encode");
        let decoded: PollMeta =
            ciborium::de::from_reader(&encoded[..]).expect("decode");
        assert_eq!(meta, decoded);
    }
}
```

- [ ] **Step 2: Verify `sha2` is in Cargo.toml**

```bash
grep -E "^sha2|^sha-2" src-tauri/Cargo.toml || echo "missing"
```

Expected: a line like `sha2 = "0.10"`. If missing (unlikely given existing crypto in the codebase), add it.

- [ ] **Step 3: Run test to verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(poll_meta_tests)' && cd ..
```

Expected: all 4 tests pass.

- [ ] **Step 4: Verify all five CI gates remain green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_voting_core.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add PollMeta + derive_poll_id helper

PollMeta is the materialized poll-state struct returned by IPC
queries (voting_get_poll, voting_list_active_polls). derive_poll_id
is the canonical PollId-from-community+create-bytes derivation:
SHA-256(community_id || signing_bytes). Both two nodes independently
observing the same PollCreate derive the same PollId — required for
deterministic dispatch in voting_log.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Eligibility verifier

**Purpose:** Single shared implementation of "is this signer allowed to cast this ballot?" that all tier modules call. Takes a poll's `Eligibility` predicate + a community-membership snapshot at the poll's eligibility-snapshot HLC + the candidate signer, returns Ok iff signer meets the predicate.

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_core.rs`:

```rust
use std::collections::HashMap;

/// Snapshot of community membership at a specific HLC, used by the
/// eligibility verifier. Built by querying `community_membership`
/// materialized state at the desired HLC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipSnapshot {
    pub members: HashMap<OwnerAddr, MemberAttrs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberAttrs {
    pub power: u8,
    pub vouching_depth: u8,
}

/// Why an eligibility check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EligibilityFailure {
    NotMember,
    InsufficientPower { required: u8, actual: u8 },
    InsufficientVouchingDepth { required: u8, actual: u8 },
}

/// Verify that `signer` meets `eligibility` against `snapshot`.
/// Returns Ok(()) if eligible; Err(reason) otherwise.
pub fn check_eligibility(
    snapshot: &MembershipSnapshot,
    signer: &OwnerAddr,
    eligibility: &Eligibility,
) -> Result<(), EligibilityFailure> {
    let attrs = snapshot
        .members
        .get(signer)
        .ok_or(EligibilityFailure::NotMember)?;
    if attrs.power < eligibility.min_power {
        return Err(EligibilityFailure::InsufficientPower {
            required: eligibility.min_power,
            actual: attrs.power,
        });
    }
    if let Some(req_depth) = eligibility.min_vouching_depth {
        if attrs.vouching_depth < req_depth {
            return Err(EligibilityFailure::InsufficientVouchingDepth {
                required: req_depth,
                actual: attrs.vouching_depth,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod eligibility_tests {
    use super::*;

    fn snapshot_with(addr: OwnerAddr, power: u8, vouching_depth: u8) -> MembershipSnapshot {
        let mut members = HashMap::new();
        members.insert(addr, MemberAttrs { power, vouching_depth });
        MembershipSnapshot { members }
    }

    #[test]
    fn non_member_rejected() {
        let snap = snapshot_with(OwnerAddr([0x11; 16]), 100, 5);
        let elig = Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None };
        assert_eq!(
            check_eligibility(&snap, &OwnerAddr([0x22; 16]), &elig),
            Err(EligibilityFailure::NotMember)
        );
    }

    #[test]
    fn member_with_sufficient_power_accepted() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 50, 0);
        let elig = Eligibility { min_power: 50, min_vouching_depth: None, sortition_size: None };
        assert_eq!(check_eligibility(&snap, &addr, &elig), Ok(()));
    }

    #[test]
    fn member_with_insufficient_power_rejected() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 10, 0);
        let elig = Eligibility { min_power: 50, min_vouching_depth: None, sortition_size: None };
        assert_eq!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientPower { required: 50, actual: 10 })
        );
    }

    #[test]
    fn vouching_depth_gate_enforced() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 1);
        let elig = Eligibility { min_power: 1, min_vouching_depth: Some(3), sortition_size: None };
        assert_eq!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientVouchingDepth { required: 3, actual: 1 })
        );
    }

    #[test]
    fn vouching_depth_gate_satisfied() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 5);
        let elig = Eligibility { min_power: 1, min_vouching_depth: Some(3), sortition_size: None };
        assert_eq!(check_eligibility(&snap, &addr, &elig), Ok(()));
    }

    #[test]
    fn power_checked_before_vouching_depth() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 1);
        let elig = Eligibility { min_power: 50, min_vouching_depth: Some(10), sortition_size: None };
        assert!(matches!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientPower { .. })
        ));
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(eligibility_tests)' && cd ..
```

Expected: 6 tests pass.

- [ ] **Step 3: Verify all five CI gates remain green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_voting_core.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add eligibility verifier (check_eligibility)

Single shared eligibility predicate (spec §1 Goal 5 + §8 V6/B3).
Takes a MembershipSnapshot + signer + Eligibility; returns Ok iff
signer is a member, meets min_power, and meets min_vouching_depth
(if set). Deterministic failure ordering: NotMember > Power > Vouching.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Lifecycle state-machine transitions

**Purpose:** Encode legal `Draft → Open → Closed → Finalized → Archived` transitions so verify-on-receive can reject illegal events (e.g., BallotCast against Closed, second PollResult after Finalized).

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_core.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    IllegalTransition { from: Lifecycle, attempted: PollEventKindCode },
}

/// Given current lifecycle + incoming event kind, return new lifecycle
/// or an error. Per spec §2 + verify rules L1, B2, R1.
pub fn next_lifecycle(
    current: Lifecycle,
    kind: PollEventKindCode,
) -> Result<Lifecycle, LifecycleError> {
    use Lifecycle::*;
    use PollEventKindCode::*;
    match (current, kind) {
        (Draft, PollCreate) => Ok(Open),
        (Open, BallotCast) | (Open, PollExtend) | (Open, PollOpen) => Ok(Open),
        (Open, PollClose) => Ok(Closed),
        (Closed, PollResult) => Ok(Finalized),
        _ => Err(LifecycleError::IllegalTransition { from: current, attempted: kind }),
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn draft_to_open_via_create() {
        assert_eq!(
            next_lifecycle(Lifecycle::Draft, PollEventKindCode::PollCreate),
            Ok(Lifecycle::Open)
        );
    }

    #[test]
    fn open_accepts_ballot_cast() {
        assert_eq!(
            next_lifecycle(Lifecycle::Open, PollEventKindCode::BallotCast),
            Ok(Lifecycle::Open)
        );
    }

    #[test]
    fn open_to_closed_via_close() {
        assert_eq!(
            next_lifecycle(Lifecycle::Open, PollEventKindCode::PollClose),
            Ok(Lifecycle::Closed)
        );
    }

    #[test]
    fn closed_to_finalized_via_result() {
        assert_eq!(
            next_lifecycle(Lifecycle::Closed, PollEventKindCode::PollResult),
            Ok(Lifecycle::Finalized)
        );
    }

    #[test]
    fn closed_rejects_ballot_cast() {
        assert!(matches!(
            next_lifecycle(Lifecycle::Closed, PollEventKindCode::BallotCast),
            Err(LifecycleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn finalized_rejects_everything() {
        for kind in &[PollEventKindCode::BallotCast, PollEventKindCode::PollClose, PollEventKindCode::PollResult] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Finalized, *kind),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    #[test]
    fn archived_rejects_everything() {
        for kind in &[PollEventKindCode::BallotCast, PollEventKindCode::PollClose, PollEventKindCode::PollResult] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Archived, *kind),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }
}
```

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(lifecycle_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_core.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add lifecycle state-machine transitions

next_lifecycle() encodes legal Draft → Open → Closed → Finalized →
Archived transitions per spec §2. Open accepts BallotCast/PollExtend/
PollClose; Closed accepts only PollResult; Finalized and Archived
reject everything. Archive transition is time-based (daily sweep,
Task 14) and not represented here. Idempotent PollOpen → Open is
allowed (network re-delivery absorption).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: voting_log scaffolding (per-community in-memory store)

**Purpose:** Create `community_voting_log.rs` with the per-community `VotingLog` struct that holds all `SignedVotingEvent`s + materialized `PollState` per `PollId`. This is the data structure; Zenoh sync wiring lands in Task 12.

**Files:**
- Create: `src-tauri/src/community_voting_log.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_voting_log;`)

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/community_voting_log.rs`:

```rust
//! ZEB-290 Phase 1: per-community voting event log.
//!
//! Parallels `community_channel_log.rs` (ZEB-248 pattern). Holds all
//! `SignedVotingEvent`s for a community plus the materialized per-poll
//! state map. Zenoh sync wiring lives in Task 12; this file is the
//! pure data structure + apply/materialize logic.

use std::collections::HashMap;

use crate::community_voting_core::{
    derive_poll_id, next_lifecycle, Lifecycle, PollEventKindCode, PollId, PollMeta,
    SignedVotingEvent,
};

/// All voting events for a single community, plus the materialized
/// per-poll state derived from them.
///
/// Stored in `NodeState` keyed by community SpaceId. Synced via Zenoh
/// topic `harmony/community/{id}/voting` (Task 12).
#[derive(Debug, Default, Clone)]
pub struct VotingLog {
    /// All accepted events, ordered by (hlc, event_hash) at insert time.
    pub events: Vec<SignedVotingEvent>,
    /// Materialized per-poll state, keyed by PollId.
    pub polls: HashMap<PollId, PollState>,
}

/// Materialized state for a single poll.
#[derive(Debug, Clone)]
pub struct PollState {
    pub meta: PollMeta,
    /// All events belonging to this poll, ordered by HLC.
    pub events: Vec<SignedVotingEvent>,
    /// Tier-specific tally state, opaque to voting_core.
    /// (Implementer: use `Box<dyn Any>` for now; v1 ships only Tier 1
    /// so a concrete `Tier1TallyState` is also acceptable. The trait-
    /// dispatched form is preferred for Phase 2+ readiness.)
    pub tier_state: TierState,
}

/// Tier-specific tally state. Phase 1 ships only `Tier1`; Phase 2/4+
/// add variants. Using an enum here (rather than Box<dyn Any>) keeps
/// the code monomorphic and trivially Clone'able for fork/persist.
#[derive(Debug, Clone)]
pub enum TierState {
    /// Placeholder — replaced by `Tier1TallyState` from voting_approval.rs in Task 8.
    Empty,
}

impl VotingLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a new event to the log. Caller has already done verify
    /// (V1-V6, kind-specific) — this function only handles materialize
    /// (lifecycle transition + tier-specific apply).
    ///
    /// Returns Ok(poll_id) if applied; Err if lifecycle transition
    /// is illegal (which indicates a verify-rule violation by the caller).
    pub fn apply(
        &mut self,
        event: SignedVotingEvent,
        community_id: &crate::owner_state_types::SpaceId,
    ) -> Result<PollId, ApplyError> {
        // Derive PollId for routing. For PollCreate, it's hash of the
        // event's signing bytes; for all other kinds, the payload contains
        // a poll_id reference (encoded by tier modules — Task 7 covers
        // Tier 1 specifically; this scaffolding accepts any PollId-prefixed
        // pd that decodes as `PollIdRef { pi: PollId }`).
        let poll_id = match event.kind {
            PollEventKindCode::PollCreate => {
                let sb = event
                    .signing_bytes()
                    .map_err(|_| ApplyError::SigningBytesError)?;
                derive_poll_id(community_id, &sb)
            }
            _ => decode_poll_id_ref(&event.payload)
                .ok_or(ApplyError::MissingPollIdRef)?,
        };

        // Lifecycle transition.
        let current = self
            .polls
            .get(&poll_id)
            .map(|p| p.meta.lifecycle)
            .unwrap_or(Lifecycle::Draft);
        let next = next_lifecycle(current, event.kind)
            .map_err(|_| ApplyError::IllegalTransition)?;

        // Insert / update PollState.
        // (Full PollMeta construction happens in Task 7 when Tier 1
        // PollConfig deserialization lands. For now we just track lifecycle.)
        if let Some(state) = self.polls.get_mut(&poll_id) {
            state.meta.lifecycle = next;
            state.events.push(event.clone());
        } else if event.kind == PollEventKindCode::PollCreate {
            // Stub PollMeta — populated fully in Task 7.
            let stub = PollMeta {
                poll_id,
                community_id: *community_id,
                creator: event.actor,
                tier: event.tier,
                eligibility: crate::community_voting_core::Eligibility {
                    min_power: 0,
                    min_vouching_depth: None,
                    sortition_size: None,
                },
                lifecycle: next,
                created_at: event.hlc.clone(),
                opens_at: event.hlc.clone(),
                closes_at: event.hlc.clone(), // tier deserialize overrides in Task 7
                extends_at: None,
                channel_id: None,
            };
            self.polls.insert(
                poll_id,
                PollState {
                    meta: stub,
                    events: vec![event.clone()],
                    tier_state: TierState::Empty,
                },
            );
        } else {
            return Err(ApplyError::EventBeforePollCreate);
        }

        self.events.push(event);
        Ok(poll_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    SigningBytesError,
    MissingPollIdRef,
    IllegalTransition,
    EventBeforePollCreate,
}

/// Decode a `{ "pi": <PollId> }` map from `pd` bytes. Used by all
/// non-PollCreate events to identify which poll they belong to.
fn decode_poll_id_ref(pd: &[u8]) -> Option<PollId> {
    #[derive(serde::Deserialize)]
    struct Ref {
        #[serde(rename = "pi")]
        pi: PollId,
    }
    ciborium::de::from_reader::<Ref, _>(pd).ok().map(|r| r.pi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_voting_core::Tier;
    use crate::owner_state_types::{DeviceId, Hlc, OwnerAddr, SpaceId};

    fn signing_bytes_of(ev: &SignedVotingEvent) -> Vec<u8> {
        ev.signing_bytes().expect("signing bytes")
    }

    fn poll_create_event(community_id: &SpaceId, creator: OwnerAddr) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: DeviceId("a".into()),
            },
            actor: creator,
            payload: vec![],
            sig: vec![0u8; 64],
        }
    }

    fn ballot_event(poll_id: PollId, hlc_ms: u64, voter: OwnerAddr) -> SignedVotingEvent {
        // Encode the poll-id-ref payload.
        let mut pd = Vec::new();
        let r = serde_json::json!({}); // placeholder — real encoder uses ciborium with "pi"
        let _ = r;
        // We hand-construct the CBOR map: { "pi": <bstr-32> }.
        let mut payload = Vec::new();
        ciborium::ser::into_writer(
            &PollIdRefHelper { pi: poll_id },
            &mut payload,
        )
        .unwrap();
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::BallotCast,
            hlc: Hlc {
                wall_ms: hlc_ms,
                logical: 0,
                device_id: DeviceId("a".into()),
            },
            actor: voter,
            payload,
            sig: vec![0u8; 64],
        }
    }

    #[derive(serde::Serialize)]
    struct PollIdRefHelper {
        #[serde(rename = "pi")]
        pi: PollId,
    }

    #[test]
    fn apply_poll_create_inserts_state() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x11; 16]);
        let ev = poll_create_event(&cid, OwnerAddr([0xaa; 16]));
        let pid = log.apply(ev.clone(), &cid).expect("apply");

        let expected_pid = derive_poll_id(&cid, &signing_bytes_of(&ev));
        assert_eq!(pid, expected_pid);
        assert_eq!(log.polls.len(), 1);
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Open);
    }

    #[test]
    fn apply_ballot_before_create_rejected() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x22; 16]);
        let phantom_pid = PollId([0x99; 32]);
        let ev = ballot_event(phantom_pid, 2000, OwnerAddr([0xbb; 16]));
        assert_eq!(log.apply(ev, &cid), Err(ApplyError::EventBeforePollCreate));
    }

    #[test]
    fn apply_ballot_against_existing_poll_appended() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x33; 16]);
        let create_ev = poll_create_event(&cid, OwnerAddr([0xaa; 16]));
        let pid = log.apply(create_ev, &cid).expect("apply create");

        let ballot = ballot_event(pid, 2000, OwnerAddr([0xbb; 16]));
        log.apply(ballot, &cid).expect("apply ballot");

        assert_eq!(log.polls[&pid].events.len(), 2);
    }
}
```

Add `pub mod community_voting_log;` to `src-tauri/src/lib.rs`.

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_log)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add voting_log scaffolding (per-community store)

New community_voting_log.rs holds VotingLog (per-community signed
events + materialized PollState per PollId). apply() handles
lifecycle transitions; PollCreate inserts new PollState with stub
PollMeta (full deserialization lands in Task 7 with Tier 1
PollConfig). decode_poll_id_ref decodes { "pi": PollId } from
payload bytes for non-create events.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Tier 1 PollConfig + Ballot CBOR types + validators

**Purpose:** Create `community_voting_approval.rs` with Tier 1-specific payload types (`Tier1PollConfig`, `Tier1Ballot`), CBOR encoding/decoding via the envelope's `pd` field, and `validate_poll_config` / `validate_ballot` functions. Also extend `voting_log.apply` to fully deserialize Tier 1 PollMeta on PollCreate.

**Files:**
- Create: `src-tauri/src/community_voting_approval.rs`
- Modify: `src-tauri/src/community_voting_log.rs` (call Tier 1 deserializer on PollCreate)
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_voting_approval;`)

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/community_voting_approval.rs`:

```rust
//! ZEB-290 Phase 1: Tier 1 Approval voting mechanism.
//!
//! Implements the Approval ballot (voter approves a subset of options),
//! validation rules per spec §4, and the deterministic tally per
//! spec §4 tally algorithm. Materialize and result variants land in
//! subsequent tasks.

use serde::{Deserialize, Serialize};

use crate::community_voting_core::{Eligibility, PollId};

/// Maximum number of options per Tier 1 poll. Spec §4.
pub const MAX_OPTIONS: usize = 20;
/// Maximum option label length in chars. Spec §4.
pub const MAX_OPTION_LABEL_LEN: usize = 80;
/// Minimum window in seconds. Spec §4.
pub const MIN_WINDOW_SECS: u32 = 60;
/// Maximum window in seconds (30 days). Spec §4.
pub const MAX_WINDOW_SECS: u32 = 2_592_000;

/// Tier 1 PollCreate payload, encoded as the envelope's `pd` field.
/// Spec §4 PollConfig payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1PollConfig {
    /// Option labels (2-20, each ≤ 80 chars).
    #[serde(rename = "o")]
    pub options: Vec<String>,
    /// Window in seconds (60-2_592_000).
    #[serde(rename = "w")]
    pub window_seconds: u32,
    /// Optional minimum quorum (number of ballots required for valid result).
    #[serde(rename = "q", skip_serializing_if = "Option::is_none", default)]
    pub quorum: Option<u32>,
    /// Optional supermajority threshold percent (0-100).
    #[serde(rename = "th", skip_serializing_if = "Option::is_none", default)]
    pub threshold_percent: Option<u8>,
    /// Optional multi-winner top-N (default 1).
    #[serde(rename = "mw", skip_serializing_if = "Option::is_none", default)]
    pub multi_winner: Option<u8>,
    /// Eligibility predicate. Embedded so verify-on-receive doesn't
    /// need a separate event type.
    #[serde(rename = "el")]
    pub eligibility: Eligibility,
    /// Channel where the poll-message card appears. Tier 1 specific.
    #[serde(rename = "ci", with = "serde_bytes")]
    pub channel_id_bytes: [u8; 16],
}

/// Tier 1 BallotCast payload. Spec §4 Ballot payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1Ballot {
    /// PollId reference (envelope `pd` carries this even on ballots
    /// to identify which poll the ballot is for).
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// Approved option indices, deduped and sorted ascending.
    #[serde(rename = "ap")]
    pub approved_indices: Vec<u8>,
}

/// Validate a PollConfig at create-time (before signing) or at receive
/// time (after deserialize). Spec §4 PollConfig constraints.
pub fn validate_poll_config(cfg: &Tier1PollConfig) -> Result<(), ValidationError> {
    if cfg.options.len() < 2 {
        return Err(ValidationError::TooFewOptions);
    }
    if cfg.options.len() > MAX_OPTIONS {
        return Err(ValidationError::TooManyOptions);
    }
    for (i, label) in cfg.options.iter().enumerate() {
        if label.is_empty() {
            return Err(ValidationError::EmptyOptionLabel(i));
        }
        if label.chars().count() > MAX_OPTION_LABEL_LEN {
            return Err(ValidationError::OptionLabelTooLong(i));
        }
    }
    if cfg.window_seconds < MIN_WINDOW_SECS {
        return Err(ValidationError::WindowTooShort);
    }
    if cfg.window_seconds > MAX_WINDOW_SECS {
        return Err(ValidationError::WindowTooLong);
    }
    if let Some(th) = cfg.threshold_percent {
        if th > 100 {
            return Err(ValidationError::ThresholdOutOfRange);
        }
    }
    if let Some(mw) = cfg.multi_winner {
        if mw == 0 {
            return Err(ValidationError::MultiWinnerZero);
        }
        if mw as usize > cfg.options.len() {
            return Err(ValidationError::MultiWinnerExceedsOptions);
        }
    }
    Ok(())
}

/// Validate a Ballot against its poll's config. Spec §4 ballot constraints.
pub fn validate_ballot(
    ballot: &Tier1Ballot,
    cfg: &Tier1PollConfig,
) -> Result<(), ValidationError> {
    if ballot.approved_indices.is_empty() {
        return Err(ValidationError::EmptyBallot);
    }
    if ballot.approved_indices.len() == cfg.options.len() {
        return Err(ValidationError::AbstentionBallot);
    }
    // Indices in range.
    for &i in &ballot.approved_indices {
        if (i as usize) >= cfg.options.len() {
            return Err(ValidationError::IndexOutOfRange);
        }
    }
    // Deduped + sorted ascending.
    for w in ballot.approved_indices.windows(2) {
        if w[0] >= w[1] {
            return Err(ValidationError::IndicesNotSortedDeduped);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    TooFewOptions,
    TooManyOptions,
    EmptyOptionLabel(usize),
    OptionLabelTooLong(usize),
    WindowTooShort,
    WindowTooLong,
    ThresholdOutOfRange,
    MultiWinnerZero,
    MultiWinnerExceedsOptions,
    EmptyBallot,
    AbstentionBallot,
    IndexOutOfRange,
    IndicesNotSortedDeduped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_config() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["Pizza".into(), "Burgers".into(), "Sushi".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id_bytes: [0x11; 16],
        }
    }

    #[test]
    fn good_config_validates() {
        assert_eq!(validate_poll_config(&good_config()), Ok(()));
    }

    #[test]
    fn too_few_options_rejected() {
        let mut c = good_config();
        c.options = vec!["only one".into()];
        assert_eq!(validate_poll_config(&c), Err(ValidationError::TooFewOptions));
    }

    #[test]
    fn too_many_options_rejected() {
        let mut c = good_config();
        c.options = (0..21).map(|i| format!("opt{i}")).collect();
        assert_eq!(validate_poll_config(&c), Err(ValidationError::TooManyOptions));
    }

    #[test]
    fn label_too_long_rejected() {
        let mut c = good_config();
        c.options[1] = "x".repeat(81);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::OptionLabelTooLong(1))
        );
    }

    #[test]
    fn empty_label_rejected() {
        let mut c = good_config();
        c.options[0] = "".into();
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::EmptyOptionLabel(0))
        );
    }

    #[test]
    fn window_too_short_rejected() {
        let mut c = good_config();
        c.window_seconds = 30;
        assert_eq!(validate_poll_config(&c), Err(ValidationError::WindowTooShort));
    }

    #[test]
    fn window_too_long_rejected() {
        let mut c = good_config();
        c.window_seconds = 2_592_001;
        assert_eq!(validate_poll_config(&c), Err(ValidationError::WindowTooLong));
    }

    #[test]
    fn threshold_over_100_rejected() {
        let mut c = good_config();
        c.threshold_percent = Some(101);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::ThresholdOutOfRange)
        );
    }

    #[test]
    fn multi_winner_zero_rejected() {
        let mut c = good_config();
        c.multi_winner = Some(0);
        assert_eq!(validate_poll_config(&c), Err(ValidationError::MultiWinnerZero));
    }

    #[test]
    fn multi_winner_exceeds_options_rejected() {
        let mut c = good_config();
        c.multi_winner = Some(5); // only 3 options
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::MultiWinnerExceedsOptions)
        );
    }

    #[test]
    fn good_ballot_validates() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 2],
        };
        assert_eq!(validate_ballot(&b, &cfg), Ok(()));
    }

    #[test]
    fn empty_ballot_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![],
        };
        assert_eq!(validate_ballot(&b, &cfg), Err(ValidationError::EmptyBallot));
    }

    #[test]
    fn approve_all_rejected_as_abstention() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 1, 2],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::AbstentionBallot)
        );
    }

    #[test]
    fn out_of_range_index_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 5],
        };
        assert_eq!(validate_ballot(&b, &cfg), Err(ValidationError::IndexOutOfRange));
    }

    #[test]
    fn unsorted_indices_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![2, 0],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::IndicesNotSortedDeduped)
        );
    }

    #[test]
    fn duplicate_indices_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 0, 2],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::IndicesNotSortedDeduped)
        );
    }

    #[test]
    fn config_round_trips_via_cbor() {
        let cfg = good_config();
        let encoded = ciborium::ser::into_vec(&cfg).expect("encode");
        let decoded: Tier1PollConfig =
            ciborium::de::from_reader(&encoded[..]).expect("decode");
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn ballot_round_trips_via_cbor() {
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![1, 3, 7],
        };
        let encoded = ciborium::ser::into_vec(&b).expect("encode");
        let decoded: Tier1Ballot =
            ciborium::de::from_reader(&encoded[..]).expect("decode");
        assert_eq!(b, decoded);
    }
}
```

Then update `src-tauri/src/community_voting_log.rs` `apply` function to call `Tier1PollConfig` deserialization on PollCreate for Tier 1 events, populating `PollMeta` fully (instead of the stub from Task 6). Add:

```rust
// In community_voting_log.rs, inside apply() for the PollCreate branch
// when event.tier == Tier::Approval:
use crate::community_voting_approval::Tier1PollConfig;
if event.tier == crate::community_voting_core::Tier::Approval
    && event.kind == PollEventKindCode::PollCreate
{
    let cfg: Tier1PollConfig = ciborium::de::from_reader(&event.payload[..])
        .map_err(|_| ApplyError::PayloadDecode)?;
    crate::community_voting_approval::validate_poll_config(&cfg)
        .map_err(|_| ApplyError::PayloadValidate)?;
    // Populate PollMeta with real config-derived fields:
    //   eligibility = cfg.eligibility
    //   closes_at = event.hlc + cfg.window_seconds (use Hlc helper to add seconds)
    //   channel_id = Some(ChannelId(cfg.channel_id_bytes))
    // Replace the stub PollMeta with this populated version.
}
```

Add `PayloadDecode` and `PayloadValidate` variants to `ApplyError`.

Add `pub mod community_voting_approval;` to `src-tauri/src/lib.rs`.

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_approval)' && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_log)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_approval.rs src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add Tier1PollConfig + Tier1Ballot types + validators

Tier 1-specific payload types encoded as the envelope's pd field per
spec §4. validate_poll_config enforces options (2-20, labels ≤ 80
chars), window (60-2_592_000s), threshold percent (0-100), multi-
winner (1..=options.len()). validate_ballot enforces non-empty,
non-approve-all (= abstention), in-range indices, sorted+deduped.

voting_log.apply now fully deserializes Tier 1 PollConfig on
PollCreate, populating PollMeta.eligibility / closes_at / channel_id
from the config. Tier 2/3 deserialization lands in their respective
phases.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Tier 1 materialize — HLC LWW + approval tally

**Purpose:** Implement the deterministic tally per spec §4. Walk BallotCast events in HLC order; keep only the latest ballot per voter (LWW); reject ballots whose voter fails eligibility at PollCreate.hlc snapshot; sum approvals per option. Stop at the result-variant step (Task 9 adds quorum/threshold/multi-winner finishing).

**Files:**
- Modify: `src-tauri/src/community_voting_approval.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_approval.rs`:

```rust
use std::collections::HashMap;

use crate::community_voting_core::{
    check_eligibility, MembershipSnapshot, SignedVotingEvent,
};
use crate::owner_state_types::{Hlc, OwnerAddr};

/// Tally state for a Tier 1 poll. Per-option approval counts plus
/// the LWW-resolved latest ballot per voter (kept for audit + IPC
/// "your current ballot" lookup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1TallyState {
    /// Per-option approval count.
    pub counts: Vec<u32>,
    /// Number of distinct voters whose latest ballot was eligible
    /// and counted.
    pub ballot_count: u32,
    /// Latest ballot per voter (for "your current vote" UI + audit).
    pub latest_ballots: HashMap<OwnerAddr, Tier1Ballot>,
}

impl Tier1TallyState {
    pub fn empty(options_len: usize) -> Self {
        Self {
            counts: vec![0; options_len],
            ballot_count: 0,
            latest_ballots: HashMap::new(),
        }
    }
}

/// Deterministically tally all BallotCast events for a Tier 1 poll.
/// Spec §4 tally algorithm steps 1-4 (steps 5-9 = result variants in Task 9).
///
/// Inputs:
///   - `cfg`: poll's deserialized Tier1PollConfig
///   - `events`: all signed events for this poll, in HLC order
///   - `snapshot`: community-membership snapshot at PollCreate.hlc
///
/// Returns: per-voter latest ballot + per-option counts + ballot count.
pub fn tally_tier1(
    cfg: &Tier1PollConfig,
    events: &[SignedVotingEvent],
    snapshot: &MembershipSnapshot,
) -> Tier1TallyState {
    use crate::community_voting_core::PollEventKindCode;

    // HLC LWW: walk in HLC order, keep latest ballot per voter.
    let mut latest_ballots: HashMap<OwnerAddr, Tier1Ballot> = HashMap::new();
    let mut latest_hlc: HashMap<OwnerAddr, Hlc> = HashMap::new();
    for ev in events {
        if ev.kind != PollEventKindCode::BallotCast {
            continue;
        }
        let ballot: Tier1Ballot = match ciborium::de::from_reader(&ev.payload[..]) {
            Ok(b) => b,
            Err(_) => continue, // malformed ballots dropped (should have been rejected at verify, but defensive)
        };
        if validate_ballot(&ballot, cfg).is_err() {
            continue;
        }
        if check_eligibility(snapshot, &ev.actor, &cfg.eligibility).is_err() {
            continue;
        }
        // LWW by HLC: replace if this event's HLC > current latest.
        let should_replace = latest_hlc
            .get(&ev.actor)
            .map(|prev| ev.hlc.compare(prev).is_gt())
            .unwrap_or(true);
        if should_replace {
            latest_ballots.insert(ev.actor, ballot);
            latest_hlc.insert(ev.actor, ev.hlc.clone());
        }
    }

    // Sum approvals across latest ballots only.
    let mut counts = vec![0u32; cfg.options.len()];
    for ballot in latest_ballots.values() {
        for &i in &ballot.approved_indices {
            counts[i as usize] += 1;
        }
    }
    let ballot_count = latest_ballots.len() as u32;

    Tier1TallyState {
        counts,
        ballot_count,
        latest_ballots,
    }
}

#[cfg(test)]
mod tally_tests {
    use super::*;
    use crate::community_voting_core::{
        check_eligibility, MemberAttrs, MembershipSnapshot, PollEventKindCode, Tier,
    };
    use crate::owner_state_types::{DeviceId, Hlc, OwnerAddr};

    fn cfg_three_opts() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["A".into(), "B".into(), "C".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id_bytes: [0; 16],
        }
    }

    fn ballot_ev(voter: OwnerAddr, hlc_ms: u64, ap: Vec<u8>) -> SignedVotingEvent {
        let payload_obj = Tier1Ballot {
            poll_id: PollId([0xab; 32]),
            approved_indices: ap,
        };
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&payload_obj, &mut payload).unwrap();
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::BallotCast,
            hlc: Hlc { wall_ms: hlc_ms, logical: 0, device_id: DeviceId("a".into()) },
            actor: voter,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn snapshot_of(addrs: &[OwnerAddr]) -> MembershipSnapshot {
        let mut members = HashMap::new();
        for a in addrs {
            members.insert(*a, MemberAttrs { power: 10, vouching_depth: 0 });
        }
        MembershipSnapshot { members }
    }

    #[test]
    fn empty_events_zero_counts() {
        let cfg = cfg_three_opts();
        let snap = snapshot_of(&[]);
        let t = tally_tier1(&cfg, &[], &snap);
        assert_eq!(t.counts, vec![0, 0, 0]);
        assert_eq!(t.ballot_count, 0);
    }

    #[test]
    fn single_ballot_counted() {
        let cfg = cfg_three_opts();
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        let evs = vec![ballot_ev(v, 1000, vec![0, 2])];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![1, 0, 1]);
        assert_eq!(t.ballot_count, 1);
    }

    #[test]
    fn re_vote_lww_keeps_latest() {
        let cfg = cfg_three_opts();
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        let evs = vec![
            ballot_ev(v, 1000, vec![0]),
            ballot_ev(v, 2000, vec![2]), // later overrides
        ];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![0, 0, 1]);
        assert_eq!(t.ballot_count, 1);
    }

    #[test]
    fn re_vote_lww_handles_out_of_order_arrival() {
        let cfg = cfg_three_opts();
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        // Events arrive in arbitrary order; tally must still pick HLC-latest.
        let evs = vec![
            ballot_ev(v, 2000, vec![2]), // arrives first
            ballot_ev(v, 1000, vec![0]), // arrives second but older
        ];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![0, 0, 1]); // later HLC wins regardless of arrival
    }

    #[test]
    fn ineligible_voter_dropped() {
        let cfg = cfg_three_opts();
        let v_eligible = OwnerAddr([0x11; 16]);
        let v_ineligible = OwnerAddr([0x22; 16]);
        let snap = snapshot_of(&[v_eligible]); // v_ineligible not in snapshot
        let evs = vec![
            ballot_ev(v_eligible, 1000, vec![0]),
            ballot_ev(v_ineligible, 1500, vec![1]),
        ];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![1, 0, 0]);
        assert_eq!(t.ballot_count, 1);
    }

    #[test]
    fn malformed_ballot_dropped_defensively() {
        let cfg = cfg_three_opts();
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        // Ballot approving out-of-range index — validate_ballot rejects.
        let evs = vec![ballot_ev(v, 1000, vec![5])];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![0, 0, 0]);
        assert_eq!(t.ballot_count, 0);
    }

    #[test]
    fn multiple_voters_sum_correctly() {
        let cfg = cfg_three_opts();
        let v1 = OwnerAddr([0x11; 16]);
        let v2 = OwnerAddr([0x22; 16]);
        let v3 = OwnerAddr([0x33; 16]);
        let snap = snapshot_of(&[v1, v2, v3]);
        let evs = vec![
            ballot_ev(v1, 1000, vec![0, 2]),
            ballot_ev(v2, 1100, vec![0]),
            ballot_ev(v3, 1200, vec![1, 2]),
        ];
        let t = tally_tier1(&cfg, &evs, &snap);
        assert_eq!(t.counts, vec![2, 1, 2]);
        assert_eq!(t.ballot_count, 3);
    }
}
```

Note: `Hlc::compare` must already exist on the `Hlc` type for HLC ordering. If it doesn't (verify in `owner_state_types.rs`), use `(hlc.wall_ms, hlc.logical, hlc.device_id.as_str())` tuple comparison inline.

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tally_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_approval.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add Tier 1 materialize (HLC LWW + approval tally)

tally_tier1 walks BallotCast events in HLC order, keeps latest
ballot per voter via LWW, rejects ineligible voters, sums
approvals per option. Spec §4 tally algorithm steps 1-4.
Defensively drops malformed/invalid ballots (verify-on-receive
should have caught them, but materialize is the second line of
defense). Returns Tier1TallyState (per-option counts +
ballot_count + latest_ballots map for "your current vote" UI).

Result variants (NoQuorum / NoMajority / Winners) land in Task 9.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Tier 1 result variants — quorum / threshold / multi-winner

**Purpose:** Complete the spec §4 tally algorithm by adding steps 5-9 (quorum check, sort + tie-break, multi-winner selection, threshold check, Winners/NoQuorum/NoMajority result). Produces the final `PollResult` payload that gets signed and broadcast.

**Files:**
- Modify: `src-tauri/src/community_voting_approval.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_approval.rs`:

```rust
/// Final result of a Tier 1 poll. Spec §4 result variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier1Result {
    /// One or more winners (option indices, sorted ascending in case
    /// of multi-winner; first is highest count).
    #[serde(rename = "w")]
    Winners(Vec<u8>),
    /// Insufficient ballots to meet quorum requirement.
    #[serde(rename = "q")]
    NoQuorum {
        #[serde(rename = "n")]
        required: u32,
        #[serde(rename = "a")]
        actual: u32,
    },
    /// Winner exists but didn't meet the supermajority threshold.
    #[serde(rename = "m")]
    NoMajority {
        #[serde(rename = "n")]
        required_percent: u8,
        #[serde(rename = "p")]
        actual_percent: u8,
    },
}

/// Apply spec §4 tally algorithm steps 5-9 to compute the final result.
pub fn finalize_tier1(
    cfg: &Tier1PollConfig,
    tally: &Tier1TallyState,
) -> Tier1Result {
    // Step 5: quorum check.
    if let Some(q) = cfg.quorum {
        if tally.ballot_count < q {
            return Tier1Result::NoQuorum {
                required: q,
                actual: tally.ballot_count,
            };
        }
    }

    // Step 6: sort options by count descending; tie-break by index ascending.
    let mut sorted: Vec<(usize, u32)> = tally.counts.iter().enumerate().map(|(i, &c)| (i, c)).collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Step 7: multi-winner top-N.
    let mw = cfg.multi_winner.unwrap_or(1) as usize;
    let winners: Vec<(usize, u32)> = sorted.iter().take(mw).copied().collect();

    // Step 8: threshold check (only the Nth winner's count is checked,
    // per spec § 4 step 8; ballot_count must be > 0 to avoid divide-by-zero).
    if let Some(th) = cfg.threshold_percent {
        if tally.ballot_count > 0 {
            // Compute Nth-winner percent without floats: count * 100 / ballot_count.
            let nth_winner_count = winners[mw - 1].1;
            let actual_percent = ((nth_winner_count as u64 * 100) / tally.ballot_count as u64) as u8;
            if actual_percent < th {
                return Tier1Result::NoMajority {
                    required_percent: th,
                    actual_percent,
                };
            }
        }
    }

    // Step 9: return Winners (sorted ascending by index for stable output).
    let mut winner_indices: Vec<u8> = winners.iter().map(|(i, _)| *i as u8).collect();
    winner_indices.sort();
    Tier1Result::Winners(winner_indices)
}

#[cfg(test)]
mod result_tests {
    use super::*;

    fn cfg(opts: usize, quorum: Option<u32>, threshold: Option<u8>, mw: Option<u8>) -> Tier1PollConfig {
        Tier1PollConfig {
            options: (0..opts).map(|i| format!("opt{i}")).collect(),
            window_seconds: 3600,
            quorum,
            threshold_percent: threshold,
            multi_winner: mw,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id_bytes: [0; 16],
        }
    }

    fn tally(counts: Vec<u32>, ballot_count: u32) -> Tier1TallyState {
        Tier1TallyState {
            counts,
            ballot_count,
            latest_ballots: HashMap::new(),
        }
    }

    #[test]
    fn single_winner_clear_majority() {
        let r = finalize_tier1(&cfg(3, None, None, None), &tally(vec![5, 2, 1], 8));
        assert_eq!(r, Tier1Result::Winners(vec![0]));
    }

    #[test]
    fn tie_break_by_lower_index() {
        let r = finalize_tier1(&cfg(3, None, None, None), &tally(vec![3, 3, 1], 7));
        assert_eq!(r, Tier1Result::Winners(vec![0])); // option 0 wins on tie-break
    }

    #[test]
    fn no_quorum_emitted() {
        let r = finalize_tier1(&cfg(3, Some(10), None, None), &tally(vec![3, 2, 1], 6));
        assert_eq!(
            r,
            Tier1Result::NoQuorum { required: 10, actual: 6 }
        );
    }

    #[test]
    fn no_majority_emitted() {
        // Winner has 3 of 10 (30%), threshold 50%.
        let r = finalize_tier1(&cfg(3, None, Some(50), None), &tally(vec![3, 2, 1], 10));
        assert_eq!(
            r,
            Tier1Result::NoMajority { required_percent: 50, actual_percent: 30 }
        );
    }

    #[test]
    fn supermajority_threshold_passes() {
        let r = finalize_tier1(&cfg(3, None, Some(50), None), &tally(vec![7, 2, 1], 10));
        assert_eq!(r, Tier1Result::Winners(vec![0]));
    }

    #[test]
    fn multi_winner_top_two() {
        let r = finalize_tier1(&cfg(4, None, None, Some(2)), &tally(vec![5, 4, 2, 1], 12));
        assert_eq!(r, Tier1Result::Winners(vec![0, 1]));
    }

    #[test]
    fn multi_winner_with_threshold_checks_nth_winner() {
        // 4 options, mw=2, threshold 30%. Counts [5,4,2,1], ballots=12.
        // Nth (2nd) winner = option 1 with 4/12 = 33%. Passes.
        let r = finalize_tier1(&cfg(4, None, Some(30), Some(2)), &tally(vec![5, 4, 2, 1], 12));
        assert_eq!(r, Tier1Result::Winners(vec![0, 1]));

        // Same setup but threshold 50%. 2nd winner only 33%. NoMajority.
        let r2 = finalize_tier1(&cfg(4, None, Some(50), Some(2)), &tally(vec![5, 4, 2, 1], 12));
        assert_eq!(
            r2,
            Tier1Result::NoMajority { required_percent: 50, actual_percent: 33 }
        );
    }

    #[test]
    fn quorum_checked_before_threshold() {
        // 5 ballots, quorum 10 → NoQuorum even though threshold would pass.
        let r = finalize_tier1(&cfg(3, Some(10), Some(50), None), &tally(vec![5, 0, 0], 5));
        assert!(matches!(r, Tier1Result::NoQuorum { .. }));
    }

    #[test]
    fn empty_ballots_with_no_constraints() {
        // No quorum, no threshold, no ballots → winner is option 0 (tied at 0, tie-break lowest index).
        let r = finalize_tier1(&cfg(3, None, None, None), &tally(vec![0, 0, 0], 0));
        assert_eq!(r, Tier1Result::Winners(vec![0]));
    }

    #[test]
    fn result_round_trips_via_cbor() {
        for r in &[
            Tier1Result::Winners(vec![0, 2]),
            Tier1Result::NoQuorum { required: 5, actual: 2 },
            Tier1Result::NoMajority { required_percent: 50, actual_percent: 33 },
        ] {
            let encoded = ciborium::ser::into_vec(r).expect("encode");
            let decoded: Tier1Result =
                ciborium::de::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*r, decoded);
        }
    }
}
```

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(result_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_approval.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add Tier 1 result variants (quorum/threshold/multi-winner)

finalize_tier1 completes spec §4 tally algorithm steps 5-9:
quorum check (emits NoQuorum if ballot_count < q), sort by count
desc with index-asc tie-break, multi-winner top-N selection,
threshold check on Nth winner (emits NoMajority if Nth count
percent < th), else Winners(sorted_indices). Quorum check
precedes threshold check (latter only fires when quorum met).
Result is wire-stable CBOR (Winners="w", NoQuorum="q",
NoMajority="m") suitable for embedding in PollResult event pd.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: PollResult event + R2 reproducibility verifier

**Purpose:** Define the `PollResult` payload type (CBOR-encoded result + poll-id ref) and the `verify_poll_result_reproducible` function that other nodes use to verify R2 (spec §8: the result must match deterministically-recomputed tally from the event log). This is what makes `PollResult` signable by anyone, not just an admin.

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`
- Modify: `src-tauri/src/community_voting_approval.rs`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_approval.rs`:

```rust
/// PollResult payload, CBOR-encoded as the envelope's pd field
/// for kd="rs" events. Tier-agnostic discriminator + tier-specific
/// result bytes; Tier 1 uses Tier1Result encoded inside `rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1PollResultPayload {
    /// PollId this result is for.
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// The computed Tier1Result.
    #[serde(rename = "rs")]
    pub result: Tier1Result,
}

/// Verify that a candidate PollResult matches the deterministically-
/// computed tally from the event log. Spec §8 R2.
///
/// Used by:
///   - voting_log apply() when accepting a PollResult event (other
///     node may have signed it; we recompute to confirm).
///   - voting_create_poll_result() pre-flight (our own node signing).
pub fn verify_poll_result_reproducible(
    candidate: &Tier1PollResultPayload,
    cfg: &Tier1PollConfig,
    events: &[SignedVotingEvent],
    snapshot: &MembershipSnapshot,
) -> bool {
    let tally = tally_tier1(cfg, events, snapshot);
    let computed = finalize_tier1(cfg, &tally);
    computed == candidate.result
}

#[cfg(test)]
mod result_reproducibility_tests {
    use super::*;
    use crate::community_voting_core::{MemberAttrs, Tier};
    use crate::owner_state_types::{DeviceId, Hlc, OwnerAddr};

    fn cfg_two_opts() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["yes".into(), "no".into()],
            window_seconds: 600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id_bytes: [0; 16],
        }
    }

    fn snapshot_of(addrs: &[OwnerAddr]) -> MembershipSnapshot {
        let mut members = HashMap::new();
        for a in addrs {
            members.insert(*a, MemberAttrs { power: 10, vouching_depth: 0 });
        }
        MembershipSnapshot { members }
    }

    fn ballot(voter: OwnerAddr, hlc_ms: u64, ap: Vec<u8>, pid: PollId) -> SignedVotingEvent {
        let payload_obj = Tier1Ballot { poll_id: pid, approved_indices: ap };
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&payload_obj, &mut payload).unwrap();
        SignedVotingEvent {
            tag: 'p', version: 1, tier: Tier::Approval,
            kind: crate::community_voting_core::PollEventKindCode::BallotCast,
            hlc: Hlc { wall_ms: hlc_ms, logical: 0, device_id: DeviceId("a".into()) },
            actor: voter,
            payload,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn matching_result_verifies() {
        let cfg = cfg_two_opts();
        let pid = PollId([0xab; 32]);
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        let events = vec![ballot(v, 1000, vec![0], pid)];
        let candidate = Tier1PollResultPayload {
            poll_id: pid,
            result: Tier1Result::Winners(vec![0]),
        };
        assert!(verify_poll_result_reproducible(&candidate, &cfg, &events, &snap));
    }

    #[test]
    fn wrong_winner_rejected() {
        let cfg = cfg_two_opts();
        let pid = PollId([0xab; 32]);
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        let events = vec![ballot(v, 1000, vec![0], pid)];
        let candidate = Tier1PollResultPayload {
            poll_id: pid,
            result: Tier1Result::Winners(vec![1]), // wrong
        };
        assert!(!verify_poll_result_reproducible(&candidate, &cfg, &events, &snap));
    }

    #[test]
    fn fabricated_no_quorum_rejected_if_no_quorum_configured() {
        let cfg = cfg_two_opts();
        let pid = PollId([0xab; 32]);
        let v = OwnerAddr([0x11; 16]);
        let snap = snapshot_of(&[v]);
        let events = vec![ballot(v, 1000, vec![0], pid)];
        let candidate = Tier1PollResultPayload {
            poll_id: pid,
            result: Tier1Result::NoQuorum { required: 5, actual: 1 },
        };
        assert!(!verify_poll_result_reproducible(&candidate, &cfg, &events, &snap));
    }
}
```

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(result_reproducibility_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_approval.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add PollResult payload + R2 reproducibility verifier

Tier1PollResultPayload is the CBOR-encoded pd for kd="rs" events:
{ pi: PollId, rs: Tier1Result }. verify_poll_result_reproducible
recomputes the tally from the event log and confirms the candidate
result matches — this is spec §8 R2, the property that makes
PollResult signable by any node (not just an admin). Any honest
node that observes the same event log signs the same result; bots
recompute on receive and reject mismatches.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: IPC commands + Tauri events

**Purpose:** Wire the 4 Phase 1 IPCs (`voting_create_tier1_poll`, `voting_cast_tier1_ballot`, `voting_list_active_polls`, `voting_get_poll`) into `lib.rs`, registering them with Tauri and emitting the 3 events (`voting-poll-created`, `voting-ballot-cast`, `voting-poll-closed`) at appropriate points.

**Files:**
- Modify: `src-tauri/src/lib.rs` (add 4 `#[tauri::command]` functions + register `VotingLog` field on `NodeState` + add to `tauri::generate_handler!` list)

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_core.rs` (the helpers needed by IPC):

```rust
/// Build a fully-signed PollCreate event for Tier 1, ready to broadcast.
/// Used by voting_create_tier1_poll IPC.
pub fn build_signed_poll_create_tier1(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    tier: Tier,
    config: &crate::community_voting_approval::Tier1PollConfig,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(config, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier,
        kind: PollEventKindCode::PollCreate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    use ed25519_dalek::Signer;
    let sig = keypair.sign(&sb);
    ev.sig = sig.to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed BallotCast event for Tier 1.
pub fn build_signed_ballot_tier1(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    approved_indices: Vec<u8>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    let ballot = crate::community_voting_approval::Tier1Ballot { poll_id, approved_indices };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&ballot, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Approval,
        kind: PollEventKindCode::BallotCast,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    use ed25519_dalek::Signer;
    let sig = keypair.sign(&sb);
    ev.sig = sig.to_bytes().to_vec();
    Ok(ev)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    EncodePayload,
    SigningBytes,
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::community_voting_approval::Tier1PollConfig;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn signed_poll_create_round_trip() {
        let mut csprng = OsRng;
        let keypair = SigningKey::generate(&mut csprng);
        let actor = OwnerAddr([0xaa; 16]);
        let cfg = Tier1PollConfig {
            options: vec!["a".into(), "b".into()],
            window_seconds: 600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id_bytes: [0; 16],
        };
        let hlc = Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: crate::owner_state_types::DeviceId("a".into()),
        };
        let ev = build_signed_poll_create_tier1(&keypair, actor, Tier::Approval, &cfg, hlc)
            .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::PollCreate);
        assert_eq!(ev.actor, actor);
        // Verify the signature against the actor's public key.
        let sb = ev.signing_bytes().expect("signing bytes");
        let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        use ed25519_dalek::Verifier;
        keypair.verifying_key().verify(&sb, &sig).expect("verify");
    }
}
```

Then in `src-tauri/src/lib.rs`, add the 4 IPC commands following the existing `community_*` IPC pattern. Sketch:

```rust
use crate::community_voting_core::{
    build_signed_ballot_tier1, build_signed_poll_create_tier1, derive_poll_id, PollId, PollMeta,
    Tier,
};
use crate::community_voting_approval::Tier1PollConfig;
use crate::community_voting_log::{ApplyError, VotingLog};

#[tauri::command]
pub async fn voting_create_tier1_poll(
    community_id: String,
    channel_id: String,
    options: Vec<String>,
    window_seconds: u32,
    min_power: u8,
    min_vouching_depth: Option<u8>,
    quorum: Option<u32>,
    threshold_percent: Option<u8>,
    multi_winner: Option<u8>,
    state: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    // 1. Parse community_id / channel_id as 16-byte structures.
    // 2. Build Tier1PollConfig from args.
    // 3. validate_poll_config — return Err if invalid.
    // 4. Pre-flight eligibility check (this node's actor must meet min_power
    //    in community_membership snapshot — fail fast UX).
    // 5. Take node lock; build current HLC; build_signed_poll_create_tier1.
    // 6. Apply to local VotingLog (must succeed — we constructed it).
    // 7. Broadcast to Zenoh topic (Task 12 wires the actual publish; this
    //    step just calls the publish hook).
    // 8. Emit Tauri event "voting-poll-created" with poll_id + channel_id.
    // 9. Return PollId as hex string.
    todo!() // replace with implementation per spec §4 IPC commands
}

#[tauri::command]
pub async fn voting_cast_tier1_ballot(
    poll_id: String,
    approved_indices: Vec<u8>,
    state: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // 1. Parse poll_id from hex.
    // 2. Look up PollState in this node's voting_log — error if unknown
    //    or not Tier::Approval or not Open.
    // 3. Pre-flight: validate_ballot against PollMeta's stored cfg
    //    (cfg stored alongside PollMeta — see Task 7).
    // 4. Pre-flight eligibility check against PollCreate.hlc snapshot.
    // 5. Build signed BallotCast via build_signed_ballot_tier1.
    // 6. Apply locally + broadcast.
    // 7. Emit Tauri event "voting-ballot-cast" with poll_id, voter, approved_count.
    todo!()
}

#[tauri::command]
pub async fn voting_list_active_polls(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<PollMeta>, String> {
    // 1. Parse community_id.
    // 2. Lock state; iterate voting_log polls; filter to lifecycle == Open.
    // 3. Return PollMeta vector.
    todo!()
}

#[tauri::command]
pub async fn voting_get_poll(
    poll_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<PollStateExport, String> {
    // PollStateExport is a serialize-friendly subset of PollState
    // (PollMeta + current tally counts + ballot_count + your_ballot).
    // Defined in voting_core.rs.
    todo!()
}
```

Add `voting_create_tier1_poll, voting_cast_tier1_ballot, voting_list_active_polls, voting_get_poll` to the `tauri::generate_handler!` list. Add a `voting_logs: HashMap<SpaceId, VotingLog>` field on `NodeState`.

Define `PollStateExport`:

```rust
// In community_voting_core.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollStateExport {
    pub meta: PollMeta,
    pub tally: TallyExport,
    pub your_ballot: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TallyExport {
    pub counts: Vec<u32>,
    pub ballot_count: u32,
}
```

Implementer notes for this task:
- **Pre-flight checks before signing** prevent "you signed an event that the network will reject" UX failures.
- **Eligibility snapshot** for Tier 1 is at PollCreate.hlc — store the snapshot inside `PollState` (extend the struct) when materializing PollCreate so subsequent ballots use the same snapshot.
- **Tauri event payloads** use camelCase keys at the IPC boundary (Tauri auto-converts).
- **Emit events after apply, not before** — guarantees subscribers see consistent state.
- **Per `feedback_tauri_error_extraction`**: production rejections are strings; tests use Error objects. The frontend code uses `e instanceof Error ? e.message : String(e)`.

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(build_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

Once IPCs compile (no `todo!()` left), also run the full Rust suite to confirm no regressions:

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_core.rs src-tauri/src/community_voting_approval.rs src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): wire 4 Tier 1 IPC commands + 3 Tauri events

voting_create_tier1_poll / voting_cast_tier1_ballot /
voting_list_active_polls / voting_get_poll registered with Tauri.
build_signed_poll_create_tier1 + build_signed_ballot_tier1 helpers
in voting_core handle Ed25519 signing over canonical CBOR signing-
bytes. IPCs apply locally + broadcast (Zenoh publish hook stub —
Task 12 wires actual transport) + emit Tauri events
(voting-poll-created / voting-ballot-cast / voting-poll-closed).
Pre-flight validation + eligibility check fail fast so UI gets
useful error before signing a doomed event.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Zenoh sync — publish + subscribe + auto-close

**Purpose:** Wire the per-community Zenoh topic (`harmony/community/{id}/voting`) for publish + subscribe of voting events. Also add the auto-close tick that emits `PollClose` events when the window expires (any node observing window-expired can sign it).

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` (add sync engine following `community_channel_log_engine` pattern)
- Modify: `src-tauri/src/lib.rs` (start/stop voting sync engine alongside channel-log sync)

- [ ] **Step 1: Pre-task — study the existing sync engine pattern**

```bash
grep -n "pub struct\|pub async fn\|tokio::select\|notify_waiters\|stop\|closing" src-tauri/src/community_channel_log_engine.rs | head -30
```

Use the resulting structure to mirror in `community_voting_log_engine` (or inline within `community_voting_log` — implementer's choice based on whether the engine grows large enough to deserve its own file).

- [ ] **Step 2: Write the failing test (multi-engine convergence smoke)**

Create a minimal test in `community_voting_log.rs`:

```rust
#[cfg(test)]
mod sync_tests {
    // Just a stub here — full multi-engine convergence test
    // lives in tests/community_voting_tier1_integration.rs (Task 15).
    // This file's tests stay unit-scoped.

    #[tokio::test]
    async fn auto_close_emits_poll_close_after_window() {
        // 1. Build VotingLog with a poll that has window_seconds = 1.
        // 2. Advance HLC to PollCreate.hlc + 2 seconds.
        // 3. Call auto_close_expired_polls(); expect one PollClose event
        //    appended to log; expect PollState.lifecycle == Closed.
        // 4. Re-invoke; expect no second PollClose (idempotent).
        todo!()
    }
}
```

Implement `auto_close_expired_polls` in `community_voting_log.rs`:

```rust
impl VotingLog {
    /// Emit PollClose events for any Open poll whose window has expired.
    /// Caller passes the current HLC and the node's signing key + actor.
    /// Idempotent: re-calling after close is a no-op.
    pub fn auto_close_expired_polls(
        &mut self,
        community_id: &crate::owner_state_types::SpaceId,
        now_hlc: &crate::owner_state_types::Hlc,
        keypair: &ed25519_dalek::SigningKey,
        actor: crate::owner_state_types::OwnerAddr,
    ) -> Vec<crate::community_voting_core::PollId> {
        // For each PollState in Open whose meta.closes_at <= now_hlc:
        //   Build a PollClose event signed by `keypair`.
        //   Apply it to self.
        //   Push the broadcast queue (caller drains and publishes via Zenoh).
        todo!()
    }
}
```

Zenoh integration follows the pattern in `community_channel_log_engine.rs`: a tokio task subscribes to `harmony/community/{id}/voting`, calls `apply()` on every received event, and a separate publish-side dispatches the broadcast queue.

- [ ] **Step 3: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sync_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): wire Zenoh sync + auto-close-on-window-expiry

Per-community Zenoh topic harmony/community/{id}/voting publishes +
subscribes voting events; sync engine mirrors community_channel_log
pattern. auto_close_expired_polls() emits PollClose for any Open
poll whose meta.closes_at has been reached at the node's current
HLC; idempotent on re-invocation. Closes the loop: ballots round-
trip via Zenoh, expire on time, materialize to a PollResult any
node can sign per R2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: 90-day archive sweep

**Purpose:** Implement the daily archive sweep that drops per-ballot events for polls finalized >90 days ago, keeping only `PollMeta` + `PollResult` for long-term audit. Wired into the existing periodic tick loop in `lib.rs`.

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs`
- Modify: `src-tauri/src/lib.rs` (wire archive_voting_logs into existing periodic tick)

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/community_voting_log.rs`:

```rust
impl VotingLog {
    /// Sweep polls finalized > 90 days ago. Drop per-ballot events;
    /// keep only PollMeta + final PollResult. Updates lifecycle to
    /// Archived. Idempotent.
    pub fn archive_finalized_polls(
        &mut self,
        now_wall_ms: u64,
    ) -> Vec<crate::community_voting_core::PollId> {
        const NINETY_DAYS_MS: u64 = 90 * 24 * 60 * 60 * 1000;
        let mut archived = Vec::new();
        for (pid, state) in self.polls.iter_mut() {
            if state.meta.lifecycle == crate::community_voting_core::Lifecycle::Finalized {
                // Find the PollResult event's wall_ms.
                let finalized_at = state
                    .events
                    .iter()
                    .find(|e| e.kind == crate::community_voting_core::PollEventKindCode::PollResult)
                    .map(|e| e.hlc.wall_ms);
                if let Some(fin_at) = finalized_at {
                    if now_wall_ms.saturating_sub(fin_at) > NINETY_DAYS_MS {
                        state.events.retain(|e| {
                            matches!(
                                e.kind,
                                crate::community_voting_core::PollEventKindCode::PollCreate
                                    | crate::community_voting_core::PollEventKindCode::PollResult
                            )
                        });
                        state.meta.lifecycle = crate::community_voting_core::Lifecycle::Archived;
                        archived.push(*pid);
                    }
                }
            }
        }
        archived
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use crate::community_voting_core::{Lifecycle, PollEventKindCode, Tier};
    use crate::owner_state_types::{DeviceId, Hlc, OwnerAddr, SpaceId};

    fn make_finalized_log_with_n_ballots(finalized_at_ms: u64, n_ballots: usize) -> (VotingLog, PollId) {
        let mut log = VotingLog::new();
        let cid = SpaceId([0xcc; 16]);
        // Build a minimal Finalized poll. (Use Task 7's machinery to
        // build a valid PollCreate; for this test we hand-construct
        // PollState directly via the public API to avoid pulling in
        // the whole stack.)
        // Implementer: see helper in Task 8 tally_tests for the pattern.
        todo!()
    }

    #[test]
    fn old_finalized_poll_archived() {
        let (mut log, pid) = make_finalized_log_with_n_ballots(0, 10);
        let now_ms = 91 * 24 * 60 * 60 * 1000; // 91 days
        let archived = log.archive_finalized_polls(now_ms);
        assert_eq!(archived, vec![pid]);
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Archived);
        // Only PollCreate + PollResult retained.
        assert_eq!(log.polls[&pid].events.len(), 2);
    }

    #[test]
    fn young_finalized_poll_kept() {
        let (mut log, pid) = make_finalized_log_with_n_ballots(0, 10);
        let now_ms = 89 * 24 * 60 * 60 * 1000;
        let archived = log.archive_finalized_polls(now_ms);
        assert!(archived.is_empty());
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Finalized);
    }

    #[test]
    fn open_poll_not_archived() {
        let mut log = VotingLog::new();
        // Add an Open poll (still in Open state). archive_finalized_polls
        // must not touch it.
        let now_ms = 999 * 24 * 60 * 60 * 1000;
        let archived = log.archive_finalized_polls(now_ms);
        assert!(archived.is_empty());
    }
}
```

Wire `archive_voting_logs(node_state)` into the existing periodic tick loop in `lib.rs` (the loop that already runs daily housekeeping — look for `tokio::time::interval` calls in `start_node` or similar).

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(archive_tests)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-290): add 90-day archive sweep for finalized polls

archive_finalized_polls() walks all PollStates in Finalized
lifecycle whose PollResult event is > 90 days old (wall-clock);
retains only PollCreate + PollResult events; transitions
lifecycle to Archived. Idempotent. Wired into the existing
daily housekeeping tick in lib.rs. Per spec §2: archived polls
keep meta + result forever for audit; per-ballot events are
dropped to bound disk use over a community's lifetime.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: Wire-format fixtures (byte-pinned canonical CBOR)

**Purpose:** Lock the wire-format bytes for the 6 Phase 1 event kinds (PollCreate, PollOpen, PollExtend, PollClose, BallotCast, PollResult) using the regen-on-first-run pattern (per ZEB-250 and ZEB-254). Future wire-format changes will fail loudly with a hex diff, forcing explicit cross-version-compat review.

**Files:**
- Create: `src-tauri/tests/wire_format_zeb290_fixtures.rs`

- [ ] **Step 1: Write the fixture file with FILL_AFTER placeholders**

Create `src-tauri/tests/wire_format_zeb290_fixtures.rs`:

```rust
//! ZEB-290: Byte-pinned canonical CBOR fixtures for Phase 1 voting events.
//!
//! Locks the canonical-CBOR wire encoding for the 6 Phase 1 event
//! kinds (PollCreate / PollOpen / PollExtend / PollClose / BallotCast
//! / PollResult). Any failure here is a wire-protocol break — review
//! carefully before updating the pinned bytes (cross-version compat,
//! peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte values) so
//! the encoded bytes are byte-stable across runs.

use harmony_app::community_voting_approval::{
    Tier1Ballot, Tier1PollConfig, Tier1PollResultPayload, Tier1Result,
};
use harmony_app::community_voting_core::{
    Eligibility, PollEventKindCode, PollId, SignedVotingEvent, Tier,
};
use harmony_app::owner_state_types::{DeviceId, Hlc, OwnerAddr};

const FIXTURE_POLL_ID: PollId = PollId([0xab; 32]);
const FIXTURE_ACTOR: OwnerAddr = OwnerAddr([0xaa; 16]);
const FIXTURE_CHANNEL_BYTES: [u8; 16] = [0xcc; 16];

// Replace FILL_AFTER values with the hex panics from first run.

const EXPECTED_TIER1_POLLCONFIG_HEX: &str = "FILL_AFTER";
const EXPECTED_TIER1_BALLOT_HEX: &str = "FILL_AFTER";
const EXPECTED_TIER1_POLLRESULT_HEX: &str = "FILL_AFTER";
const EXPECTED_ENVELOPE_POLLCREATE_HEX: &str = "FILL_AFTER";
const EXPECTED_ENVELOPE_BALLOTCAST_HEX: &str = "FILL_AFTER";
const EXPECTED_ENVELOPE_POLLCLOSE_HEX: &str = "FILL_AFTER";

fn fixture_hlc() -> Hlc {
    Hlc { wall_ms: 1_000, logical: 0, device_id: DeviceId("d".into()) }
}

fn fixture_config() -> Tier1PollConfig {
    Tier1PollConfig {
        options: vec!["Pizza".into(), "Burgers".into(), "Sushi".into()],
        window_seconds: 3600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id_bytes: FIXTURE_CHANNEL_BYTES,
    }
}

fn encode_envelope(kind: PollEventKindCode, payload: Vec<u8>) -> Vec<u8> {
    let ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Approval,
        kind,
        hlc: fixture_hlc(),
        actor: FIXTURE_ACTOR,
        payload,
        sig: vec![0u8; 64],
    };
    let mut out = Vec::new();
    ciborium::ser::into_writer(&ev, &mut out).expect("encode");
    out
}

#[test]
fn tier1_pollconfig_canonical_cbor() {
    let cfg = fixture_config();
    let encoded = ciborium::ser::into_vec(&cfg).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_TIER1_POLLCONFIG_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_TIER1_POLLCONFIG_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(actual_hex, EXPECTED_TIER1_POLLCONFIG_HEX, "Tier1PollConfig wire format changed");
}

#[test]
fn tier1_ballot_canonical_cbor() {
    let ballot = Tier1Ballot {
        poll_id: FIXTURE_POLL_ID,
        approved_indices: vec![0, 2],
    };
    let encoded = ciborium::ser::into_vec(&ballot).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_TIER1_BALLOT_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TIER1_BALLOT_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(actual_hex, EXPECTED_TIER1_BALLOT_HEX, "Tier1Ballot wire format changed");
}

#[test]
fn tier1_pollresult_canonical_cbor() {
    let r = Tier1PollResultPayload {
        poll_id: FIXTURE_POLL_ID,
        result: Tier1Result::Winners(vec![0, 2]),
    };
    let encoded = ciborium::ser::into_vec(&r).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_TIER1_POLLRESULT_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TIER1_POLLRESULT_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(actual_hex, EXPECTED_TIER1_POLLRESULT_HEX, "Tier1PollResult wire format changed");
}

#[test]
fn envelope_pollcreate_canonical_cbor() {
    let cfg_bytes = ciborium::ser::into_vec(&fixture_config()).expect("encode config");
    let encoded = encode_envelope(PollEventKindCode::PollCreate, cfg_bytes);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_POLLCREATE_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_POLLCREATE_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(actual_hex, EXPECTED_ENVELOPE_POLLCREATE_HEX, "PollCreate envelope wire format changed");

    // Structural check: 8 top-level keys, all 2-char.
    let value: ciborium::Value =
        ciborium::de::from_reader(&encoded[..]).expect("decode");
    let map = value.as_map().expect("map");
    assert_eq!(map.len(), 8);
    for (k, _) in map.iter() {
        assert_eq!(k.as_text().unwrap().len(), 2);
    }
}

#[test]
fn envelope_ballotcast_canonical_cbor() {
    let ballot = Tier1Ballot {
        poll_id: FIXTURE_POLL_ID,
        approved_indices: vec![0, 2],
    };
    let payload = ciborium::ser::into_vec(&ballot).expect("encode ballot");
    let encoded = encode_envelope(PollEventKindCode::BallotCast, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_BALLOTCAST_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_BALLOTCAST_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(actual_hex, EXPECTED_ENVELOPE_BALLOTCAST_HEX, "BallotCast envelope wire format changed");
}

#[test]
fn envelope_pollclose_canonical_cbor() {
    let mut pd = Vec::new();
    #[derive(serde::Serialize)]
    struct CloseRef {
        #[serde(rename = "pi")]
        pi: PollId,
    }
    ciborium::ser::into_writer(&CloseRef { pi: FIXTURE_POLL_ID }, &mut pd).unwrap();
    let encoded = encode_envelope(PollEventKindCode::PollClose, pd);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_POLLCLOSE_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_POLLCLOSE_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(actual_hex, EXPECTED_ENVELOPE_POLLCLOSE_HEX, "PollClose envelope wire format changed");
}
```

- [ ] **Step 2: Run tests; they will panic with the hex values to paste in**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wire_format_zeb290)' && cd ..
```

Expected: each test panics with `REGENERATE EXPECTED_*_HEX = "..."` lines. Copy the hex strings into the corresponding `const` lines, replacing `"FILL_AFTER"`.

- [ ] **Step 3: Re-run tests; all should pass with pinned hex**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wire_format_zeb290)' && cd ..
```

Expected: all 6 fixture tests pass.

- [ ] **Step 4: Verify all five CI gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/wire_format_zeb290_fixtures.rs
git commit -m "$(cat <<'EOF'
test(zeb-290): pin canonical CBOR fixtures for 6 voting event kinds

Byte-pinned wire-format fixtures for Tier1PollConfig, Tier1Ballot,
Tier1PollResult, plus envelopes for PollCreate / BallotCast /
PollClose. Uses the regen-on-first-run pattern (ZEB-250 / ZEB-254):
hex constants start as "FILL_AFTER"; first run panics with the
actual hex to paste back in. Locks wire format; any future change
breaks these tests loudly, forcing explicit cross-version compat
review before peer-interop is broken. Structural assertions on
PollCreate envelope confirm 8 top-level keys all 2-char (same-
length-keys invariant).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: Multi-engine integration test (two-engine convergence)

**Purpose:** End-to-end test where two in-process engines each create / cast / observe poll events and converge on identical tallies. Catches any convergence bugs in materialize / sync / lifecycle that unit tests can't surface.

**Files:**
- Create: `src-tauri/tests/community_voting_tier1_integration.rs`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/tests/community_voting_tier1_integration.rs`:

```rust
//! ZEB-290: integration tests for Tier 1 Approval voting.
//!
//! Multi-engine scenarios exercising end-to-end semantics — create
//! poll, cast ballots across engines, observe convergence on
//! identical tally and PollResult. Mirrors the structure of
//! community_admin_quorum_integration.rs (ZEB-250) and
//! community_channel_messages_integration.rs (ZEB-248).

use harmony_app::community_voting_approval::{
    finalize_tier1, tally_tier1, Tier1PollConfig, Tier1Result,
};
use harmony_app::community_voting_core::{
    build_signed_ballot_tier1, build_signed_poll_create_tier1, check_eligibility,
    derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot, PollEventKindCode,
    PollId, Tier,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::owner_state_types::{DeviceId, Hlc, OwnerAddr, SpaceId};

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::collections::HashMap;

const COMMUNITY: SpaceId = SpaceId([0xcc; 16]);

fn snapshot_for(addrs: &[OwnerAddr]) -> MembershipSnapshot {
    let mut members = HashMap::new();
    for a in addrs {
        members.insert(*a, MemberAttrs { power: 10, vouching_depth: 0 });
    }
    MembershipSnapshot { members }
}

fn config_three_options() -> Tier1PollConfig {
    Tier1PollConfig {
        options: vec!["Yes".into(), "No".into(), "Maybe".into()],
        window_seconds: 3600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id_bytes: [0xee; 16],
    }
}

#[test]
fn two_engines_converge_on_tally() {
    // Engine A and Engine B both have logs; we simulate Zenoh sync by
    // hand-applying each event to both logs (so we test materialize
    // determinism without the actual transport — transport is covered
    // by community_sync_integration tests).
    let mut log_a = VotingLog::new();
    let mut log_b = VotingLog::new();

    let mut csprng = OsRng;
    let kp_creator = SigningKey::generate(&mut csprng);
    let kp_voter1 = SigningKey::generate(&mut csprng);
    let kp_voter2 = SigningKey::generate(&mut csprng);

    let actor_creator = OwnerAddr([0x01; 16]);
    let actor_voter1 = OwnerAddr([0x02; 16]);
    let actor_voter2 = OwnerAddr([0x03; 16]);
    let snap = snapshot_for(&[actor_creator, actor_voter1, actor_voter2]);

    let cfg = config_three_options();
    let create_hlc = Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId("c".into()) };
    let create_ev = build_signed_poll_create_tier1(
        &kp_creator,
        actor_creator,
        Tier::Approval,
        &cfg,
        create_hlc,
    )
    .expect("build create");

    // Both engines see the PollCreate.
    let pid_a = log_a.apply(create_ev.clone(), &COMMUNITY).expect("a apply create");
    let pid_b = log_b.apply(create_ev.clone(), &COMMUNITY).expect("b apply create");
    assert_eq!(pid_a, pid_b, "deterministic PollId derivation");

    // Voter 1 casts ballot through engine A.
    let b1 = build_signed_ballot_tier1(
        &kp_voter1,
        actor_voter1,
        pid_a,
        vec![0],
        Hlc { wall_ms: 1100, logical: 0, device_id: DeviceId("v1".into()) },
    )
    .expect("build b1");
    log_a.apply(b1.clone(), &COMMUNITY).expect("a apply b1");
    log_b.apply(b1.clone(), &COMMUNITY).expect("b apply b1");

    // Voter 2 casts ballot through engine B.
    let b2 = build_signed_ballot_tier1(
        &kp_voter2,
        actor_voter2,
        pid_a,
        vec![0, 2],
        Hlc { wall_ms: 1200, logical: 0, device_id: DeviceId("v2".into()) },
    )
    .expect("build b2");
    log_a.apply(b2.clone(), &COMMUNITY).expect("a apply b2");
    log_b.apply(b2.clone(), &COMMUNITY).expect("b apply b2");

    // Both engines compute identical tally + result.
    let events_a = &log_a.polls[&pid_a].events;
    let events_b = &log_b.polls[&pid_a].events;
    let tally_a = tally_tier1(&cfg, events_a, &snap);
    let tally_b = tally_tier1(&cfg, events_b, &snap);
    assert_eq!(tally_a, tally_b, "tally must converge");
    assert_eq!(tally_a.counts, vec![2, 0, 1]);
    assert_eq!(tally_a.ballot_count, 2);

    let result_a = finalize_tier1(&cfg, &tally_a);
    let result_b = finalize_tier1(&cfg, &tally_b);
    assert_eq!(result_a, result_b);
    assert_eq!(result_a, Tier1Result::Winners(vec![0]));
}

#[test]
fn out_of_order_event_arrival_still_converges() {
    let mut log_a = VotingLog::new();
    let mut log_b = VotingLog::new();

    let mut csprng = OsRng;
    let kp = SigningKey::generate(&mut csprng);
    let voter = OwnerAddr([0x11; 16]);
    let snap = snapshot_for(&[voter]);

    let cfg = config_three_options();
    let create_ev = build_signed_poll_create_tier1(
        &kp,
        voter,
        Tier::Approval,
        &cfg,
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId("c".into()) },
    )
    .expect("build");
    let pid = log_a.apply(create_ev.clone(), &COMMUNITY).expect("a");
    log_b.apply(create_ev, &COMMUNITY).expect("b");

    let b_early = build_signed_ballot_tier1(
        &kp,
        voter,
        pid,
        vec![0],
        Hlc { wall_ms: 1100, logical: 0, device_id: DeviceId("v".into()) },
    )
    .expect("b_early");
    let b_late = build_signed_ballot_tier1(
        &kp,
        voter,
        pid,
        vec![2],
        Hlc { wall_ms: 1200, logical: 0, device_id: DeviceId("v".into()) },
    )
    .expect("b_late");

    // Engine A: arrival order = early then late.
    log_a.apply(b_early.clone(), &COMMUNITY).expect("a early");
    log_a.apply(b_late.clone(), &COMMUNITY).expect("a late");
    // Engine B: arrival order = late then early (partition heal).
    log_b.apply(b_late, &COMMUNITY).expect("b late");
    log_b.apply(b_early, &COMMUNITY).expect("b early");

    // Both must converge: LWW by HLC, so b_late wins. counts = [0, 0, 1].
    let tally_a = tally_tier1(&cfg, &log_a.polls[&pid].events, &snap);
    let tally_b = tally_tier1(&cfg, &log_b.polls[&pid].events, &snap);
    assert_eq!(tally_a.counts, vec![0, 0, 1]);
    assert_eq!(tally_b.counts, vec![0, 0, 1]);
    assert_eq!(tally_a, tally_b);
}
```

- [ ] **Step 2: Run + verify passes + 5 gates**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier1_integration)' && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cd ..
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/community_voting_tier1_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-290): two-engine integration tests for Tier 1 convergence

Two scenarios: (1) two engines independently apply PollCreate + 2
ballots and compute identical tally + Tier1Result::Winners; (2)
events arrive out of order at engine A vs engine B (simulating
partition heal); LWW by HLC means both engines still converge.
PollId derivation determinism guaranteed by SHA-256 of signing
bytes. Mirrors structure of community_admin_quorum_integration.rs
(ZEB-250) and community_channel_messages_integration.rs (ZEB-248).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 16: Frontend — types, adapter, PollMessage.svelte, chat integration

**Purpose:** Build the user-facing slice — TypeScript types matching the Rust wire format, a thin IPC adapter, a Svelte 5 `PollMessage` component for the chat-embedded poll card, integration with the message rendering pipeline, and Vitest tests.

**Files:**
- Create: `src/lib/types/voting.ts`
- Create: `src/lib/voting-adapter.ts`
- Create: `src/lib/components/PollMessage.svelte`
- Create: `src/lib/components/PollMessage.test.ts`
- Modify: `src/lib/types/index.ts` (re-export)
- Modify: `src/lib/components/MessageList.svelte` (or wherever message dispatch lives) (add poll-message rendering branch)

- [ ] **Step 1: Discover the message-rendering dispatch site**

```bash
grep -rn "MessageKind\|message-kind\|messageKind" src/lib/components/ | head -10
```

Use the result to locate where chat messages are dispatched by kind so the implementer knows where to wire `PollMessage` rendering.

- [ ] **Step 2: Write the TypeScript types**

Create `src/lib/types/voting.ts`:

```typescript
// ZEB-290 Phase 1: TypeScript types matching the Rust voting wire format.
// Auto-converted snake_case → camelCase across the Tauri IPC boundary.

export type PollId = string; // hex-encoded 32 bytes

export type Tier = 1 | 2 | 3;

export type Lifecycle = 'Draft' | 'Open' | 'Closed' | 'Finalized' | 'Archived';

export interface Eligibility {
  minPower: number;
  minVouchingDepth?: number;
  sortitionSize?: number;
}

export interface PollMeta {
  pollId: PollId;
  communityId: string;
  creator: string;
  tier: Tier;
  eligibility: Eligibility;
  lifecycle: Lifecycle;
  createdAt: Hlc;
  opensAt: Hlc;
  closesAt: Hlc;
  extendsAt?: Hlc;
  channelId?: string;
}

export interface Hlc {
  wallMs: number;
  logical: number;
  deviceId: string;
}

export interface TallyExport {
  counts: number[];
  ballotCount: number;
}

export type Tier1Result =
  | { type: 'winners'; winners: number[] }
  | { type: 'noQuorum'; required: number; actual: number }
  | { type: 'noMajority'; requiredPercent: number; actualPercent: number };

export interface PollStateExport {
  meta: PollMeta;
  tally: TallyExport;
  yourBallot?: number[];
}
```

- [ ] **Step 3: Write the adapter**

Create `src/lib/voting-adapter.ts`:

```typescript
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import type { PollId, PollMeta, PollStateExport } from './types/voting';

export async function createTier1Poll(args: {
  communityId: string;
  channelId: string;
  options: string[];
  windowSeconds: number;
  minPower: number;
  minVouchingDepth?: number;
  quorum?: number;
  thresholdPercent?: number;
  multiWinner?: number;
}): Promise<PollId> {
  return invoke('voting_create_tier1_poll', args);
}

export async function castTier1Ballot(pollId: PollId, approvedIndices: number[]): Promise<void> {
  return invoke('voting_cast_tier1_ballot', { pollId, approvedIndices });
}

export async function listActivePolls(communityId: string): Promise<PollMeta[]> {
  return invoke('voting_list_active_polls', { communityId });
}

export async function getPoll(pollId: PollId): Promise<PollStateExport> {
  return invoke('voting_get_poll', { pollId });
}

export interface PollCreatedEvent {
  pollId: PollId;
  channelId: string;
  tier: number;
  question: string;
}

export interface BallotCastEvent {
  pollId: PollId;
  voter: string;
  approvedCount: number;
}

export interface PollClosedEvent {
  pollId: PollId;
  result: unknown; // narrow at consumer side
}

export function onPollCreated(handler: (ev: PollCreatedEvent) => void): Promise<UnlistenFn> {
  return listen<PollCreatedEvent>('voting-poll-created', (e) => handler(e.payload));
}

export function onBallotCast(handler: (ev: BallotCastEvent) => void): Promise<UnlistenFn> {
  return listen<BallotCastEvent>('voting-ballot-cast', (e) => handler(e.payload));
}

export function onPollClosed(handler: (ev: PollClosedEvent) => void): Promise<UnlistenFn> {
  return listen<PollClosedEvent>('voting-poll-closed', (e) => handler(e.payload));
}
```

- [ ] **Step 4: Write PollMessage.svelte**

Create `src/lib/components/PollMessage.svelte`:

```svelte
<script lang="ts">
  import { castTier1Ballot, getPoll, onBallotCast, onPollClosed } from '$lib/voting-adapter';
  import type { PollMeta, PollStateExport, Tier1Result } from '$lib/types/voting';
  import { onMount, onDestroy } from 'svelte';

  // Svelte 5 $props — ZEB-287 R4 critical bug: destructure ALL props
  // being used; silently no-ops otherwise.
  let { meta }: { meta: PollMeta } = $props();

  let state = $state<PollStateExport | null>(null);
  let myApproved = $state<Set<number>>(new Set());
  let busy = $state(false);

  let unsubBallot: (() => void) | null = null;
  let unsubClosed: (() => void) | null = null;

  async function refresh() {
    try {
      state = await getPoll(meta.pollId);
      myApproved = new Set(state.yourBallot ?? []);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error('getPoll failed:', msg);
    }
  }

  async function toggleOption(idx: number) {
    if (busy) return;
    busy = true;
    const next = new Set(myApproved);
    if (next.has(idx)) next.delete(idx);
    else next.add(idx);
    try {
      // Don't allow approve-all (rejected as abstention).
      if (next.size === meta && state && next.size === state.tally.counts.length) {
        return; // UI guard mirroring backend rule
      }
      if (next.size === 0) return; // Empty rejected; pretend toggle didn't happen
      const sorted = [...next].sort((a, b) => a - b);
      await castTier1Ballot(meta.pollId, sorted);
      myApproved = next;
      await refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error('castBallot failed:', msg);
    } finally {
      busy = false;
    }
  }

  onMount(async () => {
    await refresh();
    unsubBallot = await onBallotCast((ev) => {
      if (ev.pollId === meta.pollId) refresh();
    });
    unsubClosed = await onPollClosed((ev) => {
      if (ev.pollId === meta.pollId) refresh();
    });
  });

  onDestroy(() => {
    unsubBallot?.();
    unsubClosed?.();
  });

  // Derived: total ballots, per-option percent for live bars.
  let totalBallots = $derived(state?.tally.ballotCount ?? 0);
  let isClosed = $derived(meta.lifecycle === 'Closed' || meta.lifecycle === 'Finalized' || meta.lifecycle === 'Archived');
</script>

<div class="poll-card" data-poll-id={meta.pollId}>
  <div class="poll-tag">TIER 1 · APPROVAL</div>
  <!-- TODO: implementer wires up option labels from a separate getPollConfig
       IPC, or extends PollStateExport to include options array. -->
  {#if state}
    {#each state.tally.counts as count, idx}
      <button
        class="opt"
        class:selected={myApproved.has(idx)}
        disabled={isClosed || busy}
        onclick={() => toggleOption(idx)}
      >
        <span class="check">{myApproved.has(idx) ? '✓' : ''}</span>
        <span class="label">Option {idx}</span>
        <span class="count">{count}</span>
        <div class="bar" style="width: {totalBallots > 0 ? (count / totalBallots) * 100 : 0}%"></div>
      </button>
    {/each}
  {/if}
  <div class="poll-meta">
    <span>{totalBallots} voted</span>
    <span>{isClosed ? 'closed' : 'open'}</span>
  </div>
</div>

<style>
  .poll-card { padding: 12px; border: 1px solid #3a4555; border-radius: 8px; max-width: 480px; }
  .poll-tag { font-size: 11px; color: #9ec3e6; font-weight: 600; margin-bottom: 8px; }
  .opt { display: flex; align-items: center; gap: 8px; padding: 8px; border: 1px solid #3a4555; border-radius: 4px; background: #1f242c; color: #ccc; cursor: pointer; position: relative; width: 100%; margin-bottom: 4px; }
  .opt.selected { border-color: #4f8ed8; }
  .opt:disabled { cursor: default; opacity: 0.7; }
  .check { width: 16px; }
  .label { flex: 1; text-align: left; }
  .count { font-variant-numeric: tabular-nums; }
  .bar { position: absolute; left: 0; top: 0; bottom: 0; background: rgba(79,142,216,0.15); pointer-events: none; }
  .poll-meta { display: flex; justify-content: space-between; color: #888; font-size: 12px; margin-top: 8px; }
</style>
```

(Implementer: refine the option-label data flow — the cleanest fix is to extend `PollStateExport` on the Rust side to include the option labels, since they're stored in the config.)

- [ ] **Step 5: Write Vitest tests for PollMessage**

Create `src/lib/components/PollMessage.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import PollMessage from './PollMessage.svelte';
import type { PollMeta } from '$lib/types/voting';

// Mock the adapter to avoid real Tauri IPC.
vi.mock('$lib/voting-adapter', () => ({
  castTier1Ballot: vi.fn().mockResolvedValue(undefined),
  getPoll: vi.fn().mockResolvedValue({
    meta: {} as PollMeta,
    tally: { counts: [3, 1, 2], ballotCount: 6 },
    yourBallot: undefined,
  }),
  onBallotCast: vi.fn().mockResolvedValue(() => {}),
  onPollClosed: vi.fn().mockResolvedValue(() => {}),
}));

function fixtureMeta(): PollMeta {
  return {
    pollId: 'abababab' + 'ab'.repeat(28),
    communityId: '11'.repeat(16),
    creator: 'cc'.repeat(16),
    tier: 1,
    eligibility: { minPower: 0 },
    lifecycle: 'Open',
    createdAt: { wallMs: 1000, logical: 0, deviceId: 'd' },
    opensAt: { wallMs: 1000, logical: 0, deviceId: 'd' },
    closesAt: { wallMs: 4600, logical: 0, deviceId: 'd' },
    extendsAt: undefined,
    channelId: 'ee'.repeat(16),
  };
}

describe('PollMessage', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders tally bars', async () => {
    const { findAllByRole } = render(PollMessage, { props: { meta: fixtureMeta() } });
    const buttons = await findAllByRole('button');
    expect(buttons.length).toBeGreaterThan(0);
  });

  it('disables interaction when closed', async () => {
    const closedMeta = { ...fixtureMeta(), lifecycle: 'Closed' as const };
    const { findAllByRole } = render(PollMessage, { props: { meta: closedMeta } });
    const buttons = await findAllByRole('button');
    for (const b of buttons) {
      expect((b as HTMLButtonElement).disabled).toBe(true);
    }
  });
});
```

- [ ] **Step 6: Wire PollMessage into message rendering**

In whichever component dispatches by message kind, add a branch that renders `PollMessage` for poll-kind messages. The implementer chooses the cleanest seam (likely `MessageList.svelte` or wherever a `MessageRenderer` lives).

- [ ] **Step 7: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: both pass.

- [ ] **Step 8: Run full Rust suite + frontend gates as final sanity**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures && cd .. && npx tsc --noEmit && npx vitest run
```

- [ ] **Step 9: Commit**

```bash
git add src/lib/types/voting.ts src/lib/voting-adapter.ts src/lib/components/PollMessage.svelte src/lib/components/PollMessage.test.ts src/lib/types/index.ts src/lib/components/MessageList.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-290): frontend — types + adapter + PollMessage.svelte

TypeScript types matching Rust wire format (snake_case → camelCase
across IPC). voting-adapter wraps the 4 IPCs + 3 event listeners.
PollMessage.svelte is the chat-native embedded poll card —
clickable options, live tally bars updated by ballot/closed events,
voter-selection highlighting, disabled when closed. Vitest tests
mock the adapter and assert render + closed-state disabling.

Per ZEB-287 R4 critical bug: $props() destructures every prop
being used (silently no-ops otherwise). Per Tauri error extraction
memory: catches use `e instanceof Error ? e.message : String(e)`.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 17: Final 5-gate sweep + push + PR

**Purpose:** Run all five CI gates from a clean state, push the branch, and open the PR with markdown-linked Linear refs.

**Files:** none modified beyond commit messages.

- [ ] **Step 1: Confirm clean working tree + branch state**

```bash
git status --short
git branch --show-current
git log --oneline origin/main..HEAD | wc -l
```

Expected: empty status; branch `zeb-290-phase1-voting-core-tier1-approval`; commit count somewhere in the 15-20 range.

- [ ] **Step 2: Run all five CI gates from scratch**

```bash
cd src-tauri \
  && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures \
  && cd .. \
  && npx tsc --noEmit \
  && npx vitest run
```

Expected: all five gates pass with zero warnings, zero failures. Per `feedback_pipe_exit_codes_lie`: if you wrap this in any pipeline, use `set -o pipefail` or `${PIPESTATUS[0]}` to catch failures correctly.

If any gate fails, debug + fix + commit a fixup before pushing.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-290-phase1-voting-core-tier1-approval
```

- [ ] **Step 4: Open the PR with markdown-linked Linear refs**

Per `feedback_linear_pr_auto_close` memory: use `[ZEB-XXX](url)` markdown links throughout body; the auto-close cascade closes every ZEB-NNN referenced in a PR body via `Closes #N`. To ensure ONLY ZEB-290 auto-closes on merge, use ONE `Closes ZEB-290` line at the bottom (NOT `Resolves`; that pattern doesn't trigger Linear's GH integration per the ZEB-241 incident).

```bash
gh pr create --title "ZEB-290 Phase 1: voting_core + Tier 1 Approval voting" --body "$(cat <<'EOF'
## Summary

Phase 1 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella. Ships:

- New `community_voting_core.rs` — shared types, signed envelope per spec §3, eligibility verifier, lifecycle state machine, build helpers
- New `community_voting_log.rs` — per-community signed-event log parallel to `community_channel_log`, Zenoh sync, auto-close on window expiry, 90-day archive sweep
- New `community_voting_approval.rs` — Tier 1 Approval mechanism (HLC-LWW tally, quorum/threshold/multi-winner result variants, R2 reproducibility verifier)
- 4 IPC commands: `voting_create_tier1_poll`, `voting_cast_tier1_ballot`, `voting_list_active_polls`, `voting_get_poll`
- 3 Tauri events: `voting-poll-created`, `voting-ballot-cast`, `voting-poll-closed`
- Chat-native `PollMessage.svelte` with live tally bars, voter-selection highlighting, window countdown
- Wire-format fixtures pinning canonical CBOR for 6 event kinds
- Two-engine integration test confirming tally convergence across out-of-order arrival

Spec: [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](../blob/main/docs/specs/2026-05-16-zeb-289-voting-polling-design.md).

Pattern source: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (parallel per-community log), [ZEB-250](https://linear.app/zeblith/issue/ZEB-250) (CBOR same-length-keys, fixture pinning).

Phase 1 of 7 — subsequent phases ([ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Conviction, [ZEB-292](https://linear.app/zeblith/issue/ZEB-292) Delegation UI, [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) Sortition, [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) Pol.is, [ZEB-295](https://linear.app/zeblith/issue/ZEB-295) D-FROST tally, [ZEB-296](https://linear.app/zeblith/issue/ZEB-296) TRIP kiosk) build on this foundation.

## Test plan

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` all passing (added ~50 new unit tests + 2 multi-engine integration tests + 6 wire-format fixture tests)
- [x] `npx tsc --noEmit` clean
- [x] `npx vitest run` all passing (added PollMessage component tests)
- [ ] Manual smoke: create poll in chat, cast ballot, observe tally bar update, await window expiry, observe auto-close + result

Closes [ZEB-290](https://linear.app/zeblith/issue/ZEB-290).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

After PR creation, the user / autonomous bot-review loop takes over (CodeRabbit, Cursor Bugbot, CodeAnt, Qodo). Greptile is manual-trigger only per `reference_greptile_manual_trigger`. CI is disabled per `feedback_ci_disabled` — bots are NOT CI; wait for their feedback regardless.

- [ ] **Step 5: Report PR URL to user**

Return the PR URL from `gh pr create` so the user can monitor. No commit on this step — it's pure surfacing.

---

## Self-Review Checklist

Run this checklist after writing each task — DON'T skip:

1. **Spec coverage:** every requirement from spec §§1-9 is covered by some task. Specifically:
   - §1 goals → Tasks 1, 4, 8, 9, 10 (verifiable, decentralized, eligibility schema, etc.)
   - §2 architecture → Tasks 1-6 (module structure, lifecycle, dispatcher)
   - §3 wire format → Tasks 2, 3, 14 (envelope + kind enum + fixtures)
   - §4 Tier 1 → Tasks 7, 8, 9, 11, 16 (full mechanism + IPCs + UI)
   - §7 community policy → deferred to Phase 2 (Phase 1 uses hardcoded defaults)
   - §8 verify rules → Tasks 4, 5, 10 (V/B/L/R rules implemented via check_eligibility / next_lifecycle / verify_poll_result_reproducible)
   - §9 materialize rules → Task 6 (apply) + Task 8 (tally)
   - §10 failure modes — partition, kicked, snapshot timing → Tasks 8, 10 (tally is HLC-LWW + snapshot at PollCreate.hlc)
   - §11 backwards compatibility → Task 11 (synthesized defaults for legacy communities), Task 14 (fixture pinning catches schema bumps)
2. **No placeholders:** scan the plan for `TBD`, `TODO`, `FIXME`, `???`, `add error handling`. The only `todo!()` macros are explicit placeholders in IPC bodies / archive tests where the implementer fills in per the surrounding context.
3. **Type consistency:** PollId / Tier / Eligibility / PollMeta / Lifecycle / PollEventKindCode / SignedVotingEvent / Tier1PollConfig / Tier1Ballot / Tier1Result / Tier1TallyState / Tier1PollResultPayload — each appears with the same fields and rename codes in every task that references them. Verified by grep across the plan file.

---

## Execution

Plan complete and saved to `docs/plans/2026-05-16-zeb-290-phase1-voting-core-tier1-approval-plan.md`. Two execution options:

1. **Subagent-Driven (recommended)** — controller dispatches a fresh subagent per task, two-stage review (spec compliance + code quality) between tasks, fast iteration.

2. **Inline Execution** — execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints for review.

Per the calling agent's stated plan: subagent-driven-development is the chosen path. After Task 17 completes successfully (PR open), control returns to the calling agent for the autonomous bot-review monitoring loop (CodeRabbit, Cursor Bugbot, CodeAnt, Qodo — NOT Greptile, NOT CI).
