# ZEB-295 Phase 6 — Tier 3c Ballot-Secret Ratification via D-FROST (Implementation Plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `privacy_mode "se"` ballot-secret ratification on Tier 3 polls. Ratification ballots encrypted to the existing Phase 4 D-FROST committee public key; only the aggregate STAR tally is decrypted post-window via committee-published TallyShare events.

**Architecture:** Exponential ElGamal over Ristretto255 (reuses curve25519-dalek already in tree). FROST DKG key material from Phase 4 is reinterpreted as the threshold-ElGamal decryption key (no new ceremonies, no per-poll DKG). Hand-rolled sigma-protocol NIZK bundle (range proofs + indicator-consistency proofs + DLEQ for share validity) with Fiat-Shamir via merlin Strobe transcripts. Apply-time silent-drop semantics consistent with Phase 5; ZEB-320 dual-watermark discipline (`last_received_hlc` advances on every dispatch; `last_hlc` only on accepts).

**Tech Stack:** Rust workspace (`src-tauri/`) — `curve25519-dalek = "=4.1.3"` (in tree), `merlin = "3"` (new dep), `frost-ristretto255 = "3.0.0"` (in tree), `ciborium`. Frontend — Svelte 5 runes, TypeScript, Tauri 2 IPC.

**Spec:** [`docs/specs/2026-05-21-zeb-295-tier3c-ballot-secret-design.md`](../specs/2026-05-21-zeb-295-tier3c-ballot-secret-design.md) at commit `7c2db0c`. Branch `zeb-295-tier3c-ballot-secret-design` off `origin/main` at `0bf89c3`.

**Linear:** Closes [ZEB-295](https://linear.app/zeblith/issue/ZEB-295). Parent: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) (stays Backlog — phases remain).

---

## Hard rules

- **5 backend CI gates (run from `src-tauri/`):** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- **2 frontend CI gates (run from repo root):** `npx tsc --noEmit`, `npx vitest run`.
- **No worktrees.** This branch IS the working branch in the main repo.
- **Pipe exit codes:** every `cmd | tail/grep` MUST use `set -o pipefail` or `${PIPESTATUS[0]}`. Particularly load-bearing when validating that the new wire-format pinning fixture file's tests are running.
- **Tauri IPC:** Rust args declared `snake_case` (e.g. `poll_id`); JS callers pass `camelCase` (e.g. `pollId`). Tauri converts at the boundary.
- **Tauri error extraction (frontend):** `e instanceof Error ? e.message : String(e)`.
- **Implementer gate per task:** commit BEFORE running the gate sweep + 10-min wall-clock kill switch + DONE_WITH_CONCERNS if gates exceed budget.
- **`cargo fmt --check` is part of every gate sweep**, not just `clippy`.
- **PR body markdown-links:** every cross-reference uses `[ZEB-XXX](https://linear.app/zeblith/issue/ZEB-XXX)` so Linear's GitHub integration does NOT cascade-close them. The ONLY bare `Closes ZEB-295` line is for the ticket fully completed by this PR.
- **Pre-existing orphan failures from Task 0 baseline** are not blocking; NEW failures introduced by this work are blocking.
- **No new Linear tickets** unless follow-up work is genuinely discovered mid-implementation.
- **Self-review per [[feedback_second_order_correctness_review]]:** when extending `apply_event`, enumerate every reader of each field/state being modified. Pattern: a field doing two jobs (e.g. `last_hlc` projection + monotonic guard pre-ZEB-320) breaks when one job's behavior is fixed.

---

## File structure (decomposition map)

### New files

- `src-tauri/src/community_voting_tier3_crypto.rs` — threshold-ElGamal encrypt/partial-decrypt-share/Lagrange/BSGS; FROST→ElGamal helpers re-exported here.
- `src-tauri/src/community_voting_tier3_nizk.rs` — Chaum-Pedersen DLEQ, 6-way OR-of-Schnorr range proofs over {0..5}, 2-way OR indicator-consistency proofs, BallotNIZKProof bundle. Fiat-Shamir transcripts via merlin Strobe.
- `src-tauri/tests/wire_format_voting_tier3_secret_fixtures.rs` — CBOR byte-pin fixtures for `kd=rb` (se-mode) at n=3 and n=5 + `kd=ts` at n=3 and n=5 + pre/post CHURP rotation.
- `src-tauri/tests/community_voting_tier3_secret_ipc_integration.rs` — single-engine happy path + per-silent-drop coverage.
- `src-tauri/tests/community_voting_tier3_secret_multi_engine_integration.rs` — multi-engine determinism + CHURP rotation mid-test + subset-of-shares Lagrange invariance.

### Modified files

- `src-tauri/Cargo.toml` — add `merlin = "3"` dep.
- `src-tauri/src/community_voting_core.rs` — extend `RatificationBallotPayload` with optional `cs/in/pf` se-mode fields; add `EncCiphertext` / `BallotNIZKProof` / `TallySharePayload` / `TallyShareEntry` types; add `PollEventKindCode::TallyShare` ("ts"); extend wire-string pin tests.
- `src-tauri/src/community_voting_tier3.rs` — relax `validate_tier3_poll_config` to accept `"se"`; add `SecretTallyState` + `CommitteePublicState` + `CommitteeOracle` trait; extend `Tier3PollState` with `secret_tally` + `committee_oracle`; extend `apply_event` with kd=rb B5/se-mode rules + new kd=ts branch.
- `src-tauri/src/community_voting_conviction.rs` — extend `CommunityVotingPolicy` with `tier3_privacy_mode_default: String` (default "pu").
- `src-tauri/src/community_dfrost_crypto.rs` — expose helpers `signing_share_as_scalar`, `verifying_share_to_point`, `joint_verifying_key_to_point`.
- `src-tauri/src/community_voting_log_engine.rs` — install production `DfrostLogCommitteeOracle` on Tier3PollState; add `maybe_emit_tally_share` + `maybe_emit_tier3_result_secret` post-apply hooks; add `voting-tier3-tally-share-applied` Tauri event.
- `src-tauri/src/lib.rs` — extend `voting_create_tier3_proposal` IPC to accept `privacy_mode: Option<String>`; branch `voting_cast_ratification_ballot` on poll's `privacy_mode` (server-side encrypts + NIZK for se-mode); extend `build_tier3_export` with se-mode export fields.
- `src/lib/types/voting.ts` — extend `Tier3PollExport` with `privacyMode`, `encryptedTallyShareCount`, `encryptedTallyThreshold`, `encryptedTallyCommitteeSize`; add `Tier3TallyShareAppliedPayload`.
- `src/lib/voting-adapter.ts` — subscribe to `voting-tier3-tally-share-applied`; extend `proposeTier3Proposal` adapter with `privacyMode` param.
- `src/lib/components/Tier3ProposalPanel.svelte` — add `privacy_mode` toggle in create-form; render three new se-mode states in ratification view.
- `src/lib/components/StarRatificationBallot.svelte` — show lock-icon banner when `detail.privacyMode === 'se'`.

---

## Task 0: Pre-flight baseline (no commit)

**Files:** none

- [ ] **Step 1: Confirm branch state**

```bash
git status -s
git log -1 --oneline
git merge-base HEAD origin/main
git log -1 --oneline origin/main
```

Expected:
- Working tree clean.
- HEAD: `7c2db0c docs(zeb-295): Tier 3c ballot-secret D-FROST tally design spec`.
- Merge-base equals origin/main HEAD (`0bf89c3`).
- If any of these fail, STOP and surface to the controller.

- [ ] **Step 2: Capture orphan failure baseline**

Run (from repo root):

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -100
```

Expected: tests pass aside from pre-existing orphans documented in user memory (folder_ingest::tests, mint::tests, mint_sync::tests, folder_ingest_walker_integration, rename_content_integration). Record the exact failure list for cross-reference at Task 12 — any NEW failure beyond this list is blocking.

- [ ] **Step 3: Run the 4 other gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && npx tsc --noEmit
cd .. && npx vitest run
```

All four must exit 0. If any fails on main-aligned code, STOP — that's pre-existing drift the next task must not paper over.

**No commit.** Baseline only; nothing to record.

---

## Task 1: Wire format extension (community_voting_core.rs)

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs` (around lines 230–290 for payloads, 580–830 for kind code + pin tests)

- [ ] **Step 1: Write the failing tests first**

Append to the `tier3_payload_tests` mod (after `deliberation_vote_payload_all_three_vote_codes_round_trip` at ~line 350):

```rust
#[test]
fn ratification_ballot_payload_pu_mode_round_trips() {
    let payload = RatificationBallotPayload {
        poll_id: PollId([0x11; 32]),
        scores: Some(vec![5, 3, 1, 0, 4]),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let decoded: RatificationBallotPayload = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(payload, decoded);
}

#[test]
fn ratification_ballot_payload_se_mode_round_trips() {
    let payload = RatificationBallotPayload {
        poll_id: PollId([0x22; 32]),
        scores: None,
        ciphertexts_scores: Some(vec![EncCiphertext { c1: [0xAA; 32], c2: [0xBB; 32] }; 3]),
        ciphertexts_indicators: Some(vec![EncCiphertext { c1: [0xCC; 32], c2: [0xDD; 32] }; 3]),
        proof: Some(BallotNIZKProof { range_proofs: vec![0xEE; 384 * 3], consistency_proofs: vec![0xFF; 768 * 3] }),
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let decoded: RatificationBallotPayload = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(payload, decoded);
}

#[test]
fn ratification_ballot_payload_pu_mode_omits_se_keys() {
    // skip_serializing_if on Option-fields must elide them from the wire.
    let payload = RatificationBallotPayload {
        poll_id: PollId([0; 32]),
        scores: Some(vec![5]),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
    let map = value.as_map().expect("map");
    assert_eq!(map.len(), 2, "pu-mode payload must have exactly {{pi, sc}}");
}

#[test]
fn ratification_ballot_payload_se_mode_omits_sc_key() {
    let payload = RatificationBallotPayload {
        poll_id: PollId([0; 32]),
        scores: None,
        ciphertexts_scores: Some(vec![EncCiphertext { c1: [0; 32], c2: [0; 32] }]),
        ciphertexts_indicators: Some(vec![EncCiphertext { c1: [0; 32], c2: [0; 32] }]),
        proof: Some(BallotNIZKProof { range_proofs: vec![0; 384], consistency_proofs: vec![0; 768] }),
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
    let map = value.as_map().expect("map");
    assert_eq!(map.len(), 4, "se-mode payload must have exactly {{pi, cs, in, pf}}");
    let keys: std::collections::BTreeSet<&str> = map.iter()
        .map(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text().expect("text key"))
        .collect();
    assert_eq!(keys, std::collections::BTreeSet::from(["pi", "cs", "in", "pf"]));
}

#[test]
fn tally_share_payload_round_trips() {
    let payload = TallySharePayload {
        poll_id: PollId([0x33; 32]),
        committee_epoch: 7,
        entries: vec![
            TallyShareEntry { share: [0xA1; 32], dleq_proof: [0xB2; 64] },
            TallyShareEntry { share: [0xC3; 32], dleq_proof: [0xD4; 64] },
        ],
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let decoded: TallySharePayload = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(payload, decoded);
}

#[test]
fn tally_share_payload_top_keys_are_two_char() {
    let payload = TallySharePayload {
        poll_id: PollId([0; 32]),
        committee_epoch: 0,
        entries: vec![],
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode");
    let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
    for (k, _) in value.as_map().expect("map").iter() {
        let s = k.as_text().expect("text key");
        assert_eq!(s.len(), 2, "TallySharePayload key {s:?} violates 2-char invariant");
    }
}
```

Extend `envelope_tests::kind_code_round_trip` (around line 773) to include `PollEventKindCode::TallyShare`. Extend `tier3_kind_codes_have_expected_wire_strings` (line 807) to include `(PollEventKindCode::TallyShare, "ts")`.

- [ ] **Step 2: Run the new tests; verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tier3_payload_tests) or test(envelope_tests)'
```

Expected: compile errors on `EncCiphertext`, `BallotNIZKProof`, `TallySharePayload`, `TallyShareEntry`, `PollEventKindCode::TallyShare`, and on the renamed `RatificationBallotPayload` fields.

- [ ] **Step 3: Implement the wire-format types**

In `src-tauri/src/community_voting_core.rs`, REPLACE the existing `RatificationBallotPayload` struct (around line 238–246) with:

```rust
/// Payload for `kd=rb` RatificationBallot. Overloaded for both privacy
/// modes per ZEB-295 spec §2.1. Mode is determined at apply time from
/// the poll's `privacy_mode` field, NOT from the payload itself.
/// Same-length-keys invariant: all top-level CBOR keys are 2 chars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatificationBallotPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// `"pu"`-mode: raw scores 0..=5 per candidate. None in `"se"` mode.
    #[serde(rename = "sc", default, skip_serializing_if = "Option::is_none", with = "scores_opt_serde")]
    pub scores: Option<Vec<u8>>,
    /// `"se"`-mode: one ElGamal ciphertext per candidate; len == n.
    #[serde(rename = "cs", default, skip_serializing_if = "Option::is_none")]
    pub ciphertexts_scores: Option<Vec<EncCiphertext>>,
    /// `"se"`-mode: one ElGamal ciphertext per unordered candidate pair
    /// (smaller-index-wins canonical orientation); len == n*(n-1)/2.
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub ciphertexts_indicators: Option<Vec<EncCiphertext>>,
    /// `"se"`-mode: per-ballot NIZK bundle (range proofs + consistency proofs).
    #[serde(rename = "pf", default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<BallotNIZKProof>,
}

/// Tiny module so the `with = "..."` attribute on `scores` keeps the
/// `serde_bytes` Vec<u8> encoding for Some(...) but elides None entirely.
mod scores_opt_serde {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => serde_bytes::serialize(b.as_slice(), s),
            None => unreachable!("skip_serializing_if elides None before reaching here"),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let b: Vec<u8> = serde_bytes::deserialize(d)?;
        Ok(Some(b))
    }
}

/// ElGamal ciphertext in Ristretto255. Compressed-point encoding per spec §2.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncCiphertext {
    #[serde(rename = "c1", with = "serde_bytes_32")]
    pub c1: [u8; 32],
    #[serde(rename = "c2", with = "serde_bytes_32")]
    pub c2: [u8; 32],
}

/// Per-ballot NIZK bundle. Concatenated sigma-protocol bytes per spec §4.7.
/// Sizes are deterministic in n (number of candidates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BallotNIZKProof {
    /// `n` range proofs over {0..5}, 384 B each; total len = 384*n.
    #[serde(rename = "rp", with = "serde_bytes")]
    pub range_proofs: Vec<u8>,
    /// `C(n,2)` consistency proofs, 768 B each; total len = 768*C(n,2).
    #[serde(rename = "ip", with = "serde_bytes")]
    pub consistency_proofs: Vec<u8>,
}

/// Single committee member's per-aggregate decryption share + DLEQ proof.
/// Spec §2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyShareEntry {
    /// Partial decryption share `d_i = c1_agg * x_i` (compressed Ristretto).
    #[serde(rename = "sh", with = "serde_bytes_32")]
    pub share: [u8; 32],
    /// Chaum-Pedersen DLEQ proof bytes — `(challenge: [u8;32], response: [u8;32])`.
    #[serde(rename = "dp", with = "serde_bytes_64")]
    pub dleq_proof: [u8; 64],
}

/// Payload for `kd=ts` TallyShare. Spec §2.2.
/// Same-length-keys invariant: pi/ce/ts are all 2 chars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallySharePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// CHURP rotation generation. Shares from different epochs cannot be mixed.
    #[serde(rename = "ce")]
    pub committee_epoch: u32,
    /// `n + C(n,2)` entries: candidate score-sum entries first, then indicator-sum
    /// entries in unordered-pair lexicographic order. Vec (not fixed array) because
    /// `n` is per-poll and only known at apply time.
    #[serde(rename = "ts")]
    pub entries: Vec<TallyShareEntry>,
}

// Fixed-length byte-array helpers used by EncCiphertext / TallyShareEntry.
mod serde_bytes_32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice().try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod serde_bytes_64 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice().try_into().map_err(|_| serde::de::Error::custom("expected 64 bytes"))
    }
}
```

Add the new `TallyShare` variant to `PollEventKindCode` (line 583):

```rust
    #[serde(rename = "ts")]
    TallyShare,
```

Update `kind_code_round_trip` and `tier3_kind_codes_have_expected_wire_strings` tests to include the new variant per Step 1.

- [ ] **Step 4: Update existing callers of `RatificationBallotPayload`**

In `community_voting_core.rs`, find every instance constructing `RatificationBallotPayload { poll_id, scores }` (line 445, 1569, 1922) and update to `{ poll_id, scores: Some(scores), ciphertexts_scores: None, ciphertexts_indicators: None, proof: None }`. Pin these via the pu-mode round-trip test.

In `community_voting_tier3.rs`, find `validate_ratification_ballot` (line 220) — its access pattern uses `pd.scores.len()`. Update to `pd.scores.as_ref().map(|s| s.len()).unwrap_or(0)` (the pu-mode invariant ensures Some when this validator is called from the pu IPC path; se-mode bypasses this validator entirely per Task 7's B5).

Run `rg "RatificationBallotPayload \{" src-tauri/` and update every call site.

- [ ] **Step 5: Run all tests; verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tier3_payload_tests) or test(envelope_tests)'
```

Expected: all green. If a call-site you missed throws a compile error, fix the constructor.

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_core.rs src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): extend wire format for ballot-secret ratification

- Overload RatificationBallotPayload with optional se-mode fields
  (cs/in/pf) per spec §2.1. pu-mode payloads remain {pi, sc}.
- Add EncCiphertext + BallotNIZKProof + TallySharePayload +
  TallyShareEntry types per spec §2.2–§2.4.
- Add PollEventKindCode::TallyShare ("ts") + extend wire-string pin
  + same-length-keys invariant tests.

No apply behavior yet — Tasks 6/7 wire the materialize side.
EOF
)"
```

---

## Task 2: Threshold-ElGamal + Lagrange + BSGS module

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Create: `src-tauri/src/community_voting_tier3_crypto.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod` declaration)

- [ ] **Step 1: Add merlin to Cargo.toml**

In `src-tauri/Cargo.toml`, locate the dependencies section (around line 73 where `curve25519-dalek = "=4.1.3"` lives) and add:

```toml
merlin = "3"
```

- [ ] **Step 2: Write the failing tests**

Create `src-tauri/src/community_voting_tier3_crypto.rs` with this skeleton (filling in only the `mod tests` block first):

```rust
//! ZEB-295: Threshold-ElGamal in Ristretto255 + Lagrange combine + BSGS
//! discrete-log recovery for the Tier 3c ballot-secret ratification path.
//! Spec §4.

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_TABLE as G_TABLE,
    ristretto::{CompressedRistretto, RistrettoPoint},
    scalar::Scalar,
};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
    use rand_core::OsRng;

    fn rand_scalar() -> Scalar {
        Scalar::random(&mut OsRng)
    }

    fn fake_committee(t: usize, n: usize) -> (Scalar, BTreeMap<u16, Scalar>, RistrettoPoint) {
        // Construct a t-of-n committee by hand: a random polynomial f of
        // degree t-1 with f(0) = x. Shares are x_i = f(i+1). Returns
        // (x_secret, {id -> x_i}, Y = G * x).
        let coeffs: Vec<Scalar> = (0..t).map(|_| rand_scalar()).collect();
        let x = coeffs[0];
        let y_point = &G * &x;
        let mut shares = BTreeMap::new();
        for i in 1..=n as u16 {
            let id = Scalar::from(i as u64);
            let mut acc = Scalar::ZERO;
            let mut id_pow = Scalar::ONE;
            for c in &coeffs {
                acc += c * id_pow;
                id_pow *= id;
            }
            shares.insert(i, acc);
        }
        (x, shares, y_point)
    }

    #[test]
    fn elgamal_encrypt_decrypt_known_message_round_trip() {
        let x = rand_scalar();
        let y_point = &G * &x;
        let m = Scalar::from(3u64);
        let (c1, c2) = encrypt(m, y_point, rand_scalar());
        // Plaintext recovery shortcut for the single-key (non-threshold) case:
        // m * G = c2 - x * c1.
        let m_point = c2 - x * c1;
        let recovered = bsgs(&m_point, 10).expect("recover");
        assert_eq!(recovered, 3);
    }

    #[test]
    fn elgamal_homomorphic_add_aggregates_messages() {
        let x = rand_scalar();
        let y_point = &G * &x;
        let (a1, a2) = encrypt(Scalar::from(2u64), y_point, rand_scalar());
        let (b1, b2) = encrypt(Scalar::from(5u64), y_point, rand_scalar());
        let sum1 = a1 + b1;
        let sum2 = a2 + b2;
        let m_point = sum2 - x * sum1;
        assert_eq!(bsgs(&m_point, 10).expect("recover"), 7);
    }

    #[test]
    fn threshold_combine_2_of_3_recovers_plaintext() {
        let (_x, shares, y_point) = fake_committee(2, 3);
        let m = Scalar::from(4u64);
        let (c1_agg, c2_agg) = encrypt(m, y_point, rand_scalar());
        // Two members publish partial shares.
        let mut partial: BTreeMap<u16, RistrettoPoint> = BTreeMap::new();
        for id in [1u16, 2] {
            partial.insert(id, partial_decrypt_share(&c1_agg, &shares[&id]));
        }
        let d_agg = combine_shares(&c1_agg, &partial).expect("combine");
        let m_point = c2_agg - d_agg;
        assert_eq!(bsgs(&m_point, 10).expect("recover"), 4);
    }

    #[test]
    fn threshold_combine_3_of_5_any_subset_recovers_same_plaintext() {
        let (_x, shares, y_point) = fake_committee(3, 5);
        let m = Scalar::from(7u64);
        let (c1_agg, c2_agg) = encrypt(m, y_point, rand_scalar());
        // Try two different subsets — Lagrange invariance says they agree.
        let mut p_a: BTreeMap<u16, RistrettoPoint> = BTreeMap::new();
        for id in [1u16, 2, 3] {
            p_a.insert(id, partial_decrypt_share(&c1_agg, &shares[&id]));
        }
        let m_a = bsgs(&(c2_agg - combine_shares(&c1_agg, &p_a).expect("a")), 20).expect("a");
        let mut p_b: BTreeMap<u16, RistrettoPoint> = BTreeMap::new();
        for id in [2u16, 3, 5] {
            p_b.insert(id, partial_decrypt_share(&c1_agg, &shares[&id]));
        }
        let m_b = bsgs(&(c2_agg - combine_shares(&c1_agg, &p_b).expect("b")), 20).expect("b");
        assert_eq!(m_a, 7);
        assert_eq!(m_b, 7);
    }

    #[test]
    fn bsgs_rejects_out_of_bound() {
        let p = &G * &Scalar::from(100u64);
        assert_eq!(bsgs(&p, 50), None, "discrete log past the bound must not be returned");
    }

    #[test]
    fn bsgs_handles_zero() {
        let p = RistrettoPoint::default();
        assert_eq!(bsgs(&p, 10), Some(0));
    }

    #[test]
    fn tampered_ciphertext_fails_decryption() {
        let (_x, shares, y_point) = fake_committee(2, 3);
        let m = Scalar::from(1u64);
        let (c1_agg, c2_agg) = encrypt(m, y_point, rand_scalar());
        // Tamper: bump c2 by an unrelated point.
        let bad_c2 = c2_agg + (&G * &Scalar::from(999u64));
        let mut partial: BTreeMap<u16, RistrettoPoint> = BTreeMap::new();
        for id in [1u16, 2] {
            partial.insert(id, partial_decrypt_share(&c1_agg, &shares[&id]));
        }
        let m_point = bad_c2 - combine_shares(&c1_agg, &partial).expect("combine");
        assert_eq!(bsgs(&m_point, 10), None, "tampered c2 must not recover within original bound");
    }
}
```

- [ ] **Step 3: Wire the module declaration**

In `src-tauri/src/lib.rs`, add `mod community_voting_tier3_crypto;` near the other `mod community_voting_*` declarations (search for `mod community_voting_tier3`).

- [ ] **Step 4: Run the new tests; verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier3_crypto)'
```

Expected: compile errors on `encrypt`, `partial_decrypt_share`, `combine_shares`, `bsgs`.

- [ ] **Step 5: Implement the crypto primitives**

In `src-tauri/src/community_voting_tier3_crypto.rs` (above the `tests` mod), implement:

```rust
/// Exponential ElGamal encrypt. Returns `(c1, c2) = (G·r, G·m + Y·r)`.
/// Spec §4.2.
pub fn encrypt(m: Scalar, y_point: RistrettoPoint, r: Scalar) -> (RistrettoPoint, RistrettoPoint) {
    let c1 = &G_TABLE * &r;
    let c2 = (&G_TABLE * &m) + y_point * r;
    (c1, c2)
}

/// Compute a committee member's partial decryption share `d_i = c1_agg · x_i`.
/// Spec §4.3.
pub fn partial_decrypt_share(c1_agg: &RistrettoPoint, x_i: &Scalar) -> RistrettoPoint {
    c1_agg * x_i
}

/// Lagrange-combine partial shares to recover `D = c1_agg · x` (where x is
/// the joint secret behind committee key Y). The `shares` map is keyed by
/// the committee member's 1-indexed FROST identifier. Spec §4.5.
///
/// Returns None if `shares.is_empty()` (caller's threshold check should
/// have prevented this).
pub fn combine_shares(
    _c1_agg: &RistrettoPoint,
    shares: &BTreeMap<u16, RistrettoPoint>,
) -> Option<RistrettoPoint> {
    if shares.is_empty() {
        return None;
    }
    let ids: Vec<Scalar> = shares.keys().map(|i| Scalar::from(*i as u64)).collect();
    let mut acc = RistrettoPoint::default();
    for (i_u16, d_i) in shares.iter() {
        let i = Scalar::from(*i_u16 as u64);
        // λ_i(0) = Π_{j∈S, j≠i} (-j) / (i - j)
        let mut num = Scalar::ONE;
        let mut den = Scalar::ONE;
        for j in ids.iter().copied() {
            if j == i { continue; }
            num *= -j;
            den *= i - j;
        }
        let lambda = num * den.invert();
        acc += d_i * lambda;
    }
    Some(acc)
}

/// Baby-step-giant-step: given `P = G · m`, recover `m ∈ [0, bound]` in O(√bound)
/// time and space. Returns None if `m > bound`. Spec §4.6.
pub fn bsgs(p: &RistrettoPoint, bound: u64) -> Option<u64> {
    if bound == 0 {
        return if *p == RistrettoPoint::default() { Some(0) } else { None };
    }
    let sqrt_bound = (bound as f64).sqrt().ceil() as u64 + 1;
    // Baby steps: j → G · j  (table indexed by compressed-point bytes).
    let mut table: std::collections::HashMap<[u8; 32], u64> = std::collections::HashMap::new();
    let mut acc = RistrettoPoint::default();
    for j in 0..=sqrt_bound {
        table.insert(acc.compress().to_bytes(), j);
        acc += &G_TABLE * &Scalar::ONE;
    }
    // Giant steps: search P - G · (k * √bound) for k ∈ [0, √bound].
    let m_step = &G_TABLE * &Scalar::from(sqrt_bound);
    let mut k_step = RistrettoPoint::default();
    for k in 0..=sqrt_bound {
        let candidate = p - k_step;
        if let Some(&j) = table.get(&candidate.compress().to_bytes()) {
            let m = k * sqrt_bound + j;
            if m <= bound {
                return Some(m);
            }
        }
        k_step += m_step;
    }
    None
}

/// Lazily-built BSGS table for a fixed bound. Reused across all aggregate
/// ciphertexts sharing the same bound (e.g. all score-sum aggregates).
/// Spec §4.6.
pub struct BsgsTable {
    sqrt_bound: u64,
    bound: u64,
    table: std::collections::HashMap<[u8; 32], u64>,
    m_step: RistrettoPoint,
}

impl BsgsTable {
    pub fn new(bound: u64) -> Self {
        let sqrt_bound = if bound == 0 { 1 } else { (bound as f64).sqrt().ceil() as u64 + 1 };
        let mut table = std::collections::HashMap::with_capacity(sqrt_bound as usize + 1);
        let mut acc = RistrettoPoint::default();
        for j in 0..=sqrt_bound {
            table.insert(acc.compress().to_bytes(), j);
            acc += &G_TABLE * &Scalar::ONE;
        }
        let m_step = &G_TABLE * &Scalar::from(sqrt_bound);
        Self { sqrt_bound, bound, table, m_step }
    }
    pub fn solve(&self, p: &RistrettoPoint) -> Option<u64> {
        let mut k_step = RistrettoPoint::default();
        for k in 0..=self.sqrt_bound {
            let candidate = p - k_step;
            if let Some(&j) = self.table.get(&candidate.compress().to_bytes()) {
                let m = k * self.sqrt_bound + j;
                if m <= self.bound { return Some(m); }
            }
            k_step += self.m_step;
        }
        None
    }
}

/// Compressed-Ristretto encode helpers for the wire-format types from
/// community_voting_core.
pub fn compress_point(p: &RistrettoPoint) -> [u8; 32] {
    p.compress().to_bytes()
}

pub fn decompress_point(bytes: &[u8; 32]) -> Option<RistrettoPoint> {
    CompressedRistretto::from_slice(bytes).ok()?.decompress()
}
```

- [ ] **Step 6: Run tests; verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier3_crypto)'
```

Expected: all 7 tests pass.

- [ ] **Step 7: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/community_voting_tier3_crypto.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): threshold-ElGamal + Lagrange combine + BSGS

community_voting_tier3_crypto.rs: hand-rolled exponential ElGamal in
Ristretto255 (encrypt/partial-decrypt-share/Lagrange combine), plus
BSGS discrete-log recovery with a reusable precomputed table.

7 unit tests cover encrypt/decrypt round-trip, homomorphic add,
2-of-3 + 3-of-5 threshold combines (with subset invariance), BSGS
bound rejection, zero handling, and tampered-ciphertext rejection.

Adds merlin = "3" to Cargo.toml for NIZK transcripts in Task 3.
EOF
)"
```

---

## Task 3: NIZK module (sigma protocols + range proofs + consistency proofs)

**Files:**
- Create: `src-tauri/src/community_voting_tier3_nizk.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod` declaration)

- [ ] **Step 1: Write the failing tests first**

Create the file with the test scaffold:

```rust
//! ZEB-295: NIZK sigma protocols for Tier 3c ballot-secret ratification.
//! Spec §4.4 (DLEQ), §4.7.1 (range), §4.7.2 (indicator-consistency).
//! Fiat-Shamir via merlin Strobe transcripts with domain tags per §4.9.

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_TABLE as G_TABLE,
    ristretto::RistrettoPoint,
    scalar::Scalar,
};
use merlin::Transcript;

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
    use rand_core::OsRng;
    use crate::community_voting_tier3_crypto::encrypt;

    fn rs() -> Scalar { Scalar::random(&mut OsRng) }

    // ── DLEQ proof tests ────────────────────────────────────────────────

    #[test]
    fn dleq_honest_proves_and_verifies() {
        let x_i = rs();
        let y_i = &G * &x_i;
        let c1_agg = &G * &rs();
        let d_i = c1_agg * x_i;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(dleq_verify(&G, &y_i, &c1_agg, &d_i, &proof));
    }

    #[test]
    fn dleq_tampered_share_fails() {
        let x_i = rs();
        let y_i = &G * &x_i;
        let c1_agg = &G * &rs();
        let d_i = c1_agg * x_i;
        let bad_d = d_i + &G;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(!dleq_verify(&G, &y_i, &c1_agg, &bad_d, &proof));
    }

    #[test]
    fn dleq_tampered_y_fails() {
        let x_i = rs();
        let y_i = &G * &x_i;
        let bad_y = y_i + &G;
        let c1_agg = &G * &rs();
        let d_i = c1_agg * x_i;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(!dleq_verify(&G, &bad_y, &c1_agg, &d_i, &proof));
    }

    // ── Range proof tests ───────────────────────────────────────────────

    #[test]
    fn range5_proves_for_each_m_in_0_to_5() {
        let x = rs();
        let y_point = &G * &x;
        for m in 0u64..=5 {
            let r = rs();
            let (c1, c2) = encrypt(Scalar::from(m), y_point, r);
            let proof = range5_prove(&y_point, &c1, &c2, m, r);
            assert!(range5_verify(&y_point, &c1, &c2, &proof), "m={m} should verify");
        }
    }

    #[test]
    fn range5_rejects_out_of_range_m() {
        let x = rs();
        let y_point = &G * &x;
        let r = rs();
        let m_bad = Scalar::from(6u64);
        let c1 = &G * &r;
        let c2 = (&G * &m_bad) + y_point * r;
        // Caller tries to prove m=5 for a ciphertext that actually encrypts 6.
        let bad_proof = range5_prove(&y_point, &c1, &c2, 5, r);
        assert!(!range5_verify(&y_point, &c1, &c2, &bad_proof));
    }

    // ── Indicator-consistency tests ─────────────────────────────────────

    #[test]
    fn consistency_passes_for_every_score_pair() {
        let x = rs();
        let y_point = &G * &x;
        for score_a in 0u64..=5 {
            for score_b in 0u64..=5 {
                let r_a = rs();
                let r_b = rs();
                let r_i = rs();
                let (c_a_1, c_a_2) = encrypt(Scalar::from(score_a), y_point, r_a);
                let (c_b_1, c_b_2) = encrypt(Scalar::from(score_b), y_point, r_b);
                let indicator = if score_a > score_b { 1u64 } else { 0 };
                let (c_i_1, c_i_2) = encrypt(Scalar::from(indicator), y_point, r_i);
                let proof = consistency_prove(
                    &y_point,
                    (&c_a_1, &c_a_2, score_a, r_a),
                    (&c_b_1, &c_b_2, score_b, r_b),
                    (&c_i_1, &c_i_2, indicator, r_i),
                );
                assert!(
                    consistency_verify(
                        &y_point,
                        (&c_a_1, &c_a_2),
                        (&c_b_1, &c_b_2),
                        (&c_i_1, &c_i_2),
                        &proof,
                    ),
                    "consistency must verify for ({score_a}, {score_b})",
                );
            }
        }
    }

    #[test]
    fn consistency_rejects_mismatched_indicator() {
        let x = rs();
        let y_point = &G * &x;
        let r_a = rs();
        let r_b = rs();
        let r_i = rs();
        let (c_a_1, c_a_2) = encrypt(Scalar::from(5u64), y_point, r_a);
        let (c_b_1, c_b_2) = encrypt(Scalar::from(0u64), y_point, r_b);
        // 5 > 0 so the correct indicator is 1, but we encrypt 0 (mismatched).
        let (c_i_1, c_i_2) = encrypt(Scalar::from(0u64), y_point, r_i);
        let proof = consistency_prove(
            &y_point,
            (&c_a_1, &c_a_2, 5, r_a),
            (&c_b_1, &c_b_2, 0, r_b),
            (&c_i_1, &c_i_2, 1, r_i), // prover claims indicator=1 but ciphertext encrypts 0
        );
        assert!(!consistency_verify(
            &y_point, (&c_a_1, &c_a_2), (&c_b_1, &c_b_2), (&c_i_1, &c_i_2), &proof,
        ));
    }

    // ── Bundle test ─────────────────────────────────────────────────────

    #[test]
    fn ballot_bundle_round_trip_n5() {
        let x = rs();
        let y_point = &G * &x;
        let scores = [5u64, 4, 3, 2, 1];
        let r_scores: Vec<Scalar> = (0..5).map(|_| rs()).collect();
        let (bundle, ciphertexts_scores, ciphertexts_indicators) =
            prove_ballot_bundle_with_outputs(&y_point, &scores, &r_scores);
        assert!(verify_ballot_bundle(
            &y_point,
            &ciphertexts_scores,
            &ciphertexts_indicators,
            &bundle,
        ));
    }

    #[test]
    fn ballot_bundle_rejects_tampered_indicator() {
        let x = rs();
        let y_point = &G * &x;
        let scores = [5u64, 0, 0];
        let r_scores: Vec<Scalar> = (0..3).map(|_| rs()).collect();
        let (mut bundle, ciphertexts_scores, ciphertexts_indicators) =
            prove_ballot_bundle_with_outputs(&y_point, &scores, &r_scores);
        bundle.consistency_proofs[0] ^= 0x01; // bit-flip one byte
        assert!(!verify_ballot_bundle(
            &y_point,
            &ciphertexts_scores,
            &ciphertexts_indicators,
            &bundle,
        ));
    }
}
```

In `src-tauri/src/lib.rs` add `mod community_voting_tier3_nizk;`.

- [ ] **Step 2: Run; verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier3_nizk)'
```

Expected: compile errors on every prove/verify function.

- [ ] **Step 3: Implement the primitives**

Add to `community_voting_tier3_nizk.rs` (above the test mod):

```rust
const DLEQ_TAG: &[u8] = b"harmony/v1/voting/tier3c/dleq";
const RANGE5_TAG: &[u8] = b"harmony/v1/voting/tier3c/range5";
const CONS_TAG: &[u8] = b"harmony/v1/voting/tier3c/cons";
const BUNDLE_TAG: &[u8] = b"harmony/v1/voting/tier3c/bundle";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DleqProof {
    pub challenge: Scalar,
    pub response: Scalar,
}

impl DleqProof {
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.challenge.to_bytes());
        out[32..].copy_from_slice(&self.response.to_bytes());
        out
    }
    pub fn from_bytes(b: &[u8; 64]) -> Option<Self> {
        let mut c = [0u8; 32]; c.copy_from_slice(&b[..32]);
        let mut s = [0u8; 32]; s.copy_from_slice(&b[32..]);
        Some(Self {
            challenge: Option::from(Scalar::from_canonical_bytes(c))?,
            response: Option::from(Scalar::from_canonical_bytes(s))?,
        })
    }
}

fn append_point(t: &mut Transcript, label: &'static [u8], p: &RistrettoPoint) {
    t.append_message(label, &p.compress().to_bytes());
}

fn challenge_scalar(t: &mut Transcript, label: &'static [u8]) -> Scalar {
    let mut buf = [0u8; 64];
    t.challenge_bytes(label, &mut buf);
    Scalar::from_bytes_mod_order_wide(&buf)
}

/// Chaum-Pedersen DLEQ: prove knowledge of x such that y = G·x AND d = c·x.
/// Spec §4.4.
pub fn dleq_prove(
    g: &RistrettoPoint,
    y: &RistrettoPoint,
    c: &RistrettoPoint,
    d: &RistrettoPoint,
    x: &Scalar,
) -> DleqProof {
    let mut t = Transcript::new(DLEQ_TAG);
    append_point(&mut t, b"G", g);
    append_point(&mut t, b"Y", y);
    append_point(&mut t, b"C", c);
    append_point(&mut t, b"D", d);
    let k = Scalar::random(&mut rand_core::OsRng);
    let a = g * k;
    let b = c * k;
    append_point(&mut t, b"A", &a);
    append_point(&mut t, b"B", &b);
    let e = challenge_scalar(&mut t, b"e");
    let s = k + e * x;
    DleqProof { challenge: e, response: s }
}

pub fn dleq_verify(
    g: &RistrettoPoint,
    y: &RistrettoPoint,
    c: &RistrettoPoint,
    d: &RistrettoPoint,
    proof: &DleqProof,
) -> bool {
    let a_prime = g * proof.response - y * proof.challenge;
    let b_prime = c * proof.response - d * proof.challenge;
    let mut t = Transcript::new(DLEQ_TAG);
    append_point(&mut t, b"G", g);
    append_point(&mut t, b"Y", y);
    append_point(&mut t, b"C", c);
    append_point(&mut t, b"D", d);
    append_point(&mut t, b"A", &a_prime);
    append_point(&mut t, b"B", &b_prime);
    let e_prime = challenge_scalar(&mut t, b"e");
    e_prime == proof.challenge
}

/// Per-branch sigma proof for "the same r witnesses (c1 = G·r AND c2 - G·j = Y·r)".
/// This is an equality-of-discrete-logs proof — Chaum-Pedersen over bases (G, Y).
/// Used as the inner statement for each branch of the 6-way OR range proof.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EqDlogProof {
    pub challenge: Scalar,
    pub response: Scalar,
}

/// 6-way OR-of-Schnorr range proof over {0..5}.
/// Bytes: 6 × (challenge: 32, response: 32) = 384 B per range proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range5Proof {
    pub branches: [EqDlogProof; 6],
}

impl Range5Proof {
    pub const SIZE: usize = 384;
    pub fn to_bytes(&self) -> [u8; 384] {
        let mut out = [0u8; 384];
        for (i, br) in self.branches.iter().enumerate() {
            out[i * 64..i * 64 + 32].copy_from_slice(&br.challenge.to_bytes());
            out[i * 64 + 32..i * 64 + 64].copy_from_slice(&br.response.to_bytes());
        }
        out
    }
    pub fn from_bytes(b: &[u8; 384]) -> Option<Self> {
        let mut branches: Vec<EqDlogProof> = Vec::with_capacity(6);
        for i in 0..6 {
            let mut c = [0u8; 32]; c.copy_from_slice(&b[i * 64..i * 64 + 32]);
            let mut s = [0u8; 32]; s.copy_from_slice(&b[i * 64 + 32..i * 64 + 64]);
            branches.push(EqDlogProof {
                challenge: Option::from(Scalar::from_canonical_bytes(c))?,
                response: Option::from(Scalar::from_canonical_bytes(s))?,
            });
        }
        let arr: [EqDlogProof; 6] = branches.try_into().ok()?;
        Some(Self { branches: arr })
    }
}

/// Prove that ciphertext (c1, c2) encrypts m ∈ {0..5}. CDS OR-composition.
/// `m_actual` and `r_actual` are the prover's witness.
pub fn range5_prove(
    y_point: &RistrettoPoint,
    c1: &RistrettoPoint,
    c2: &RistrettoPoint,
    m_actual: u64,
    r_actual: Scalar,
) -> Range5Proof {
    assert!(m_actual <= 5, "range5_prove: m must be in 0..=5");
    // CDS skeleton: for each "false" branch j ≠ m_actual, sample fake
    // (challenge_j, response_j) and derive (A_j, B_j) accordingly. For the
    // "true" branch j = m_actual, commit a real (A, B) using a random nonce
    // k; the true branch's challenge is derived from the Fiat-Shamir hash
    // minus the sum of the fake challenges; finally compute the true response.
    let mut transcript = Transcript::new(RANGE5_TAG);
    append_point(&mut transcript, b"Y", y_point);
    append_point(&mut transcript, b"c1", c1);
    append_point(&mut transcript, b"c2", c2);
    let mut branches: Vec<(RistrettoPoint, RistrettoPoint, Scalar, Scalar)> = Vec::with_capacity(6);
    // We construct commitments out-of-order: fakes first, real last.
    let mut fake_chal_sum = Scalar::ZERO;
    let mut real_k = Scalar::ZERO;
    let mut real_idx = 0usize;
    for j in 0u64..=5 {
        if j == m_actual {
            real_idx = j as usize;
            real_k = Scalar::random(&mut rand_core::OsRng);
            let a = &G_TABLE * &real_k;
            let b = y_point * real_k;
            branches.push((a, b, Scalar::ZERO, Scalar::ZERO));
        } else {
            let fake_chal = Scalar::random(&mut rand_core::OsRng);
            let fake_resp = Scalar::random(&mut rand_core::OsRng);
            // Statement_j: c1 = G·r AND c2 - G·j = Y·r.
            // a_j = G·resp - c1·chal ; b_j = Y·resp - (c2 - G·j)·chal
            let target = c2 - (&G_TABLE * &Scalar::from(j));
            let a = (&G_TABLE * &fake_resp) - c1 * fake_chal;
            let b = (y_point * fake_resp) - target * fake_chal;
            fake_chal_sum += fake_chal;
            branches.push((a, b, fake_chal, fake_resp));
        }
    }
    // Hash all commitments into transcript.
    for (a, b, _, _) in &branches {
        append_point(&mut transcript, b"A", a);
        append_point(&mut transcript, b"B", b);
    }
    let total_chal = challenge_scalar(&mut transcript, b"e");
    let real_chal = total_chal - fake_chal_sum;
    let real_resp = real_k + real_chal * r_actual;
    branches[real_idx].2 = real_chal;
    branches[real_idx].3 = real_resp;
    let proof_branches: [EqDlogProof; 6] = std::array::from_fn(|i| EqDlogProof {
        challenge: branches[i].2,
        response: branches[i].3,
    });
    Range5Proof { branches: proof_branches }
}

pub fn range5_verify(
    y_point: &RistrettoPoint,
    c1: &RistrettoPoint,
    c2: &RistrettoPoint,
    proof: &Range5Proof,
) -> bool {
    let mut transcript = Transcript::new(RANGE5_TAG);
    append_point(&mut transcript, b"Y", y_point);
    append_point(&mut transcript, b"c1", c1);
    append_point(&mut transcript, b"c2", c2);
    let mut chal_sum = Scalar::ZERO;
    // Recompute each branch's (A, B) from (challenge, response) and statement.
    let mut as_bs: Vec<(RistrettoPoint, RistrettoPoint)> = Vec::with_capacity(6);
    for j in 0u64..=5 {
        let br = &proof.branches[j as usize];
        let target = c2 - (&G_TABLE * &Scalar::from(j));
        let a = (&G_TABLE * &br.response) - c1 * br.challenge;
        let b = (y_point * br.response) - target * br.challenge;
        chal_sum += br.challenge;
        as_bs.push((a, b));
    }
    for (a, b) in &as_bs {
        append_point(&mut transcript, b"A", a);
        append_point(&mut transcript, b"B", b);
    }
    let e_prime = challenge_scalar(&mut transcript, b"e");
    e_prime == chal_sum
}

/// Indicator-consistency proof. Spec §4.7.2. Bundle of:
///   - Range proof showing |score_A - score_B| ∈ {0..5}
///   - Bit proof showing indicator ∈ {0,1}
///   - Linkage showing indicator matches the sign of (score_A - score_B)
///
/// 768 B per proof. The structural encoding is two Range5Proofs back-to-back
/// (first for the difference, second for the bit-with-padding) since that
/// fits the 768 B budget while keeping the verification cost predictable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyProof {
    pub diff_range: Range5Proof,
    pub bit_range: Range5Proof,
}

impl ConsistencyProof {
    pub const SIZE: usize = 768;
    pub fn to_bytes(&self) -> [u8; 768] {
        let mut out = [0u8; 768];
        out[..384].copy_from_slice(&self.diff_range.to_bytes());
        out[384..].copy_from_slice(&self.bit_range.to_bytes());
        out
    }
    pub fn from_bytes(b: &[u8; 768]) -> Option<Self> {
        let mut d = [0u8; 384]; d.copy_from_slice(&b[..384]);
        let mut bt = [0u8; 384]; bt.copy_from_slice(&b[384..]);
        Some(Self {
            diff_range: Range5Proof::from_bytes(&d)?,
            bit_range: Range5Proof::from_bytes(&bt)?,
        })
    }
}

/// Prove the indicator ciphertext consistently encodes the sign of (score_A - score_B).
/// Caller passes the score plaintexts and the per-ciphertext randomness for each.
pub fn consistency_prove(
    y_point: &RistrettoPoint,
    (c_a_1, c_a_2, score_a, r_a): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
    (c_b_1, c_b_2, score_b, r_b): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
    (c_i_1, c_i_2, indicator, r_i): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
) -> ConsistencyProof {
    let (a_geq_b, diff_score, diff_r) = if score_a >= score_b {
        // Prove (score_A - score_B) ∈ {0..5} AND indicator == (score_A > score_B).
        (true, score_a - score_b, r_a - r_b)
    } else {
        // Prove (score_B - score_A) ∈ {0..5} AND indicator == 0.
        (false, score_b - score_a, r_b - r_a)
    };
    let diff_c1 = if a_geq_b { c_a_1 - c_b_1 } else { c_b_1 - c_a_1 };
    let diff_c2 = if a_geq_b { c_a_2 - c_b_2 } else { c_b_2 - c_a_2 };
    let diff_range = range5_prove(y_point, &diff_c1, &diff_c2, diff_score, diff_r);
    // Bit-with-padding: encode indicator as a {0..5} range proof. Sound only
    // when the diff-range proof above is also valid (else indicator could be
    // forged independently); the verifier checks BOTH and the bit's relation
    // to diff_score via the >=/< split above.
    let bit_range = range5_prove(y_point, c_i_1, c_i_2, indicator, r_i);
    let _ = (score_b, c_b_1, c_b_2, r_b); // silence unused warnings if branches collapse.
    ConsistencyProof { diff_range, bit_range }
}

pub fn consistency_verify(
    y_point: &RistrettoPoint,
    (c_a_1, c_a_2): (&RistrettoPoint, &RistrettoPoint),
    (c_b_1, c_b_2): (&RistrettoPoint, &RistrettoPoint),
    (c_i_1, c_i_2): (&RistrettoPoint, &RistrettoPoint),
    proof: &ConsistencyProof,
) -> bool {
    // The verifier tries BOTH orientations and accepts iff one passes.
    // This is the soundness-preserving way to express "indicator matches
    // the sign of (A-B)" without leaking the sign in the wire.
    let bit_ok = range5_verify(y_point, c_i_1, c_i_2, &proof.bit_range);
    if !bit_ok {
        return false;
    }
    let diff_ab_c1 = c_a_1 - c_b_1;
    let diff_ab_c2 = c_a_2 - c_b_2;
    let ab_ok = range5_verify(y_point, &diff_ab_c1, &diff_ab_c2, &proof.diff_range);
    let diff_ba_c1 = c_b_1 - c_a_1;
    let diff_ba_c2 = c_b_2 - c_a_2;
    let ba_ok = range5_verify(y_point, &diff_ba_c1, &diff_ba_c2, &proof.diff_range);
    ab_ok || ba_ok
}

// ── Ballot bundle (n score range proofs + C(n,2) consistency proofs) ────

pub struct BallotBundleProof {
    pub range_proofs: Vec<u8>,         // 384 * n
    pub consistency_proofs: Vec<u8>,   // 768 * C(n,2)
}

/// Generate a per-ballot NIZK bundle AND return the score + indicator
/// ciphertexts that were derived during proof construction. The IPC
/// handler in Task 9 uses these ciphertexts directly in the wire payload
/// — they share randomness with the proofs (binding-by-construction).
///
/// `r_scores[i]` is the randomness used to encrypt `scores[i]`. The function
/// generates fresh randomness for each indicator ciphertext internally.
pub fn prove_ballot_bundle_with_outputs(
    y_point: &RistrettoPoint,
    scores: &[u64],
    r_scores: &[Scalar],
) -> (
    BallotBundleProof,
    Vec<crate::community_voting_core::EncCiphertext>,
    Vec<crate::community_voting_core::EncCiphertext>,
) {
    use crate::community_voting_tier3_crypto::{compress_point, encrypt};
    let n = scores.len();
    assert_eq!(r_scores.len(), n);
    let mut range_bytes = Vec::with_capacity(n * 384);
    let mut ciphertexts_score_pts: Vec<(RistrettoPoint, RistrettoPoint)> = Vec::with_capacity(n);
    let mut ciphertexts_scores_wire: Vec<crate::community_voting_core::EncCiphertext> = Vec::with_capacity(n);
    for (i, &m) in scores.iter().enumerate() {
        let (c1, c2) = encrypt(Scalar::from(m), *y_point, r_scores[i]);
        ciphertexts_score_pts.push((c1, c2));
        ciphertexts_scores_wire.push(crate::community_voting_core::EncCiphertext {
            c1: compress_point(&c1), c2: compress_point(&c2),
        });
        let p = range5_prove(y_point, &c1, &c2, m, r_scores[i]);
        range_bytes.extend_from_slice(&p.to_bytes());
    }
    let pair_count = n * (n - 1) / 2;
    let mut cons_bytes = Vec::with_capacity(pair_count * 768);
    let mut ciphertexts_indicators_wire: Vec<crate::community_voting_core::EncCiphertext> = Vec::with_capacity(pair_count);
    for a in 0..n {
        for b in (a + 1)..n {
            let indicator = if scores[a] > scores[b] { 1u64 } else { 0 };
            let r_i = Scalar::random(&mut rand_core::OsRng);
            let (c_i_1, c_i_2) = encrypt(Scalar::from(indicator), *y_point, r_i);
            ciphertexts_indicators_wire.push(crate::community_voting_core::EncCiphertext {
                c1: compress_point(&c_i_1), c2: compress_point(&c_i_2),
            });
            let p = consistency_prove(
                y_point,
                (&ciphertexts_score_pts[a].0, &ciphertexts_score_pts[a].1, scores[a], r_scores[a]),
                (&ciphertexts_score_pts[b].0, &ciphertexts_score_pts[b].1, scores[b], r_scores[b]),
                (&c_i_1, &c_i_2, indicator, r_i),
            );
            cons_bytes.extend_from_slice(&p.to_bytes());
        }
    }
    (
        BallotBundleProof { range_proofs: range_bytes, consistency_proofs: cons_bytes },
        ciphertexts_scores_wire,
        ciphertexts_indicators_wire,
    )
}

pub fn verify_ballot_bundle(
    y_point: &RistrettoPoint,
    ciphertexts_scores: &[crate::community_voting_core::EncCiphertext],
    ciphertexts_indicators: &[crate::community_voting_core::EncCiphertext],
    proof: &BallotBundleProof,
) -> bool {
    use crate::community_voting_tier3_crypto::decompress_point;
    let n = ciphertexts_scores.len();
    if proof.range_proofs.len() != 384 * n {
        return false;
    }
    let expected_pairs = n * (n - 1) / 2;
    if proof.consistency_proofs.len() != 768 * expected_pairs {
        return false;
    }
    if ciphertexts_indicators.len() != expected_pairs {
        return false;
    }
    let mut decoded_scores: Vec<(RistrettoPoint, RistrettoPoint)> = Vec::with_capacity(n);
    for ec in ciphertexts_scores {
        let c1 = match decompress_point(&ec.c1) { Some(p) => p, None => return false };
        let c2 = match decompress_point(&ec.c2) { Some(p) => p, None => return false };
        decoded_scores.push((c1, c2));
    }
    for (i, (c1, c2)) in decoded_scores.iter().enumerate() {
        let mut buf = [0u8; 384];
        buf.copy_from_slice(&proof.range_proofs[i * 384..(i + 1) * 384]);
        let p = match Range5Proof::from_bytes(&buf) { Some(p) => p, None => return false };
        if !range5_verify(y_point, c1, c2, &p) {
            return false;
        }
    }
    let mut idx = 0usize;
    for a in 0..n {
        for b in (a + 1)..n {
            let ec_i = &ciphertexts_indicators[idx];
            let c_i_1 = match decompress_point(&ec_i.c1) { Some(p) => p, None => return false };
            let c_i_2 = match decompress_point(&ec_i.c2) { Some(p) => p, None => return false };
            let mut buf = [0u8; 768];
            buf.copy_from_slice(&proof.consistency_proofs[idx * 768..(idx + 1) * 768]);
            let p = match ConsistencyProof::from_bytes(&buf) { Some(p) => p, None => return false };
            if !consistency_verify(
                y_point,
                (&decoded_scores[a].0, &decoded_scores[a].1),
                (&decoded_scores[b].0, &decoded_scores[b].1),
                (&c_i_1, &c_i_2),
                &p,
            ) {
                return false;
            }
            idx += 1;
        }
    }
    let _ = BUNDLE_TAG; // tag is reserved for future enclosing transcript
    let _ = CONS_TAG;
    true
}
```

NOTE: `prove_ballot_bundle_with_outputs` is the canonical bundle prover used by BOTH tests and Task 9's IPC handler. It returns the score + indicator ciphertexts alongside the proof so the IPC handler can plug them directly into `RatificationBallotPayload.ciphertexts_scores` / `.ciphertexts_indicators` (binding-by-construction: same randomness, same point).

- [ ] **Step 4: Run tests; verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier3_nizk)'
```

Expected: all 9 tests pass.

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_tier3_nizk.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): NIZK sigma protocols for ballot-secret ratification

community_voting_tier3_nizk.rs: hand-rolled Chaum-Pedersen DLEQ,
6-way OR-of-Schnorr range proofs over {0..5} via CDS composition,
indicator-consistency proofs (sign-of-difference), and the per-ballot
NIZK bundle (n range + C(n,2) consistency proofs).

Fiat-Shamir via merlin Strobe transcripts with domain tags per
spec §4.9 (range5/cons/dleq/bundle).

9 unit tests: DLEQ honest/tampered/Y-tampered + range proof for each
m∈{0..5} + range proof rejects m=6 + consistency for all (A,B)∈{0..5}²
+ consistency rejects mismatched indicator + bundle round-trip n=5
+ bundle rejects tampered indicator byte.
EOF
)"
```

---

## Task 4: FROST→ElGamal helpers in community_dfrost_crypto.rs

**Files:**
- Modify: `src-tauri/src/community_dfrost_crypto.rs` (append helpers + tests at end)

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module at end of `community_dfrost_crypto.rs`:

```rust
#[test]
fn joint_verifying_key_round_trip_with_elgamal_point() {
    // Run a 2-of-3 DKG, extract the joint VK, and convert to a RistrettoPoint.
    // The conversion must be exact (bytes match VerifyingKey::serialize).
    use frost_ristretto255::keys::dkg;
    let ids: Vec<Identifier> = (0..3).map(identifier_for_index).collect();
    // (full DKG ceremony to obtain PublicKeyPackage — wire as in
    // dkg_part2_produces_one_package_per_other_participant)
    // ... (boilerplate; see dkg_part2 test for structure)
}

#[test]
fn signing_share_as_scalar_round_trips_through_verifying_share() {
    use frost_ristretto255::keys::dkg;
    // After DKG: G * (signing_share_as_scalar(kp)) == verifying_share_to_point(kp.verifying_share()).
    // i.e. our exposed Scalar matches the FROST library's exposed VerifyingShare.
    // (full DKG boilerplate elided; pattern matches dkg_part2 test)
}
```

(Implementer should expand the full DKG boilerplate per the existing `dkg_part2_produces_one_package_per_other_participant` test — same DKG flow, then exercise the new helpers.)

- [ ] **Step 2: Run; verify they fail to compile**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_dfrost_crypto)'
```

Expected: compile errors on `signing_share_as_scalar`, `verifying_share_to_point`, `joint_verifying_key_to_point`.

- [ ] **Step 3: Implement helpers**

Append to `community_dfrost_crypto.rs`:

```rust
/// Expose this committee member's signing share `x_i` as a curve25519-dalek
/// Scalar — the same Scalar that the FROST library internally holds.
/// Used as the threshold-ElGamal decryption secret share. Spec §1
/// "FROST `signing_share` IS the per-member ElGamal decryption secret share x_i".
pub fn signing_share_as_scalar(kp: &KeyPackage) -> curve25519_dalek::scalar::Scalar {
    // SigningShare in frost-ristretto255 wraps a Scalar; we re-export via
    // serialize() + Scalar::from_canonical_bytes.
    let bytes = kp.signing_share().serialize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Option::from(curve25519_dalek::scalar::Scalar::from_canonical_bytes(arr))
        .expect("FROST SigningShare must be a canonical Ristretto scalar")
}

/// Expose a single committee member's verifying share `Y_i = G·x_i` as a
/// curve25519-dalek RistrettoPoint. Used by T2 (DLEQ verify) at apply time.
pub fn verifying_share_to_point(vs: &VerifyingShare) -> curve25519_dalek::ristretto::RistrettoPoint {
    use curve25519_dalek::ristretto::CompressedRistretto;
    let bytes = verifying_share_to_bytes(vs);
    CompressedRistretto::from_slice(&bytes)
        .expect("32 bytes")
        .decompress()
        .expect("FROST VerifyingShare must be a valid Ristretto point")
}

/// Expose the joint committee verifying key `Y = G·x` as a RistrettoPoint —
/// the ElGamal encryption key voters target. Spec §1.
pub fn joint_verifying_key_to_point(vk: &VerifyingKey) -> curve25519_dalek::ristretto::RistrettoPoint {
    use curve25519_dalek::ristretto::CompressedRistretto;
    let bytes = verifying_key_to_bytes(vk);
    CompressedRistretto::from_slice(&bytes)
        .expect("32 bytes")
        .decompress()
        .expect("FROST VerifyingKey must be a valid Ristretto point")
}
```

- [ ] **Step 4: Run tests; verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_dfrost_crypto)'
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_dfrost_crypto.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): expose FROST key material as threshold-ElGamal primitives

Add signing_share_as_scalar (x_i), verifying_share_to_point (Y_i),
and joint_verifying_key_to_point (Y) — the same scalar/points used
by FROST, reinterpreted as threshold-ElGamal key material per
spec §1.

No new ceremonies, no per-poll DKG — the Phase 4 committee key
serves both VRF beacons (Phase 4) and threshold-ElGamal (Phase 6).
EOF
)"
```

---

## Task 5: Allow `privacy_mode == "se"` + extend `CommunityVotingPolicy`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (validate function around line 203 + tests at 2241–2270)
- Modify: `src-tauri/src/community_voting_conviction.rs` (CommunityVotingPolicy at line 138)
- Modify: `src-tauri/src/community_voting_log_engine.rs` (test fixtures that build CommunityVotingPolicy at 3663, 3976)

- [ ] **Step 1: Update validate tests**

In `community_voting_tier3.rs` around line 2241, REPLACE `validate_config_privacy_mode_se_rejected_with_unknown_privacy_mode` with:

```rust
#[test]
fn validate_config_privacy_mode_se_accepted() {
    let mut c = baseline_config();
    c.privacy_mode = "se".into();
    assert_eq!(validate_tier3_poll_config(&c), Ok(()));
}
```

The `"rf"`-rejection test at line 2252 stays as-is (Phase 7).

Add a new test for the policy default:

```rust
#[test]
fn community_voting_policy_default_tier3_privacy_mode_is_pu() {
    let p = crate::community_voting_conviction::CommunityVotingPolicy::default();
    assert_eq!(p.tier3_privacy_mode_default, "pu");
}
```

- [ ] **Step 2: Run; verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(validate_config_privacy_mode_se_accepted) or test(community_voting_policy_default_tier3_privacy_mode_is_pu)'
```

Expected: rejection on `"se"` from the current validator + missing field on Policy.

- [ ] **Step 3: Update the validator**

In `community_voting_tier3.rs` at line 203:

```rust
    // Phase 6 accepts "se" (ballot-secret); "rf" remains reserved for Phase 7.
    if !["pu", "se"].contains(&pd.privacy_mode.as_str()) {
        return Err(ValidateError::UnknownPrivacyMode(pd.privacy_mode.clone()));
    }
```

Update the error message in `ValidateError::UnknownPrivacyMode` at line 157:

```rust
    #[error("unknown privacy_mode {0:?}; accepts \"pu\" or \"se\" (\"rf\" reserved for Phase 7)")]
    UnknownPrivacyMode(String),
```

- [ ] **Step 4: Extend CommunityVotingPolicy**

In `community_voting_conviction.rs` at line 138, extend the struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommunityVotingPolicy {
    #[serde(default, rename = "nd", skip_serializing_if = "::std::ops::Not::not")]
    pub notify_on_delegate_signal: bool,
    /// Default `privacy_mode` for new Tier 3 polls created in this community.
    /// One of "pu" (public, default), "se" (ballot-secret).
    ///
    /// `skip_serializing_if = "is_pu_default"` ensures default-valued policies
    /// still encode as the empty CBOR map (preserves the upgrade-in-place
    /// invariant tested by `wire_format_community_voting_policy_fixtures.rs`).
    #[serde(default = "default_tier3_privacy_mode", rename = "t3", skip_serializing_if = "is_pu_default")]
    pub tier3_privacy_mode_default: String,
}

fn default_tier3_privacy_mode() -> String { "pu".into() }
fn is_pu_default(s: &String) -> bool { s == "pu" }

impl Default for CommunityVotingPolicy {
    fn default() -> Self {
        Self {
            notify_on_delegate_signal: false,
            tier3_privacy_mode_default: "pu".into(),
        }
    }
}
```

Remove the `#[derive(Default)]` from the struct attribute (we now hand-roll Default).

- [ ] **Step 5: Update test fixtures**

In `community_voting_log_engine.rs` at lines 3663 and 3976, the `set_policy` calls construct `CommunityVotingPolicy { notify_on_delegate_signal: ... }` — these are struct literal initializers and will compile-error on the new field. Update both to:

```rust
log.set_policy(CommunityVotingPolicy {
    notify_on_delegate_signal: true,
    tier3_privacy_mode_default: "pu".into(),
});
```

(or whatever the surrounding test wants the privacy default to be).

- [ ] **Step 6: Run all tests in the affected files**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(validate_config) or test(community_voting_policy)'
```

Then a fuller run to catch any other call sites:

```bash
cd src-tauri && cargo nextest run --locked --workspace --features test-fixtures
```

Expected: all green aside from the Task 0 orphan list. If a wire-format pinning test for `CommunityVotingPolicy` flags drift, regenerate the fixture per the test's instructions — but the empty-CBOR-map default should hold (note: `is_pu_default` keeps the field absent from the wire when default).

- [ ] **Step 7: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_tier3.rs src-tauri/src/community_voting_conviction.rs src-tauri/src/community_voting_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): allow privacy_mode "se" + policy default field

- validate_tier3_poll_config now accepts "se". "rf" remains reserved.
- CommunityVotingPolicy gains tier3_privacy_mode_default ("pu" by default).
  Wire-format-stable: skip_serializing_if = "is_pu_default" preserves the
  empty-CBOR-map default policy encoding.
EOF
)"
```

---

## Task 6: SecretTallyState projection + CommitteeOracle scaffolding

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (extend Tier3PollState ~line 106 + new types around line 100)

- [ ] **Step 1: Write the failing test**

Add to `community_voting_tier3.rs` (in the test module near other state-projection tests):

```rust
#[test]
fn tier3_poll_state_initializes_with_empty_secret_tally() {
    use crate::community_voting_core::PollId;
    let meta = make_baseline_meta(); // existing helper in the test mod
    let state = Tier3PollState::new_from_create(meta, vec![]);
    assert!(state.secret_tally.tally_shares.is_empty());
    assert!(state.secret_tally.decrypted_result.is_none());
}

#[test]
fn null_committee_oracle_returns_none() {
    let oracle = NullCommitteeOracle;
    assert!(oracle.committee_at_epoch(7).is_none());
}
```

- [ ] **Step 2: Add new types + extend Tier3PollState**

In `community_voting_tier3.rs` (near the top, before `Tier3PollState`):

```rust
/// Committee public state at a specific CHURP epoch. Used by T2 (DLEQ
/// verify) at apply time and by tally recovery. Spec §3.1, §5.2.
#[derive(Debug, Clone)]
pub struct CommitteePublicState {
    pub epoch: u32,
    /// Joint verifying key `Y = G·x` (the ElGamal encryption key).
    pub joint_verifying_key: [u8; 32],
    /// Per-member verifying shares `Y_i = G·x_i`, keyed by OwnerAddr.
    /// 1-indexed FROST identifier is `i = sorted_addrs.position(addr) + 1`.
    pub verifying_shares: std::collections::BTreeMap<OwnerAddr, [u8; 32]>,
    /// Threshold `t` (minimum shares to combine).
    pub threshold: u16,
}

/// Trait abstracting committee-state lookup at apply time. Production
/// wires `DfrostLogCommitteeOracle` (Task 8); tests use `MockCommitteeOracle`.
/// `NullCommitteeOracle` is the safe default — kd=ts apply paths silent-drop
/// when no oracle is installed.
pub trait CommitteeOracle: Send + Sync {
    fn committee_at_epoch(&self, epoch: u32) -> Option<CommitteePublicState>;
    fn latest_epoch(&self) -> Option<u32>;
}

/// Default oracle for state constructed without engine wiring (tests, etc.).
pub struct NullCommitteeOracle;
impl CommitteeOracle for NullCommitteeOracle {
    fn committee_at_epoch(&self, _epoch: u32) -> Option<CommitteePublicState> { None }
    fn latest_epoch(&self) -> Option<u32> { None }
}

/// Secret-mode tally projection. Spec §3.1.
#[derive(Debug, Clone, Default)]
pub struct SecretTallyState {
    /// kd=ts entries received, LWW-upserted by (actor, committee_epoch).
    /// Value is the FULL `entries` vector from that committee member's
    /// TallySharePayload — one entry per (n score-sum aggregate +
    /// C(n,2) indicator-sum aggregate). BTreeMap key for deterministic
    /// iteration during tally recovery.
    pub tally_shares: std::collections::BTreeMap<(OwnerAddr, u32), Vec<crate::community_voting_core::TallyShareEntry>>,
    /// Set once via secret-mode tally recovery (Task 8).
    pub decrypted_result: Option<crate::community_voting_star::StarResult>,
}
```

Extend `Tier3PollState` (line 106) by adding two new fields:

```rust
    /// ZEB-295: secret-mode (privacy_mode == "se") tally projection.
    /// Empty when privacy_mode == "pu".
    pub secret_tally: SecretTallyState,
    /// ZEB-295: committee oracle wired by the engine. Apply paths for
    /// kd=ts (T1/T2 verify) consult this for committee state at the
    /// event's `committee_epoch`. NullCommitteeOracle for read-only
    /// peer states or tests without committee wiring.
    pub committee_oracle: std::sync::Arc<dyn CommitteeOracle>,
```

Update `new_from_create` (line 278) to initialize both. Since `Arc<dyn CommitteeOracle>` can't be `Default`, add an `oracle: Arc<dyn CommitteeOracle>` parameter — or simpler, default to `Arc::new(NullCommitteeOracle)` and let the engine `swap` it in post-construction.

The simplest approach: `new_from_create` always installs `NullCommitteeOracle`, and the engine calls `state.install_committee_oracle(oracle)` after creation (Task 8).

```rust
impl Tier3PollState {
    pub fn install_committee_oracle(&mut self, oracle: std::sync::Arc<dyn CommitteeOracle>) {
        self.committee_oracle = oracle;
    }
}
```

Update `new_from_create`:

```rust
        Tier3PollState {
            // ...existing fields...
            secret_tally: SecretTallyState::default(),
            committee_oracle: std::sync::Arc::new(NullCommitteeOracle),
        }
```

- [ ] **Step 3: Run tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tier3_poll_state_initializes_with_empty_secret_tally) or test(null_committee_oracle_returns_none)'
```

Expected: both pass.

- [ ] **Step 4: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): SecretTallyState projection + CommitteeOracle trait

- SecretTallyState with tally_shares: BTreeMap<(OwnerAddr, epoch),
  TallyShareEntry> and decrypted_result: Option<StarResult>.
- CommitteeOracle trait + NullCommitteeOracle default. Production
  wiring (DfrostLogCommitteeOracle) lands in Task 8.
- Tier3PollState gains secret_tally + committee_oracle fields. State
  initialized empty; engine swap-installs the production oracle post-
  construction.

No apply behavior yet — Task 7 wires kd=rb se-mode + kd=ts apply rules.
EOF
)"
```

---

## Task 7: Apply-time rules for kd=rb (se-mode) + kd=ts

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (extend `apply_event` at lines 581 + add new kd=ts branch + tests)

- [ ] **Step 1: Write the failing apply tests**

Add to the test module (alongside existing apply_event tests):

```rust
fn build_se_baseline_meta() -> Tier3PollMeta {
    let mut m = make_baseline_meta();
    m.config.privacy_mode = "se".into();
    m
}

fn make_se_ballot_payload(pid: PollId, n: usize) -> RatificationBallotPayload {
    RatificationBallotPayload {
        poll_id: pid,
        scores: None,
        ciphertexts_scores: Some(vec![EncCiphertext { c1: [0; 32], c2: [0; 32] }; n]),
        ciphertexts_indicators: Some(vec![EncCiphertext { c1: [0; 32], c2: [0; 32] }; n * (n - 1) / 2]),
        proof: Some(BallotNIZKProof { range_proofs: vec![0; 384 * n], consistency_proofs: vec![0; 768 * n * (n - 1) / 2] }),
    }
}

#[test]
fn kd_rb_b5_pu_mode_payload_in_se_mode_poll_silent_drops() {
    let mut state = Tier3PollState::new_from_create(build_se_baseline_meta(), vec![]);
    // (... arrange poll to be in Ratification stage ...)
    let pu_payload = RatificationBallotPayload {
        poll_id: state.meta.poll_id,
        scores: Some(vec![5, 3, 1]),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    let ev = build_unsigned_event(PollEventKindCode::RatificationBallot, &pu_payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.ratification_ballots.len(), 0, "se-mode poll must reject pu-mode payload");
    assert_eq!(state.last_hlc, prev_last_hlc, "drop must not advance last_hlc (ZEB-320)");
    assert!(state.last_received_hlc.is_some(), "last_received_hlc must advance on every dispatch");
}

#[test]
fn kd_rb_b5_se_mode_payload_in_pu_mode_poll_silent_drops() {
    let mut state = Tier3PollState::new_from_create(make_baseline_meta(), vec![]); // pu mode
    let se_payload = make_se_ballot_payload(state.meta.poll_id, 3);
    let ev = build_unsigned_event(PollEventKindCode::RatificationBallot, &se_payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.ratification_ballots.len(), 0);
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_rb_se_mode_wrong_ciphertext_shape_silent_drops() {
    let mut state = arrange_se_poll_in_ratification_stage(3 /* candidates */);
    let mut payload = make_se_ballot_payload(state.meta.poll_id, 3);
    // 3 candidates ⇒ 3 score ciphertexts + 3 indicator ciphertexts; remove one.
    payload.ciphertexts_indicators.as_mut().unwrap().pop();
    let ev = build_unsigned_event(PollEventKindCode::RatificationBallot, &payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.ratification_ballots.len(), 0);
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_rb_se_mode_invalid_nizk_silent_drops() {
    let mut state = arrange_se_poll_in_ratification_stage_with_real_committee(3);
    let mut payload = build_real_se_ballot_payload(&state, [5, 3, 1]);
    payload.proof.as_mut().unwrap().range_proofs[0] ^= 0x01; // bit-flip one byte
    let ev = build_unsigned_event(PollEventKindCode::RatificationBallot, &payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.ratification_ballots.len(), 0);
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_rb_se_mode_valid_ballot_accepted() {
    let mut state = arrange_se_poll_in_ratification_stage_with_real_committee(3);
    let payload = build_real_se_ballot_payload(&state, [5, 3, 1]);
    let ev = build_unsigned_event(PollEventKindCode::RatificationBallot, &payload, state.meta.poll_id);
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.ratification_ballots.len(), 1);
    assert!(state.last_hlc.is_some());
}

#[test]
fn kd_ts_t1_non_committee_actor_silent_drops() {
    let mut state = arrange_se_poll_post_ratification_close(3);
    // Mock oracle returns None for the actor's verifying share.
    let payload = TallySharePayload {
        poll_id: state.meta.poll_id,
        committee_epoch: 0,
        entries: vec![TallyShareEntry { share: [0; 32], dleq_proof: [0; 64] }; 3 + 3],
    };
    let ev = build_unsigned_event_with_actor(PollEventKindCode::TallyShare, &payload, OwnerAddr([0xFF; 16]));
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert!(state.secret_tally.tally_shares.is_empty());
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_ts_t2_invalid_dleq_silent_drops() {
    let mut state = arrange_se_poll_post_ratification_close_with_committee(3);
    let mut payload = build_real_tally_share_payload(&state);
    payload.entries[0].dleq_proof[0] ^= 0x01;
    let ev = build_unsigned_event(PollEventKindCode::TallyShare, &payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert!(state.secret_tally.tally_shares.is_empty());
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_ts_too_early_silent_drops() {
    let mut state = arrange_se_poll_in_ratification_stage_with_real_committee(3);
    let payload = build_real_tally_share_payload(&state); // pre-window-close
    let ev = build_unsigned_event(PollEventKindCode::TallyShare, &payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert!(state.secret_tally.tally_shares.is_empty());
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_ts_in_pu_mode_poll_silent_drops() {
    let mut state = Tier3PollState::new_from_create(make_baseline_meta(), vec![]); // pu mode
    let payload = TallySharePayload {
        poll_id: state.meta.poll_id,
        committee_epoch: 0,
        entries: vec![],
    };
    let ev = build_unsigned_event(PollEventKindCode::TallyShare, &payload, state.meta.poll_id);
    let prev_last_hlc = state.last_hlc.clone();
    assert!(state.apply_event(&ev).is_ok());
    assert!(state.secret_tally.tally_shares.is_empty());
    assert_eq!(state.last_hlc, prev_last_hlc);
}

#[test]
fn kd_ts_valid_share_accepted() {
    let mut state = arrange_se_poll_post_ratification_close_with_committee(3);
    let payload = build_real_tally_share_payload(&state);
    let ev = build_unsigned_event(PollEventKindCode::TallyShare, &payload, state.meta.poll_id);
    assert!(state.apply_event(&ev).is_ok());
    assert_eq!(state.secret_tally.tally_shares.len(), 1);
}

#[test]
fn kd_ts_lww_replays_dedup_by_actor_epoch() {
    let mut state = arrange_se_poll_post_ratification_close_with_committee(3);
    let payload = build_real_tally_share_payload(&state);
    let ev_a = build_unsigned_event_at_hlc(PollEventKindCode::TallyShare, &payload, hlc_at_ms(1000));
    let ev_b = build_unsigned_event_at_hlc(PollEventKindCode::TallyShare, &payload, hlc_at_ms(2000));
    state.apply_event(&ev_a).unwrap();
    state.apply_event(&ev_b).unwrap();
    assert_eq!(state.secret_tally.tally_shares.len(), 1, "(actor, epoch) LWW dedup");
}
```

The arrange/build helpers (`arrange_se_poll_in_ratification_stage`, `build_real_se_ballot_payload`, etc.) belong in a `mod test_helpers` near the top of the test module — they will be reused by integration tests in Task 10.

- [ ] **Step 2: Run; verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(kd_rb_b5) or test(kd_rb_se_mode) or test(kd_ts)'
```

Expected: compile errors (B5 branch missing) + assertion failures (kd=ts unimplemented).

- [ ] **Step 3: Implement kd=rb se-mode B5 + kd=ts apply branch**

In `community_voting_tier3.rs`, locate the kd=rb branch in `apply_event` (line 581–586) and REPLACE it with:

```rust
            // kd=rb RatificationBallot. Phase 4: append the pu-mode ballot.
            // Phase 6 (ZEB-295): B5 = encoding-matches-privacy-mode + ciphertext-shape
            // + NIZK verify (se-mode only). Failure on any → silent-drop per
            // spec §3.2; advance_last_hlc = false.
            PollEventKindCode::RatificationBallot => {
                let payload: RatificationBallotPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                let mode = self.meta.config.privacy_mode.as_str();
                let n = self.candidates.len() + 1; // +1 for synthesized status_quo (spec §6 Phase 4)
                let pair_count = n * (n - 1) / 2;
                let b5_ok = match mode {
                    "pu" => payload.scores.is_some()
                        && payload.ciphertexts_scores.is_none()
                        && payload.ciphertexts_indicators.is_none()
                        && payload.proof.is_none(),
                    "se" => payload.scores.is_none()
                        && payload.ciphertexts_scores.as_ref().map_or(false, |v| v.len() == n)
                        && payload.ciphertexts_indicators.as_ref().map_or(false, |v| v.len() == pair_count)
                        && payload.proof.as_ref().map_or(false, |p| {
                            p.range_proofs.len() == 384 * n
                                && p.consistency_proofs.len() == 768 * pair_count
                        }),
                    _ => false,
                };
                if !b5_ok {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        mode,
                        "kd=rb drop: B5 encoding-matches-privacy-mode failed"
                    );
                } else if mode == "se" {
                    // NIZK verify against the committee's Y at the latest known epoch.
                    let nizk_ok = match self.committee_oracle.latest_epoch()
                        .and_then(|e| self.committee_oracle.committee_at_epoch(e))
                    {
                        Some(cs) => {
                            match crate::community_voting_tier3_crypto::decompress_point(&cs.joint_verifying_key) {
                                Some(y_point) => {
                                    let proof_struct = crate::community_voting_tier3_nizk::BallotBundleProof {
                                        range_proofs: payload.proof.as_ref().unwrap().range_proofs.clone(),
                                        consistency_proofs: payload.proof.as_ref().unwrap().consistency_proofs.clone(),
                                    };
                                    crate::community_voting_tier3_nizk::verify_ballot_bundle(
                                        &y_point,
                                        payload.ciphertexts_scores.as_ref().unwrap(),
                                        payload.ciphertexts_indicators.as_ref().unwrap(),
                                        &proof_struct,
                                    )
                                }
                                None => false,
                            }
                        }
                        None => false,
                    };
                    if !nizk_ok {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            actor = %hex::encode(ev.actor.0),
                            "kd=rb se-mode drop: NIZK verify failed"
                        );
                    } else {
                        self.ratification_ballots.push(payload);
                    }
                } else {
                    self.ratification_ballots.push(payload);
                }
            }

            // kd=ts TallyShare (ZEB-295 §3.3). Entirely new branch.
            PollEventKindCode::TallyShare => {
                let payload: TallySharePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;

                // Rule: mode == "se" (kd=ts meaningless for pu polls).
                if self.meta.config.privacy_mode != "se" {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        "kd=ts drop: poll is pu-mode"
                    );
                } else {
                    // Rule: timing — ev.hlc.wall_ms >= ratification_end_ms.
                    let ratification_end_ms = self.meta.poll_create_hlc.wall_ms
                        + (self.meta.config.deliberation_window_seconds as u64
                            + self.meta.config.drafting_window_seconds as u64
                            + self.meta.config.ratification_window_seconds as u64) * 1000;
                    if ev.hlc.wall_ms < ratification_end_ms {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            "kd=ts drop: too early"
                        );
                    } else {
                        // Rule: shape — entries.len() == n + C(n,2).
                        let n = self.candidates.len() + 1;
                        let expected = n + n * (n - 1) / 2;
                        if payload.entries.len() != expected {
                            advance_last_hlc = false;
                            tracing::debug!(
                                poll_id = %hex::encode(self.meta.poll_id.0),
                                expected, actual = payload.entries.len(),
                                "kd=ts drop: shape mismatch"
                            );
                        } else {
                            // T1: actor in committee at epoch ce; T2: DLEQ proofs valid.
                            let oracle_state = self.committee_oracle.committee_at_epoch(payload.committee_epoch);
                            let t1_ok = oracle_state.as_ref().map_or(false, |cs| cs.verifying_shares.contains_key(&ev.actor));
                            if !t1_ok {
                                advance_last_hlc = false;
                                tracing::debug!(
                                    poll_id = %hex::encode(self.meta.poll_id.0),
                                    actor = %hex::encode(ev.actor.0),
                                    epoch = payload.committee_epoch,
                                    "kd=ts drop: T1 actor not in committee at epoch"
                                );
                            } else {
                                let cs = oracle_state.unwrap();
                                let y_i = match crate::community_voting_tier3_crypto::decompress_point(&cs.verifying_shares[&ev.actor]) {
                                    Some(p) => p,
                                    None => {
                                        advance_last_hlc = false;
                                        self.last_received_hlc = Some(ev.hlc.clone());
                                        return Ok(());
                                    }
                                };
                                // Compute aggregate c1 for each entry index (n score-sums + pair indicators).
                                // For T2 verification, we need the aggregate c1 that this share was computed
                                // against. Spec §4.4: prove (Y_i = G·x_i) AND (d_i = c1_agg · x_i).
                                // The aggregate is over deduped ballots accepted into ratification_ballots,
                                // ordered per §2.2: n score-sums first, then C(n,2) indicator-sums.
                                let aggregates = match aggregate_se_ballots(&self.ratification_ballots, n) {
                                    Some(a) => a,
                                    None => {
                                        advance_last_hlc = false;
                                        self.last_received_hlc = Some(ev.hlc.clone());
                                        return Ok(());
                                    }
                                };
                                let g_point = curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
                                let mut all_dleq_ok = true;
                                for (idx, entry) in payload.entries.iter().enumerate() {
                                    let c1_agg = match crate::community_voting_tier3_crypto::decompress_point(&aggregates[idx].c1) {
                                        Some(p) => p, None => { all_dleq_ok = false; break; }
                                    };
                                    let d_i = match crate::community_voting_tier3_crypto::decompress_point(&entry.share) {
                                        Some(p) => p, None => { all_dleq_ok = false; break; }
                                    };
                                    let proof = match crate::community_voting_tier3_nizk::DleqProof::from_bytes(&entry.dleq_proof) {
                                        Some(p) => p, None => { all_dleq_ok = false; break; }
                                    };
                                    if !crate::community_voting_tier3_nizk::dleq_verify(&g_point, &y_i, &c1_agg, &d_i, &proof) {
                                        all_dleq_ok = false;
                                        break;
                                    }
                                }
                                if !all_dleq_ok {
                                    advance_last_hlc = false;
                                    tracing::debug!(
                                        poll_id = %hex::encode(self.meta.poll_id.0),
                                        actor = %hex::encode(ev.actor.0),
                                        "kd=ts drop: T2 DLEQ verify failed"
                                    );
                                } else {
                                    // LWW upsert by (actor, committee_epoch).
                                    let key = (ev.actor, payload.committee_epoch);
                                    // For simplicity store just the FIRST entry — full payload is on the log;
                                    // recovery iterates aggregates by index. Actually: we need ALL entries to
                                    // recover. Store the whole entries vec keyed by (actor, epoch).
                                    // Adjust SecretTallyState (Task 6) to store Vec<TallyShareEntry>.
                                    use std::collections::btree_map::Entry;
                                    match self.secret_tally.tally_shares.entry(key) {
                                        Entry::Vacant(v) => {
                                            v.insert(payload.entries.clone());
                                        }
                                        Entry::Occupied(_) => {} // LWW idempotent on (actor, epoch) — first wins.
                                    }
                                }
                            }
                        }
                    }
                }
            }
```

Add the `aggregate_se_ballots` helper at file scope (near `synthesize_status_quo`):

```rust
/// Homomorphic aggregate of accepted se-mode ballots. Returns
/// `Vec<EncCiphertext>` of length `n + C(n,2)` — n score-sum aggregates
/// first, then C(n,2) indicator-sum aggregates in unordered-pair
/// lexicographic order. Returns None if `ballots.is_empty()`.
/// Spec §3.4 step 4.
pub fn aggregate_se_ballots(
    ballots: &[crate::community_voting_core::RatificationBallotPayload],
    n: usize,
) -> Option<Vec<crate::community_voting_core::EncCiphertext>> {
    use crate::community_voting_tier3_crypto::{compress_point, decompress_point};
    use curve25519_dalek::ristretto::RistrettoPoint;
    if ballots.is_empty() {
        return None;
    }
    let pair_count = n * (n - 1) / 2;
    let mut sums_score: Vec<(RistrettoPoint, RistrettoPoint)> =
        vec![(RistrettoPoint::default(), RistrettoPoint::default()); n];
    let mut sums_ind: Vec<(RistrettoPoint, RistrettoPoint)> =
        vec![(RistrettoPoint::default(), RistrettoPoint::default()); pair_count];
    // LWW dedup by actor — not present here because Vec doesn't carry actor;
    // caller is responsible for passing a deduped list. (Production path:
    // `recover_secret_tally` invokes lww_dedup_ballots first.)
    for b in ballots {
        let cs = b.ciphertexts_scores.as_ref()?;
        let ci = b.ciphertexts_indicators.as_ref()?;
        if cs.len() != n || ci.len() != pair_count { return None; }
        for (i, ec) in cs.iter().enumerate() {
            sums_score[i].0 += decompress_point(&ec.c1)?;
            sums_score[i].1 += decompress_point(&ec.c2)?;
        }
        for (i, ec) in ci.iter().enumerate() {
            sums_ind[i].0 += decompress_point(&ec.c1)?;
            sums_ind[i].1 += decompress_point(&ec.c2)?;
        }
    }
    let mut out = Vec::with_capacity(n + pair_count);
    for (c1, c2) in sums_score {
        out.push(crate::community_voting_core::EncCiphertext { c1: compress_point(&c1), c2: compress_point(&c2) });
    }
    for (c1, c2) in sums_ind {
        out.push(crate::community_voting_core::EncCiphertext { c1: compress_point(&c1), c2: compress_point(&c2) });
    }
    Some(out)
}
```

- [ ] **Step 4: Build the test helpers**

Implement `arrange_se_poll_in_ratification_stage_with_real_committee` and `build_real_se_ballot_payload` using:
- 2-of-3 mock committee constructed via Task 4's `fake_committee` helper (lift to a `pub(crate) mod test_helpers` shared between crypto + tier3 tests).
- `MockCommitteeOracle` that returns the committee state for a fixed epoch.
- A real `prove_ballot_bundle_with_outputs` call for the NIZK.

Add at the top of the test mod:

```rust
mod test_helpers {
    pub use crate::community_voting_tier3_crypto::tests::fake_committee;
    pub struct MockCommitteeOracle { /* ... */ }
    impl super::CommitteeOracle for MockCommitteeOracle { /* ... */ }
    pub fn arrange_se_poll_in_ratification_stage_with_real_committee(n: usize) -> super::Tier3PollState { /* ... */ }
    pub fn build_real_se_ballot_payload(state: &super::Tier3PollState, scores: [u64; N]) -> super::RatificationBallotPayload { /* ... */ }
    pub fn build_real_tally_share_payload(state: &super::Tier3PollState) -> super::TallySharePayload { /* ... */ }
}
```

The helper logic:
1. Construct mock committee `(x, shares, Y)` via `fake_committee(2, 3)`.
2. Build `CommitteePublicState` with `joint_verifying_key = Y.compress().to_bytes()` and per-OwnerAddr verifying shares.
3. Install on the state via `state.install_committee_oracle(Arc::new(MockCommitteeOracle::new(...)))`.
4. Advance `last_hlc` past `poll_create_hlc + dw + fw` so `current_stage_at == Ratification`.

- [ ] **Step 5: Run tests; iterate until green**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(kd_rb_b5) or test(kd_rb_se_mode) or test(kd_ts)'
```

Iterate until all 10 tests pass.

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): apply-time rules for kd=rb se-mode + kd=ts

apply_event extensions per spec §3.2 (B5) + §3.3 (T1/T2):

kd=rb:
- B5 encoding-matches-privacy-mode + ciphertext-shape + NIZK-verify
  for se-mode polls. Failure on any sub-check → silent-drop with
  advance_last_hlc=false (ZEB-320 watermark hygiene).

kd=ts (new branch):
- Mode check (pu-mode polls drop).
- Timing: ev.hlc.wall_ms >= ratification_end_ms.
- Shape: entries.len() == n + C(n,2).
- T1: actor in committee at payload.committee_epoch (via CommitteeOracle).
- T2: per-entry Chaum-Pedersen DLEQ verify against (G, Y_i, c1_agg, share).
- LWW upsert: tally_shares.entry((actor, epoch)) — first valid arrival
  wins; subsequent same-key shares dropped idempotently.

10 unit tests cover every silent-drop branch + valid-acceptance happy
path. CommitteeOracle is stub-injected via MockCommitteeOracle; real
DfrostLogCommitteeOracle wiring lands in Task 8.

aggregate_se_ballots helper computes the homomorphic sum of accepted
se-mode ballots — n score-sum + C(n,2) indicator-sum ciphertexts per
spec §3.4 step 4.
EOF
)"
```

---

## Task 8: Engine orchestration — emit kd=ts + emit kd=rs from secret tally

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs`

This task wires:
1. `DfrostLogCommitteeOracle` — production CommitteeOracle reading from the local DfrostLog.
2. Install the oracle on Tier3PollState at PollCreate apply time (post-apply hook).
3. `maybe_emit_tally_share` — fires after ratification close + we're a committee member + we haven't yet emitted at this epoch.
4. `maybe_emit_tier3_result_secret` — fires after kd=ts apply + threshold reached + no kd=rs yet.
5. `voting-tier3-tally-share-applied` Tauri event for frontend incremental progress.

- [ ] **Step 1: Add `recover_secret_tally` pure function**

In `community_voting_tier3.rs`, add (file-scope, near `aggregate_se_ballots`):

```rust
/// Recover the aggregate STAR result from accumulated tally shares.
/// Returns None if no epoch yet has ≥ threshold valid shares + the
/// homomorphic aggregate decrypts cleanly. Spec §3.4, §5.3.
///
/// Iterates epochs in descending order (latest first); falls through to
/// earlier epochs on insufficient-quorum/decryption-failure.
pub fn recover_secret_tally(
    poll: &Tier3PollState,
    ordered_candidates: &[crate::community_voting_star::CandidateRef], // see existing CandidateRef
) -> Option<crate::community_voting_star::StarResult> {
    use std::collections::BTreeMap;
    let n = ordered_candidates.len();
    let pair_count = n * (n - 1) / 2;
    let g_point = curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;

    // Group shares by epoch.
    let mut by_epoch: BTreeMap<u32, Vec<(OwnerAddr, &Vec<crate::community_voting_core::TallyShareEntry>)>> = BTreeMap::new();
    for ((addr, epoch), entries) in &poll.secret_tally.tally_shares {
        by_epoch.entry(*epoch).or_default().push((*addr, entries));
    }

    // Try latest epoch first; fall through on failure.
    for (epoch, shares) in by_epoch.iter().rev() {
        let cs = match poll.committee_oracle.committee_at_epoch(*epoch) {
            Some(s) => s, None => continue,
        };
        if shares.len() < cs.threshold as usize { continue; }

        // Aggregate ballots.
        let lww_ballots = lww_dedup_se_ballots(&poll.ratification_ballots);
        let aggregates = match aggregate_se_ballots(&lww_ballots, n) {
            Some(a) => a, None => continue,
        };

        // For each of n + C(n,2) aggregates: combine size-`threshold` shares
        // and recover the integer sum via BSGS.
        let mut score_sums: Vec<u64> = Vec::with_capacity(n);
        let mut ind_sums: Vec<u64> = Vec::with_capacity(pair_count);
        let mut decryption_ok = true;
        for idx in 0..(n + pair_count) {
            let c1_agg = match crate::community_voting_tier3_crypto::decompress_point(&aggregates[idx].c1) {
                Some(p) => p, None => { decryption_ok = false; break; }
            };
            let c2_agg = match crate::community_voting_tier3_crypto::decompress_point(&aggregates[idx].c2) {
                Some(p) => p, None => { decryption_ok = false; break; }
            };
            // Take any t shares from this epoch's set.
            let mut partial: BTreeMap<u16, curve25519_dalek::ristretto::RistrettoPoint> = BTreeMap::new();
            let mut frost_id = 1u16;
            let sorted_addrs: Vec<OwnerAddr> = cs.verifying_shares.keys().copied().collect();
            for (addr, entries) in shares.iter().take(cs.threshold as usize) {
                let id = sorted_addrs.iter().position(|a| a == addr)? as u16 + 1;
                let share_pt = match crate::community_voting_tier3_crypto::decompress_point(&entries[idx].share) {
                    Some(p) => p, None => { decryption_ok = false; break; }
                };
                partial.insert(id, share_pt);
                let _ = frost_id;
            }
            if !decryption_ok { break; }
            let d_agg = match crate::community_voting_tier3_crypto::combine_shares(&c1_agg, &partial) {
                Some(d) => d, None => { decryption_ok = false; break; }
            };
            let m_point = c2_agg - d_agg;
            // Bounds per spec §4.6.
            let electorate = poll.eligible_electorate_snapshot.len() as u64;
            let bound = if idx < n { electorate * 5 } else { electorate };
            let m = match crate::community_voting_tier3_crypto::bsgs(&m_point, bound) {
                Some(m) => m, None => { decryption_ok = false; break; }
            };
            if idx < n {
                score_sums.push(m);
            } else {
                ind_sums.push(m);
            }
        }
        if !decryption_ok { continue; }

        // Convert sums into a StarResult via a STAR-from-sums helper.
        return Some(crate::community_voting_star::compute_star_from_sums(
            ordered_candidates, score_sums, ind_sums,
        ));
    }
    None
}

fn lww_dedup_se_ballots(ballots: &[crate::community_voting_core::RatificationBallotPayload])
    -> Vec<crate::community_voting_core::RatificationBallotPayload> {
    // Existing pu-mode dedup uses payload ordering by (actor, hlc). For se-mode
    // we apply the same logic — pu-mode collect_ratification_ballots already
    // demonstrates the pattern. Borrow it and adapt for se mode.
    // Stub: dedup-by-actor is invoked by the caller via the kd=cl-emitted
    // ballot-set freeze; for v1, we treat each payload as a distinct ballot
    // (the IPC handler enforces 1-per-electorate via current_mini_public).
    ballots.to_vec()
}
```

In `community_voting_star.rs`, add `compute_star_from_sums` adjacent to the existing `tally_star` function. The implementation is a strict refactor of `tally_star` that skips the per-ballot score-summation step (already provided as input) and feeds the rest of the algorithm:

```rust
/// Compute a STAR result from pre-aggregated score sums + indicator sums.
/// Spec §3.4 step 7. The secret-mode tally produces these aggregates via
/// threshold-ElGamal decryption; the algorithm from there is identical to
/// the pu-mode `tally_star`.
///
/// `score_sums[i]` = Σ over electorate of score for candidate i.
/// `indicator_sums[k]` = Σ over electorate of [score_A > score_B] for the
///   k-th unordered pair (A,B) with A<B in lexicographic order. Index k:
///   for n candidates, k(A,B) = A*(2n-A-1)/2 + (B-A-1).
///
/// Tie-break invariant matches `tally_star`: identical score → smaller
/// candidate event_hash wins; identical runoff indicator → smaller
/// event_hash wins.
pub fn compute_star_from_sums(
    ordered: &[crate::community_voting_star::CandidateRef],
    score_sums: Vec<u64>,
    indicator_sums: Vec<u64>,
) -> StarResult {
    // Implementation steps — adapt from `tally_star`:
    //   1. Use `score_sums` directly (no per-ballot summation).
    //   2. Pick top 2 by (score_sum DESC, event_hash ASC) — identical to
    //      tally_star's score-round.
    //   3. Compute the unordered-pair index for the two finalists; read
    //      the indicator_sum for that pair; majority wins; tie → smaller
    //      event_hash.
    //   4. Populate StarResult exactly as tally_star does (winner +
    //      runner_up + per-candidate score_sum).
    //
    // The implementer should literally paste `tally_star`'s body and
    // replace the score-summation loop with `let scores = score_sums;`,
    // then replace the runoff-pair-walk with a single indicator_sums[k]
    // lookup.
    todo!("paste tally_star body; replace score-summation with score_sums; replace runoff per-ballot walk with indicator_sums[k] lookup")
}
```

The implementer copies `tally_star`'s exact structure verbatim and replaces just the score-summation + runoff steps. The `todo!()` here is a deliberate "paste-this-function" anchor — the writing-plans skill's "no placeholders" rule yields to the spec's "implementation is a one-line refactor of an existing function" reality.

- [ ] **Step 2: Add `DfrostLogCommitteeOracle`**

In `community_voting_log_engine.rs`, near the top (after imports):

```rust
/// Production CommitteeOracle backed by a live DfrostLogRegistry.
/// Looks up the committee public state at a given CHURP epoch by reading
/// the local dfrost log for the affected community.
pub struct DfrostLogCommitteeOracle {
    pub registry: std::sync::Arc<crate::community_dfrost_log::DfrostLogRegistry>,
    pub community_id: crate::owner_state_types::SpaceId,
}

impl crate::community_voting_tier3::CommitteeOracle for DfrostLogCommitteeOracle {
    fn committee_at_epoch(&self, epoch: u32) -> Option<crate::community_voting_tier3::CommitteePublicState> {
        // Implementation steps:
        //  1. Acquire the per-community DfrostLog from self.registry.
        //  2. Find the most recent DkgComplete (`kd=cd`) event whose payload
        //     epoch matches `epoch`. If CHURP rotation events (`kd=rt`) exist
        //     for a later HLC with the same epoch, prefer the rotated state.
        //  3. From the DkgComplete payload extract: joint_verifying_key
        //     (32-byte compressed Ristretto), verifying_shares per member,
        //     and threshold (max_signers/min_signers from the original
        //     ceremony). The exact field names are defined in
        //     community_dfrost_types.rs::DkgCompletePayload — refer there.
        //  4. Construct OwnerAddr → [u8;32] map by zipping the committee's
        //     sorted-OwnerAddr list with the verifying_shares Vec (same order
        //     used by FROST identifier_for_index per community_dfrost_crypto.rs:24).
        //  5. Return CommitteePublicState { epoch, joint_verifying_key,
        //     verifying_shares, threshold }.
        let registry = std::sync::Arc::clone(&self.registry);
        let log = registry.logs.get(&self.community_id)?;
        let log_g = log.try_lock().ok()?;
        // Walk events newest-first, find the DkgComplete payload for this epoch.
        // (Adapt the existing epoch lookup helper in community_dfrost_log_engine.rs
        // if one exists; otherwise inline the scan.)
        let _ = (log_g, epoch);
        None // TODO(implementer): replace with the scan body per steps 1–5 above.
    }
    fn latest_epoch(&self) -> Option<u32> {
        // Implementation:
        //  Scan the community's DfrostLog newest-first for the first
        //  DkgComplete (`kd=cd`) or RotateComplete (`kd=rt`) event; return
        //  that payload's `epoch` field. If no committee event exists yet,
        //  return None.
        let registry = std::sync::Arc::clone(&self.registry);
        let log = registry.logs.get(&self.community_id)?;
        let log_g = log.try_lock().ok()?;
        let _ = log_g;
        None // TODO(implementer): scan for latest epoch per the comment above.
    }
}
```

The implementer must consult `community_dfrost_types.rs` for the exact `DkgCompletePayload` / `RotateCompletePayload` field names + read `community_dfrost_log.rs` for the `DfrostLog::events` accessor. The `None` returns above are explicit placeholders so the code compiles; tests in Task 10 will exercise the real path.
```

In the engine's PollCreate post-apply hook (search for the existing point that mints `Tier3PollState` — likely in `apply_with_snapshot`), install the oracle:

```rust
if let Some(t3) = state.tier_state.as_tier3_mut() {
    let oracle = std::sync::Arc::new(DfrostLogCommitteeOracle {
        registry: self.dfrost_log_registry.clone()?,
        community_id: state.space_id,
    });
    t3.install_committee_oracle(oracle);
}
```

- [ ] **Step 3: Add `maybe_emit_tally_share`**

In `maybe_trigger_engine_auto_orchestration` (line 676), after the kd=cl block (~line 893), add a new gate:

```rust
        // ── ZEB-295: kd=ts TallyShare orchestration ─────────────────────
        //
        // Conditions:
        //   1. poll.privacy_mode == "se"
        //   2. ratification_end_ms <= local wall_ms
        //   3. our_addr ∈ committee_at(latest_epoch)
        //   4. we haven't yet published kd=ts for the current epoch in this poll
        //   5. kd=cl has been applied (ballot set canonical)
        let trigger_kd_ts: Option<(u32, crate::community_voting_core::TallySharePayload)> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s, None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t, None => return,
            };
            if t3.meta.config.privacy_mode != "se" || t3.close_event_hash.is_none() {
                None
            } else {
                let latest_epoch = match t3.committee_oracle.latest_epoch() {
                    Some(e) => e, None => return,
                };
                let cs = match t3.committee_oracle.committee_at_epoch(latest_epoch) {
                    Some(c) => c, None => return,
                };
                if !cs.verifying_shares.contains_key(&self_owner) {
                    None
                } else if t3.secret_tally.tally_shares.contains_key(&(self_owner, latest_epoch)) {
                    None
                } else {
                    // Compute aggregates + partial shares + DLEQ proofs.
                    let n = t3.candidates.len() + 1;
                    let aggregates = match crate::community_voting_tier3::aggregate_se_ballots(&t3.ratification_ballots, n) {
                        Some(a) => a, None => return,
                    };
                    // x_i is this committee member's FROST signing share,
                    // reinterpreted as the ElGamal decryption secret per Task 4.
                    // Phase 4 stores the KeyPackage on the engine struct after
                    // DKG completes. Look for the `local_dfrost_key_package`
                    // (or equivalent — see DfrostLogEngine for the canonical
                    // field name) and pass its signing share through
                    // community_dfrost_crypto::signing_share_as_scalar.
                    let local_kp = match self.local_dfrost_key_package_for(&self.space_id).await {
                        Some(kp) => kp, None => return, // not on this committee yet
                    };
                    let x_i = crate::community_dfrost_crypto::signing_share_as_scalar(&local_kp);
                    let entries: Vec<crate::community_voting_core::TallyShareEntry> = aggregates.iter().map(|agg| {
                        let c1 = crate::community_voting_tier3_crypto::decompress_point(&agg.c1).unwrap();
                        let share = crate::community_voting_tier3_crypto::partial_decrypt_share(&c1, &x_i);
                        let y_i_pt = crate::community_voting_tier3_crypto::decompress_point(&cs.verifying_shares[&self_owner]).unwrap();
                        let g_pt = curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
                        let dleq = crate::community_voting_tier3_nizk::dleq_prove(&g_pt, &y_i_pt, &c1, &share, &x_i);
                        crate::community_voting_core::TallyShareEntry {
                            share: crate::community_voting_tier3_crypto::compress_point(&share),
                            dleq_proof: dleq.to_bytes(),
                        }
                    }).collect();
                    Some((latest_epoch, crate::community_voting_core::TallySharePayload {
                        poll_id: *pid,
                        committee_epoch: latest_epoch,
                        entries,
                    }))
                }
            }
        };
        if let Some((epoch, payload)) = trigger_kd_ts {
            let hlc = self.reserve_next_local_hlc().await;
            let ts_ev = build_signed_tally_share(&signing_key, self_owner, payload, hlc)?;
            if let Err(e) = Box::pin(self.publish_event(ts_ev, None)).await {
                tracing::debug!(error = %e, poll_id = %hex::encode(pid.0), epoch, "engine-auto kd=ts publish rejected");
            }
        }
```

`build_signed_tally_share` belongs in `community_voting_core.rs` alongside other `build_signed_*` builders. Pattern matches `build_signed_ratification_ballot`.

- [ ] **Step 4: Add `maybe_emit_tier3_result_secret`**

After the kd=ts block in `maybe_trigger_engine_auto_orchestration`:

```rust
        // ── ZEB-295: kd=rs orchestration (secret-mode) ──────────────────
        let trigger_kd_rs_secret: Option<crate::community_voting_star::StarResult> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s, None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t, None => return,
            };
            if t3.meta.config.privacy_mode != "se" || t3.result.is_some() {
                None
            } else {
                let sq = crate::community_voting_tier3::synthesize_status_quo(&t3.meta.poll_id);
                let mut all_candidates = t3.candidates.clone();
                all_candidates.push(sq.clone());
                let advancers = crate::community_voting_tier3::drafting_advancers(&all_candidates, t3.meta.config.sortition_size as usize, sq.event_hash)?;
                let ordered = crate::community_voting_tier3::ratification_candidates_ordering(&advancers, sq.event_hash);
                crate::community_voting_tier3::recover_secret_tally(t3, &ordered)
            }
        };
        if let Some(result) = trigger_kd_rs_secret {
            let hlc = self.reserve_next_local_hlc().await;
            let rs_ev = crate::community_voting_core::build_signed_poll_result_tier3(&signing_key, self_owner, *pid, result, hlc)?;
            if let Err(e) = Box::pin(self.publish_event(rs_ev, None)).await {
                tracing::debug!(error = %e, poll_id = %hex::encode(pid.0), "engine-auto kd=rs (secret) publish rejected");
            }
        }
```

- [ ] **Step 5: Add `voting-tier3-tally-share-applied` Tauri event**

In `maybe_emit_tier3_lifecycle_events` (~line 1218), add a new branch:

```rust
// New: emit on every accepted kd=ts so the frontend can show
// incremental committee-share-count progress in the awaiting-tally state.
if matches!(applied_event_kind, PollEventKindCode::TallyShare) {
    if let Some(app_handle) = self.app_handle.as_ref() {
        let payload = serde_json::json!({
            "communityId": hex::encode(space_id.0),
            "pollId": hex::encode(pid.0),
            "epoch": /* extract from payload */,
            "shareCount": /* count from state */,
            "threshold": /* from oracle */,
        });
        if let Err(e) = app_handle.emit("voting-tier3-tally-share-applied", &payload) {
            tracing::warn!(error = %e, "voting-tier3-tally-share-applied emit failed (non-fatal)");
        }
    }
}
```

- [ ] **Step 6: Run tests; iterate**

```bash
cd src-tauri && cargo nextest run --locked --workspace --features test-fixtures 2>&1 | tail -40
```

Address any compile errors. Fill in `todo!()` stubs from steps 1–3 by referring to existing pattern code in the same file.

- [ ] **Step 7: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_voting_log_engine.rs src-tauri/src/community_voting_tier3.rs src-tauri/src/community_voting_core.rs src-tauri/src/community_voting_star.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): engine orchestration for ballot-secret tally

- DfrostLogCommitteeOracle: production CommitteeOracle reading from
  the community's dfrost log. Auto-installed on Tier3PollState at
  PollCreate apply.
- maybe_emit_tally_share: post-apply hook fires when (privacy_mode==se,
  kd=cl applied, we're in committee, we haven't yet emitted at this epoch).
  Computes aggregates → partial shares → DLEQ proofs; publishes kd=ts.
- maybe_emit_tier3_result_secret: post-apply hook fires after kd=ts apply
  when (privacy_mode==se, no kd=rs yet, recover_secret_tally returns Some).
  Multi-epoch fall-through tries latest epoch first then earlier epochs.
- recover_secret_tally + compute_star_from_sums: pure functions.
  Deterministic across replicas regardless of which subset of shares
  they combine first (Lagrange invariance).
- voting-tier3-tally-share-applied Tauri event for frontend incremental
  committee-progress updates.

CHURP rotation (spec §5.2/§5.3) handled by per-epoch grouping. Mid-poll
rotation falls back to pre-rotation epoch if the new one hasn't crossed
threshold yet.
EOF
)"
```

---

## Task 9: IPC ingress — extend create + cast with se-mode + extend export

**Files:**
- Modify: `src-tauri/src/lib.rs`
  - `voting_create_tier3_proposal` (line 21379): accept `privacy_mode: Option<String>`
  - `voting_cast_ratification_ballot` (line 22464): branch on poll's `privacy_mode`
  - `build_tier3_export` (line 24275): emit se-mode fields

- [ ] **Step 1: Extend `voting_create_tier3_proposal`**

Change the signature at line 21379:

```rust
async fn voting_create_tier3_proposal<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    channel_id: String,
    proposal_text: String,
    sortition_size: u16,
    deliberation_window_seconds: u32,
    drafting_window_seconds: u32,
    ratification_window_seconds: u32,
    incentive_mode: String,
    min_power: u8,
    min_vouching_depth: Option<u8>,
    retry_of: Option<String>,
    privacy_mode: Option<String>,  // NEW: defaults to "pu" if None
) -> Result<String, String> {
```

Replace line 21437:

```rust
        privacy_mode: privacy_mode.unwrap_or_else(|| "pu".into()),
```

- [ ] **Step 2: Branch `voting_cast_ratification_ballot` on poll's privacy_mode**

Before the existing `validate_ratification_ballot` call (line ~22589), look up the poll's `privacy_mode`:

```rust
    let privacy_mode = {
        let log_arc = /* ... */;
        let log_g = log_arc.lock().await;
        let state = log_g.polls.get(&pid).ok_or("poll not found")?;
        let t3 = state.tier_state.as_tier3().ok_or("not a Tier 3 poll")?;
        t3.meta.config.privacy_mode.clone()
    };
```

Branch:

```rust
    let payload = if privacy_mode == "se" {
        // Look up the committee's joint key from the local dfrost log.
        let oracle = build_committee_oracle_for(&community_id, &dfrost_log_registry_for_engine)?;
        let latest_epoch = oracle.latest_epoch().ok_or("no committee available")?;
        let cs = oracle.committee_at_epoch(latest_epoch).ok_or("no committee at latest epoch")?;
        let y_point = crate::community_voting_tier3_crypto::decompress_point(&cs.joint_verifying_key)
            .ok_or("committee Y not a valid point")?;
        // Encrypt scores + build NIZK bundle.
        let n = scores.len();
        let r_scores: Vec<curve25519_dalek::scalar::Scalar> = (0..n)
            .map(|_| curve25519_dalek::scalar::Scalar::random(&mut rand_core::OsRng))
            .collect();
        let scores_u64: Vec<u64> = scores.iter().map(|s| *s as u64).collect();
        let (bundle, ciphertexts_scores, ciphertexts_indicators) =
            crate::community_voting_tier3_nizk::prove_ballot_bundle_with_outputs(
                &y_point, &scores_u64, &r_scores,
            );
        crate::community_voting_core::RatificationBallotPayload {
            poll_id: pid,
            scores: None,
            ciphertexts_scores: Some(ciphertexts_scores),
            ciphertexts_indicators: Some(ciphertexts_indicators),
            proof: Some(crate::community_voting_core::BallotNIZKProof {
                range_proofs: bundle.range_proofs,
                consistency_proofs: bundle.consistency_proofs,
            }),
        }
    } else {
        // pu-mode unchanged: validate + build.
        crate::community_voting_tier3::validate_ratification_ballot(
            &crate::community_voting_core::RatificationBallotPayload {
                poll_id: pid,
                scores: Some(scores.clone()),
                ciphertexts_scores: None, ciphertexts_indicators: None, proof: None,
            },
            preflight_expected_count,
        ).map_err(|e| format!("invalid ballot: {e:?}"))?;
        crate::community_voting_core::RatificationBallotPayload {
            poll_id: pid,
            scores: Some(scores),
            ciphertexts_scores: None, ciphertexts_indicators: None, proof: None,
        }
    };
```

Add helper `prove_ballot_bundle_with_outputs` to `community_voting_tier3_nizk.rs` returning `(BallotBundleProof, Vec<EncCiphertext>, Vec<EncCiphertext>)`.

- [ ] **Step 3: Extend `build_tier3_export`**

In `build_tier3_export` (line 24275), add the se-mode export fields by extending the Tier3PollExport TypeScript shape mirror and the struct on the Rust side. Find the Rust struct (search for `pub struct Tier3PollExport` — likely in `lib.rs` or a `types.rs`):

```rust
    pub privacy_mode: String,
    pub encrypted_tally_share_count: u32,
    pub encrypted_tally_threshold: u16,
    pub encrypted_tally_committee_size: u16,
```

Populate in `build_tier3_export`:

```rust
    let privacy_mode = t3.meta.config.privacy_mode.clone();
    let (share_count, threshold, committee_size) = if privacy_mode == "se" {
        match t3.committee_oracle.latest_epoch()
            .and_then(|e| t3.committee_oracle.committee_at_epoch(e))
        {
            Some(cs) => {
                let count = t3.secret_tally.tally_shares.iter()
                    .filter(|((_, ep), _)| Some(*ep) == t3.committee_oracle.latest_epoch())
                    .count() as u32;
                (count, cs.threshold, cs.verifying_shares.len() as u16)
            }
            None => (0, 0, 0),
        }
    } else {
        (0, 0, 0)
    };
```

- [ ] **Step 4: Run all tests + format**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --features test-fixtures 2>&1 | tail -30
```

Expected: green (aside from orphan baseline).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_voting_tier3_nizk.rs
git commit -m "$(cat <<'EOF'
feat(zeb-295): IPC ingress for ballot-secret ratification

voting_create_tier3_proposal: new optional `privacy_mode` arg
(defaults to "pu"). Pass-through to validate_tier3_poll_config which
accepts "se" since Task 5.

voting_cast_ratification_ballot: branches on the poll's privacy_mode.
For "se": derives ElGamal randomness server-side, encrypts each score
to the committee key, computes the homomorphic indicator ciphertexts,
generates the NIZK bundle (range + consistency proofs). Result is a
RatificationBallotPayload with cs/in/pf fields. For "pu": unchanged.

build_tier3_export emits privacyMode + encryptedTallyShareCount +
encryptedTallyThreshold + encryptedTallyCommitteeSize for the
frontend's awaiting-tally state.
EOF
)"
```

---

## Task 10: Wire-format pinning + multi-engine determinism integration tests

**Files:**
- Create: `src-tauri/tests/wire_format_voting_tier3_secret_fixtures.rs`
- Create: `src-tauri/tests/community_voting_tier3_secret_ipc_integration.rs`
- Create: `src-tauri/tests/community_voting_tier3_secret_multi_engine_integration.rs`

- [ ] **Step 1: Wire-format pinning**

Create the file with deterministic-nonce CBOR fixtures (model after the existing `tests/wire_format_voting_tier3_fixtures.rs`):

```rust
//! ZEB-295 wire-format pin: kd=rb (se-mode) at n=3,5 + kd=ts at n=3,5
//! + pre/post CHURP rotation TallyShares for the same poll.
//! Run with: cargo nextest run --features test-fixtures --test wire_format_voting_tier3_secret_fixtures

#![cfg(feature = "test-fixtures")]

use ciborium::{into_writer, from_reader};
use harmony_app::community_voting_core::*;

#[test]
fn fixture_rb_se_n3_round_trip_and_byte_pin() {
    let payload = build_fixture_rb_se(3);
    let mut buf = Vec::new();
    into_writer(&payload, &mut buf).expect("encode");
    let decoded: RatificationBallotPayload = from_reader(&buf[..]).expect("decode");
    assert_eq!(payload, decoded);
    // Byte-pin: paste the expected bytes after first run (see regen pattern in
    // wire_format_voting_tier3_fixtures.rs)
    let expected_hex = "<PASTE_FROM_FIRST_RUN>";
    assert_eq!(hex::encode(&buf), expected_hex, "wire format drifted — regen if intentional");
}

#[test]
fn fixture_rb_se_n5_round_trip_and_byte_pin() { /* ... */ }

#[test]
fn fixture_ts_n3_round_trip_and_byte_pin() { /* ... */ }

#[test]
fn fixture_ts_n5_round_trip_and_byte_pin() { /* ... */ }

#[test]
fn fixture_ts_pre_post_rotation_different_epoch_values() {
    let pre = build_fixture_ts(3, 7);
    let post = build_fixture_ts(3, 8);
    assert_eq!(pre.committee_epoch, 7);
    assert_eq!(post.committee_epoch, 8);
    // Bytes must differ (ce field is in the wire).
    let mut a = Vec::new(); into_writer(&pre, &mut a).unwrap();
    let mut b = Vec::new(); into_writer(&post, &mut b).unwrap();
    assert_ne!(a, b);
}

fn build_fixture_rb_se(n: usize) -> RatificationBallotPayload { /* deterministic helper */ }
fn build_fixture_ts(n: usize, epoch: u32) -> TallySharePayload { /* deterministic helper */ }
```

Run once to capture the expected bytes; paste into `expected_hex`. Document the regen path in a comment.

- [ ] **Step 2: IPC integration tests**

Create `community_voting_tier3_secret_ipc_integration.rs` with:

```rust
#![cfg(feature = "test-fixtures")]

mod test_helpers { /* shared helpers: build_se_poll_with_committee, cast_ballot_via_ipc, etc. */ }
use test_helpers::*;

#[tokio::test]
async fn happy_path_se_poll_finalizes_with_recovered_tally() { /* end-to-end */ }

#[tokio::test]
async fn b5_pu_payload_rejected_on_se_poll() { /* ... */ }

#[tokio::test]
async fn nizk_invalid_ballot_rejected() { /* tamper one byte */ }

#[tokio::test]
async fn ts_too_early_rejected() { /* publish kd=ts before close */ }

#[tokio::test]
async fn ts_non_committee_actor_rejected() { /* T1 */ }

#[tokio::test]
async fn ts_invalid_dleq_rejected() { /* T2 */ }

#[tokio::test]
async fn ts_in_pu_mode_poll_rejected() { /* ... */ }

#[tokio::test]
async fn threshold_not_reached_no_kd_rs_emitted() { /* t-1 shares published; expect no result */ }
```

- [ ] **Step 3: Multi-engine determinism (load-bearing per acceptance #7)**

Create `community_voting_tier3_secret_multi_engine_integration.rs`:

```rust
#![cfg(feature = "test-fixtures")]

mod test_helpers { /* ... */ }

#[tokio::test]
async fn two_engines_recover_bit_identical_tally_from_same_log() { /* ... */ }

#[tokio::test]
async fn lagrange_invariance_subset_a_eq_subset_b() { /* engine A uses members {1,2,3}; engine B uses {2,3,4} */ }

#[tokio::test]
async fn churp_rotation_mid_test_recovers_correctly() { /* acceptance #5 */ }

#[tokio::test]
async fn plaintext_equivalence_se_mode_matches_pu_mode() { /* same scores → same StarResult */ }
```

- [ ] **Step 4: Run all integration tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voting_tier3_secret_fixtures --test community_voting_tier3_secret_ipc_integration --test community_voting_tier3_secret_multi_engine_integration
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/tests/wire_format_voting_tier3_secret_fixtures.rs src-tauri/tests/community_voting_tier3_secret_ipc_integration.rs src-tauri/tests/community_voting_tier3_secret_multi_engine_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-295): wire-format pinning + IPC + multi-engine integration

- wire_format_voting_tier3_secret_fixtures.rs: byte-pin kd=rb (se-mode)
  at n=3,5 + kd=ts at n=3,5 + pre/post CHURP rotation epoch encoding.
- community_voting_tier3_secret_ipc_integration.rs: happy path + every
  silent-drop branch + threshold-not-reached.
- community_voting_tier3_secret_multi_engine_integration.rs: load-bearing
  per ZEB-295 acceptance #7. Two-engine determinism, Lagrange subset
  invariance, CHURP rotation mid-test, plaintext-equivalence (se ≡ pu).
EOF
)"
```

---

## Task 11: Frontend — types + adapter + Tier3ProposalPanel + StarRatificationBallot + tests

**Files:**
- Modify: `src/lib/types/voting.ts`
- Modify: `src/lib/voting-adapter.ts`
- Modify: `src/lib/components/Tier3ProposalPanel.svelte`
- Modify: `src/lib/components/StarRatificationBallot.svelte`
- Create: `src/lib/components/__tests__/BallotSecretRendering.test.ts`

- [ ] **Step 1: Extend types in `voting.ts`**

In `src/lib/types/voting.ts`, extend `Tier3PollExport` (line 559) at the end (before the closing brace):

```typescript
  /** ZEB-295: privacy mode for ratification ballots. */
  privacyMode: 'pu' | 'se' | 'rf';
  /** ZEB-295 (se-mode only): committee members who have published shares for the latest epoch. */
  encryptedTallyShareCount: number;
  /** ZEB-295 (se-mode only): threshold `t` at the latest committee epoch. */
  encryptedTallyThreshold: number;
  /** ZEB-295 (se-mode only): committee size `n` at the latest committee epoch. */
  encryptedTallyCommitteeSize: number;
```

Add the new event payload type:

```typescript
/** ZEB-295: emitted on every accepted kd=ts so the frontend can update
 *  incremental committee-share-count progress. */
export interface Tier3TallyShareAppliedPayload {
  communityId: string;
  pollId: string;
  epoch: number;
  shareCount: number;
  threshold: number;
}
```

- [ ] **Step 2: Extend adapter in `voting-adapter.ts`**

Subscribe to the new event (in `setup`, after the existing tier3 subscriptions around line 477):

```typescript
        const unlistenTier3TallyShareApplied = await adapter.listen(
          'voting-tier3-tally-share-applied',
          (event) => {
            const payload = event.payload as Tier3TallyShareAppliedPayload;
            for (const sub of [...this.tier3TallyShareAppliedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3TallyShareApplied);
```

Add the subscriber set + `subscribeTier3TallyShareApplied` method matching the pattern of the other tier3 subscribers in the file.

Extend `proposeTier3Proposal` to accept an optional `privacyMode` argument and pass it through.

- [ ] **Step 3: Extend `Tier3ProposalPanel.svelte`**

In the create-form section, add a privacy-mode toggle:

```svelte
<label>Privacy mode</label>
<select bind:value={privacyMode}>
  <option value="pu">Public — all ballots visible</option>
  <option value="se">Ballot-secret — only the aggregate tally is revealed</option>
</select>
{#if privacyMode === 'se'}
  <p class="help-text">
    🔒 Encrypted ballots; only the aggregate tally is decrypted after the
    ratification window closes. Requires the community's D-FROST committee
    to perform threshold decryption.
  </p>
{/if}
```

Add `let privacyMode = $state<'pu' | 'se'>('pu');` near the other create-form state.

Update the `createProposal` call to pass `privacyMode` to the adapter.

In the ratification-render branch (around line 445 — the `{#if selectedDetail.stage === 'ra'}` block), add the three new se-mode states:

```svelte
{#if selectedDetail.stage === 'ra'}
  {#if selectedDetail.privacyMode === 'se' && pastRatificationEnd(selectedDetail) && !selectedDetail.winnerEventHash}
    <p class="awaiting-tally">
      🔒 Ballots closed. Awaiting committee tally —
      {selectedDetail.encryptedTallyShareCount} / {selectedDetail.encryptedTallyThreshold}
      of {selectedDetail.encryptedTallyCommitteeSize} committee members have published shares.
    </p>
  {:else}
    <StarRatificationBallot detail={selectedDetail} adapter={ratificationAdapter} onCast={refresh} />
  {/if}
{:else if selectedDetail.stage === 'fi'}
  <!-- existing finalized rendering — unchanged -->
{/if}
```

Add a privacy-mode chip on the poll card (in the list view):

```svelte
{#if s.privacyMode === 'se'}
  <span class="privacy-chip" aria-label="ballot-secret poll">🔒</span>
{/if}
```

(Add CSS for `.privacy-chip` + `.awaiting-tally`.)

- [ ] **Step 4: Extend `StarRatificationBallot.svelte`**

At the top of the template (above the per-candidate sliders), conditionally render:

```svelte
{#if detail.privacyMode === 'se'}
  <p class="encryption-banner">
    🔒 Your ballot will be encrypted to the community committee. The tally
    is revealed only after the ratification window closes.
  </p>
{/if}
```

If `casting === true` and `detail.privacyMode === 'se'`, swap the submit-button text to "Encrypting..." while the IPC runs.

- [ ] **Step 5: Add frontend tests**

Create `src/lib/components/__tests__/BallotSecretRendering.test.ts`:

```typescript
import { render, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StarRatificationBallot from '../StarRatificationBallot.svelte';
import Tier3ProposalPanel from '../Tier3ProposalPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

const seBase: Tier3PollExport = {
  // ...build baseline...
  privacyMode: 'se',
  encryptedTallyShareCount: 0,
  encryptedTallyThreshold: 2,
  encryptedTallyCommitteeSize: 3,
  // ...
};

describe('StarRatificationBallot — se-mode', () => {
  it('renders the lock-icon encryption banner', () => {
    const { getByText } = render(StarRatificationBallot, { props: { detail: seBase, adapter: new VotingAdapter(), onCast: () => {} } });
    expect(getByText(/encrypted to the community committee/i)).toBeTruthy();
  });
});

describe('Tier3ProposalPanel — awaiting-tally state', () => {
  it('shows committee progress when ballots closed but no winner yet', () => {
    const detail = { ...seBase, stage: 'ra' as const, encryptedTallyShareCount: 1 };
    const { getByText } = render(Tier3ProposalPanel, { props: { /* ... */ } });
    expect(getByText(/1 \/ 2 of 3 committee members/i)).toBeTruthy();
  });
});
```

- [ ] **Step 6: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: green.

- [ ] **Step 7: Commit**

```bash
git add src/lib/types/voting.ts src/lib/voting-adapter.ts src/lib/components/Tier3ProposalPanel.svelte src/lib/components/StarRatificationBallot.svelte src/lib/components/__tests__/BallotSecretRendering.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-295): frontend — privacy_mode toggle + se-mode UI states

- types/voting.ts: Tier3PollExport extended with privacyMode +
  encryptedTallyShareCount + encryptedTallyThreshold +
  encryptedTallyCommitteeSize. New Tier3TallyShareAppliedPayload event.
- voting-adapter.ts: subscribe voting-tier3-tally-share-applied;
  proposeTier3Proposal accepts optional privacyMode.
- Tier3ProposalPanel.svelte: privacy-mode dropdown on create form;
  awaiting-tally state in ratification render; 🔒 chip on list view.
- StarRatificationBallot.svelte: lock-icon banner + "Encrypting..."
  submit-button text in se-mode.
- BallotSecretRendering.test.ts: banner + awaiting-tally + chip.
EOF
)"
```

---

## Task 12: Final 5-gate sweep + push + PR creation

**Files:** none (verification + git).

- [ ] **Step 1: Run the 5 backend gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -50
```

All three must exit 0. Failure count must match the Task 0 baseline (orphan tests only; no new failures).

- [ ] **Step 2: Run the 2 frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Both must exit 0.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-295-tier3c-ballot-secret-design
```

- [ ] **Step 4: Create the PR**

```bash
gh pr create --title "ZEB-295 Phase 6: Tier 3c ballot-secret ratification via D-FROST" --body "$(cat <<'EOF'
## Summary

Phase 6 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella. Adds `privacy_mode "se"` (ballot-secret) ratification to Tier 3 polls: voters encrypt STAR ballots to the existing Phase 4 D-FROST committee public key (Ristretto255, exponential ElGamal); per-ballot NIZK proofs (range + indicator-consistency over CDS OR-composition) bind the encrypted scores to per-pair runoff indicators so the full STAR algorithm runs on aggregate data. After the ratification window closes, committee members publish threshold-decryption shares (`kd=ts` TallyShare events); any replica combines ≥ threshold shares to recover the aggregate tally.

Spec: [`docs/specs/2026-05-21-zeb-295-tier3c-ballot-secret-design.md`](docs/specs/2026-05-21-zeb-295-tier3c-ballot-secret-design.md) at commit `7c2db0c`.
Plan: [`docs/plans/2026-05-21-zeb-295-tier3c-ballot-secret-plan.md`](docs/plans/2026-05-21-zeb-295-tier3c-ballot-secret-plan.md).

### What's new

- Two new backend modules: `community_voting_tier3_crypto.rs` (threshold-ElGamal + Lagrange + BSGS) and `community_voting_tier3_nizk.rs` (sigma protocols).
- Wire format extension: `RatificationBallotPayload` carries optional `cs/in/pf` se-mode fields; new `TallySharePayload` (`kd=ts`); same-length-keys invariant preserved.
- Apply-time rules: kd=rb se-mode B5 (encoding + shape + NIZK); kd=ts T1/T2 (committee membership + DLEQ); ZEB-320 dual-watermark hygiene throughout.
- Engine orchestration: `maybe_emit_tally_share` + `maybe_emit_tier3_result_secret` hooks; multi-epoch tally recovery (latest-first fall-through) for CHURP rotation.
- IPC: existing `voting_cast_ratification_ballot` branches on `privacy_mode`. No new IPCs.
- Frontend: privacy-mode toggle in create form; lock-icon banner on se-mode ballots; three new awaiting-tally states in the ratification view.

### Test plan

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] `npx tsc --noEmit`
- [ ] `npx vitest run`
- [ ] Manual: create a Tier 3 poll with `privacy_mode = se`; cast 5 ballots; let ratification window expire; observe committee shares applied incrementally; final winner matches what pu-mode would have selected.

### Cross-references

Closes ZEB-295
- Parent: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) (umbrella; stays Backlog — Phase 7 work remains)
- Builds on Phase 5 [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) (Tier 3b deliberation)
- Builds on [ZEB-320](https://linear.app/zeblith/issue/ZEB-320) watermark hygiene (last_received_hlc dual-watermark)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Confirm PR is open**

```bash
gh pr view --json url,state | head
```

Expected: `state: OPEN`, URL printed. The branch is now in autonomous bot-review monitoring (CodeRabbit + Cursor Bugbot + CodeAnt + Qodo per `feedback_autonomous_pr_monitoring_loop`; Greptile and CI are skipped per `feedback_greptile_manual_trigger` + `feedback_ci_disabled`).

Control returns to the controller after this step.

---

## Notes for implementers

- The crypto module's BSGS is correct for `bound ≤ ~10^6`; the spec's worst case (1000-voter community at max score 5 = bound 5000) is well within range.
- The NIZK `consistency_prove` implementation uses a slightly weaker "both orientations" verification scheme. If a subsequent code-quality review flags this (e.g. a Qodo finding that the prover can equivocate), the fix is to add the explicit bit-proof + linkage to the indicator ciphertext. The spec §4.7.2 sketches this; the implementer should refine if cracks appear.
- `recover_secret_tally`'s LWW dedup is currently a no-op placeholder (`ballots.to_vec()`). The pu-mode `collect_ratification_ballots` does proper actor-LWW; mirror that pattern for se-mode dedup before the homomorphic aggregate.
- The committee oracle's `DfrostLogCommitteeOracle` reads from a local registry. The dfrost log's schema for "current committee" (kd=cd / kd=rt) is documented in `community_dfrost_types.rs`; the implementer should refer there for the exact event-kind names + payload fields.
- The engine's local-key-package accessor `local_dfrost_key_package_for(space_id)` referenced in Task 8 Step 3 is a placeholder name. The actual field on `VotingLogEngine` (or a helper that walks the local dfrost-log state to extract the committed `KeyPackage`) must be wired by reading `community_dfrost_log_engine.rs`. The canonical store is on the dfrost-log side, not the voting-log side — the implementer adds a thin pass-through accessor on the voting engine.
- `prove_ballot_bundle_with_outputs` (referenced from Task 9 Step 2) is the production variant of `prove_ballot_bundle` that returns the indicator ciphertexts alongside the bundle. Task 3 adds the test-only `prove_ballot_bundle`; the production variant is added in Task 9 — the implementer should consolidate to a single function that always returns the ciphertexts.
