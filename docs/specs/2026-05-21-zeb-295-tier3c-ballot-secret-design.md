# ZEB-295: Tier 3c — Ballot-Secret Ratification via D-FROST Tally (Design)

**Status:** Design (post-brainstorm, awaiting plan)
**Phase:** 6 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella
**Dependencies:** Phase 4 (Tier 3a sortition + STAR — shipped via PRs #148–#152), Phase 5 (Tier 3b deliberation — shipped via PR #153). Branches off `main` at `0bf89c3`.
**Spec for:** [ZEB-295](https://linear.app/zeblith/issue/ZEB-295)
**Umbrella spec:** [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](2026-05-16-zeb-289-voting-polling-design.md) §6.5 Mode A.

## Summary

Adds a `privacy_mode "se"` (ballot-secret) variant to Tier 3 ratification. Voters encrypt their STAR ballots to the community's existing Phase 4 D-FROST committee public key using exponential ElGamal in Ristretto255; per-ballot NIZK proofs bind score ciphertexts to per-pair runoff-indicator ciphertexts so the full STAR algorithm (score round + automatic runoff) can run on aggregate data. After the ratification window closes, committee members publish threshold-decryption shares (`kd=ts` TallyShare events); any replica combines ≥ threshold shares to recover the aggregate tally without recovering any individual ballot.

The community's existing D-FROST committee — already established for VRF beacons in Phase 4 and rotated via CHURP — is reused as-is. No per-poll DKG, no new key material; the same FROST `signing_share` is interpreted as the ElGamal decryption secret share. All cryptographic primitives layer over the existing `curve25519-dalek` Ristretto255 operations.

## Goals & non-goals

### Goals (priority order)

1. **Defeat passive observers and post-vote doxxing.** No replica or external observer (committee included, individually) can read individual ballots. Only the aggregate tally is revealed.
2. **Preserve full STAR semantics.** The recovered `StarResult` is bit-identical to what plaintext-mode STAR would have computed on the same scores. Both score round and automatic runoff are evaluated on the encrypted data.
3. **Deterministic multi-engine convergence.** Every replica recovers the same `StarResult` from the event log regardless of which subset of committee shares they combine first.
4. **Tolerate CHURP rotation during a poll.** Acceptance criterion #5: pre-rotation ballots remain decryptable after rotation; post-rotation shares can complete a stalled tally.
5. **Graceful committee unavailability.** Acceptance #6: <threshold available → tally stalls with a clear UI signal; recovers automatically when committee returns.

### Non-goals (v1)

- **Receipt-freeness.** Voter retains the per-ballot randomness `r` values; they can prove their ballot content to a coercer if they choose. Receipt-free mode is Phase 7 (TRIP via civic infrastructure).
- **Per-poll DKG.** The community committee key is reused across polls. Compromise of the committee within one CHURP rotation window affects all encrypted polls in that window.
- **Committee composition change ceremonies.** Adding/removing committee members is a community-governance concern, separate from CHURP rotation. Beyond-threshold composition change during a poll stalls the tally.
- **Cross-epoch share interpolation.** Shares from different CHURP epochs cannot be mixed; tally recovery uses shares from a single epoch.
- **Voter "prove your vote" UI.** Backend retains the randomness; surfacing it for verification is a future ticket.

## 1. Architecture overview

### Module placement

| Module | Purpose | Est. LoC |
|---|---|---|
| `community_voting_tier3_crypto.rs` (new) | Threshold-ElGamal: encrypt, partial-decrypt-share, Lagrange combine, baby-step-giant-step | ~400 |
| `community_voting_tier3_nizk.rs` (new) | Sigma protocols: bit / range-{0..5} / OR-composition / indicator-consistency / DLEQ; Fiat-Shamir transcripts | ~600 |
| `community_voting_core.rs` (extend) | `TallySharePayload` + `PollEventKindCode::TallyShare` ("ts"); extend `RatificationBallotPayload` with `cs/in/pf` optional fields | ~150 |
| `community_voting_tier3.rs` (extend) | `SecretTallyState` projection field on `Tier3PollState`; new apply branches for kd=ts; B5/T1/T2 verify rules; secret-mode `recover_secret_tally` | ~500 |
| `community_voting_log_engine.rs` (extend) | `maybe_emit_tally_share` + `maybe_emit_tier3_result_secret` engine-auto hooks | ~250 |
| `community_dfrost_crypto.rs` (extend) | Expose existing `KeyPackage.signing_share` and `verifying_shares` as threshold-ElGamal primitives | ~100 |
| `lib.rs` (extend) | No new IPCs — existing `voting_cast_ratification_ballot` branches on `privacy_mode` | ~50 |
| `src/lib/components/StarRatificationBallot.svelte` (extend) | Lock-icon banner + spinner during encryption | small |
| `src/lib/components/RatificationView.svelte` (extend) | Three new se-mode states (pre-close, awaiting-committee, decrypted) | small |
| `src/lib/components/Tier3ProposalCreateForm.svelte` (extend) | `privacy_mode` toggle | small |
| `src/lib/types/voting.ts` (extend) | `privacyMode`, `encryptedTallyShareCount`, `encryptedTallyThreshold`, `encryptedTallyCommitteeSize` fields on `Tier3PollExport` | small |

### Cryptographic primitive selection

- **Group:** Ristretto255 (curve25519-dalek `=4.1.3`, already in `Cargo.toml`).
- **Encryption scheme:** Exponential ElGamal — `m` encoded as `G·m` in the group so aggregation is homomorphic.
- **NIZK toolkit:** Hand-rolled sigma protocols over dalek + `merlin` for Strobe-based Fiat-Shamir transcripts. Auditable, ~700–900 LoC of crypto + ~1500 LoC of tests. The `bulletproofs` crate is rejected because most of its surface is unused for our small-range proofs; `zkp` crate is rejected because its macro-based approach hides the proven statements.
- **Committee key:** Reused from Phase 4. The FROST DKG's joint verifying key `Y = G·x` IS the ElGamal encryption key in the same group; FROST `signing_share` IS the per-member ElGamal decryption secret share `x_i`; FROST `verifying_share` IS `Y_i = G·x_i`.

## 2. Wire format

### 2.1 RatificationBallot (`kd=rb`) — overloaded for both modes

The existing Phase 4 payload is extended with optional encrypted-mode fields. Dispatch happens at apply time, gated by the poll's `privacy_mode`.

```text
RatificationBallotPayload {
  "pi": <PollId>,                    # unchanged (both modes)
  "sc": Option<[u8]>,                # present iff privacy_mode == "pu"; raw scores 0-5 per candidate
  "cs": Option<[EncCiphertext]>,     # present iff privacy_mode == "se"; len == candidates.len()
  "in": Option<[EncCiphertext]>,     # present iff privacy_mode == "se"; len == C(n,2) (unordered pairs, smaller-index-wins canonical orientation)
  "pf": Option<BallotNIZKProof>,     # present iff privacy_mode == "se"; per-ballot NIZK bundle
}
```

**Same-length-keys invariant maintained:** all top-level CBOR keys are 2 chars (`pi/sc/cs/in/pf`).

Why overload `kd=rb` rather than introducing `kd=eb`:
1. The umbrella spec reserved only `kd=ts`, not a new ballot kind.
2. Avoids forcing every downstream consumer (tally fn, projection, exports) to handle two parallel event lineages for "the same conceptual action" (casting a ratification ballot).

### 2.2 TallyShare (`kd=ts`)

```text
TallyShareEntry {
  "sh": [u8; 32],                    # partial decryption share d_i = c1_agg · x_i (compressed Ristretto)
  "dp": [u8; 64],                    # Chaum-Pedersen DLEQ proof (e, s) — proves d_i is computed with same x_i that produced Y_i
}

TallyShare {
  "pi": <PollId>,
  "ce": u32,                         # committee_epoch (CHURP rotation generation)
  "ts": Vec<TallyShareEntry>,        # length = n + C(n,2): n candidate score-sum entries first, then C(n,2) indicator-sum entries
}
```

One `kd=ts` event per committee member per poll per CHURP epoch. The `ts` array length is determined by the poll's `ordered_candidates.len()` (i.e., `n`); at the n=5 upper bound it holds `5 + 10 = 15` entries. The canonical ordering is: score sums for candidates `[0..n]` in poll order, then indicator sums for unordered pairs `(A, B)` with `A < B` in lexicographic `(A, B)` order.

Wire size at n=5: ≈ 1.5 KB per kd=ts event. Total tally-share footprint per poll ≈ `committee_size × 1.5 KB` ≈ ~10 KB.

### 2.3 EncCiphertext

```text
EncCiphertext {
  "c1": [u8; 32],                    # compressed Ristretto point: G·r
  "c2": [u8; 32],                    # compressed Ristretto point: G·m + Y·r
}
```

64 bytes per ciphertext.

### 2.4 BallotNIZKProof

```text
BallotNIZKProof {
  "rp": [u8; 384 * n],               # n range proofs over {0..5}, one per score ciphertext
  "ip": [u8; 768 * C(n,2)],          # C(n,2) consistency proofs, one per pair ciphertext
}
```

Encoding: each proof is a concatenation of fixed-size group elements + scalars per the sigma-protocol layout in §4. Sizes are deterministic given `n`.

Ballot size table:

| candidates | score ciphertexts | indicator ciphertexts | range proofs | consistency proofs | **total ballot** |
|------------|-------------------|------------------------|--------------|---------------------|---------------------|
| 2          | 128 B             | 64 B                   | 768 B        | 768 B               | ~1.7 KB             |
| 3          | 192 B             | 192 B                  | 1152 B       | 2304 B              | ~3.8 KB             |
| 5 (max)    | 320 B             | 640 B                  | 1920 B       | 7680 B              | ~10.6 KB            |

For a 1000-voter community at max candidates: ~10 MB ratification log. For a 10-million-member community: ~100 MB. Acceptable by today's storage standards.

## 3. Apply-time semantics

### 3.1 State projection — `Tier3PollState` extension

```rust
pub struct SecretTallyState {
    /// kd=ts events received. LWW upsert by (actor, committee_epoch):
    /// later (HLC, event_hash) replaces earlier for the same key.
    /// BTreeMap key for deterministic iteration during tally recovery.
    pub tally_shares: BTreeMap<(OwnerAddr, u32), TallyShareEntry>,
    /// None until ratification closes + ≥ threshold shares of a single
    /// epoch present + recovery succeeds. Set once via secret-mode
    /// tally recovery, then kd=rs publishes the same StarResult.
    pub decrypted_result: Option<StarResult>,
}

// Added field on Tier3PollState:
//     pub secret_tally: SecretTallyState,
```

The existing `ratification_ballots: Vec<RatificationBallotPayload>` is reused as-is — the overloaded payload now carries either `sc` (pu) or `cs/in/pf` (se). LWW dedup at tally time, not at apply time (matches the existing pu-mode pattern).

### 3.2 kd=rb apply rules (extension for `"se"` mode)

Order (silent-drop on failure, per Phase 5 §2.3 pattern; all drop paths set `advance_last_hlc = false` per ZEB-320):

1. **PayloadDecode** error → `Err(ApplyError::PayloadDecode)` (per existing pattern; not a silent drop).
2. **B5 (encoding-matches-privacy-mode):**
   - `privacy_mode == "pu"` → must have `sc`, must NOT have `cs/in/pf`.
   - `privacy_mode == "se"` → must have `cs/in/pf`, must NOT have `sc`.
   - Mismatch → silent-drop.
3. **Ciphertext-shape check (se-mode only):** `cs.len() == ordered_candidates.len()` AND `in.len() == n*(n-1)/2`. Wrong shape → silent-drop.
4. **NIZK proof verification (se-mode only):** Full bundle verifies (per-candidate range proofs + per-pair consistency proofs). Any sub-proof invalid → silent-drop.
5. **Existing checks (unchanged):** stage == Ratification at ev.hlc; actor in eligible_electorate_snapshot; LWW behavior at tally time.

On accept: append payload to `ratification_ballots`.

### 3.3 kd=ts apply rules (entirely new)

Order:

1. **PayloadDecode** error → `Err(ApplyError::PayloadDecode)`.
2. **Mode check:** `poll.privacy_mode == "se"`. If `"pu"` → silent-drop (tally shares meaningless for pu-mode polls).
3. **Timing:** `ev.hlc.wall_ms >= ratification_end_ms` (computed from `poll_create + dw + fw + rw`). Too-early shares → silent-drop.
4. **T1 (actor in committee at epoch `ce`):** lookup via `community_dfrost_log` projection. Not in committee → silent-drop.
5. **T2 (DLEQ proofs valid):** for each of the `n + C(n,2)` entries, verify the Chaum-Pedersen proof against `(G, Y_i, c1_aggregate, sh)` where `Y_i` is the verifying share at epoch `ce`. Any proof invalid → silent-drop (do NOT partial-accept).
6. **Ciphertext-shape check:** `ts.len() == n + C(n, 2)` where n is the poll's `ordered_candidates.len()` (matches the per-aggregate ordering from §2.2). Wrong length → silent-drop.

LWW upsert: insert/replace `(actor, ce) → TallyShareEntry` in `secret_tally.tally_shares` keyed by `(hlc, event_hash)` ordering — this handles a committee member re-publishing shares after a CHURP rotation (different `ce`, both coexist).

All silent-drop branches set `advance_last_hlc = false`.

### 3.4 Tally recovery (deterministic pure function)

```rust
pub fn recover_secret_tally(
    poll: &Tier3PollState,
    ordered_candidates: &[CandidateRef],
    committee_at_epoch: impl Fn(u32) -> CommitteePublicState,
    threshold_at_epoch: impl Fn(u32) -> u16,
) -> Option<StarResult> {
    // 1. Group received shares by epoch.
    let by_epoch: BTreeMap<u32, Vec<(OwnerAddr, TallyShareEntry)>> =
        group_shares_by_epoch(&poll.secret_tally.tally_shares);
    
    // 2. Try each epoch in descending order (latest first). Fall through
    //    to earlier epochs if the latest doesn't have threshold yet.
    for (epoch, shares) in by_epoch.iter().rev() {
        let t = threshold_at_epoch(*epoch);
        if shares.len() < t as usize { continue; }
        
        // 3. LWW dedup of ratification_ballots by actor (latest by HLC).
        let deduped_ballots = lww_dedup_ballots(&poll.ratification_ballots);
        
        // 4. Homomorphic aggregate: 5 candidate score-sum ciphertexts +
        //    C(n,2) indicator-sum ciphertexts. Pure point addition.
        let aggregates = aggregate_secret_ballots(&deduped_ballots, ordered_candidates.len());
        
        // 5. For each aggregate, Lagrange-interpolate over `t` shares.
        if let Some(decrypted) = combine_shares(&aggregates, shares, *epoch, t) {
            // 6. Baby-step-giant-step on each decrypted point to recover
            //    the integer sum; bound by electorate × max_score for scores,
            //    electorate for indicators.
            let score_sums = decrypted.score_sums.iter().map(bsgs_score).collect();
            let indicator_sums = decrypted.indicator_sums.iter().map(bsgs_count).collect();
            
            // 7. Compute STAR result from sums (same shape as existing tally_star).
            return Some(compute_star_from_sums(ordered_candidates, score_sums, indicator_sums));
        }
    }
    None
}
```

**Multi-engine determinism property:** Lagrange-interpolation over ANY size-`t` subset of `(i, x_i)` pairs from the SAME polynomial reconstructs the same secret. So replicas seeing different first-`t` shares from the same epoch converge on the same plaintext. Test 7.4 exercises this directly.

### 3.5 Stage projection — unchanged

`current_stage_at(now)` continues to be purely time-based via the existing function. The "awaiting committee tally" state is NOT a new `Stage` variant — it's UI-only: `current_stage_at == Ratification` AND `now > ratification_end` AND `decrypted_result.is_none()`.

Once the engine emits kd=rs (after successful tally recovery), the existing kd=rs apply path transitions the poll to `Stage::Finalized` — unchanged from Phase 4.

### 3.6 ZEB-320 watermark discipline

All new silent-drop paths (B5 mismatch, NIZK invalid, T1/T2 violations, mode/timing/shape errors) set `advance_last_hlc = false`. The new `last_received_hlc` field shipped in PR #154 continues to gate the monotonic-HLC guard so out-of-order delivery still surfaces as `HlcNotMonotonic`.

## 4. Cryptographic protocol

### 4.1 Group setting

- Group: Ristretto255 (curve25519-dalek standard basepoint `G`).
- Scalar field: `Z_q` where `q` is the Ristretto255 group order.
- Committee key: `Y = G · x` where `x` is Shamir-shared via FROST DKG across committee members at indices `i ∈ {1..n}`. Member `i` holds `x_i`. Reconstruction: `x = Σ_{i ∈ S} λ_i(0) · x_i` over any size-`t` subset `S`. No member ever reconstructs `x`.

### 4.2 Encryption (voter, per ballot value `m`)

```text
r  ← uniform Z_q                  # voter's per-ciphertext randomness
c1 = G · r
c2 = G · m + Y · r                # exponential ElGamal — m in the exponent
ciphertext = (c1, c2)             # 64 bytes
```

For score ciphertexts `m ∈ {0..5}`; for indicator ciphertexts `m ∈ {0,1}`.

Homomorphic-add property used by §3.4 step 4:

```text
Σ c1_j = G · (Σ r_j)
Σ c2_j = G · (Σ m_j) + Y · (Σ r_j)
→ ciphertext of (Σ m_j)
```

### 4.3 Threshold partial decryption

Each committee member, given aggregate `(c1_agg, c2_agg)`:

```text
d_i = c1_agg · x_i                # partial share (32-byte Ristretto point)
```

### 4.4 DLEQ proof of share validity (T2 — Chaum-Pedersen)

Prove knowledge of `x_i` such that **both** `Y_i = G · x_i` AND `d_i = c1_agg · x_i` (same exponent).

```text
Prover:
  k ← uniform Z_q
  A = G · k
  B = c1_agg · k
  e = H("harmony/v1/voting/tier3c/dleq" || G || Y_i || c1_agg || d_i || A || B)
  s = k + e · x_i (mod q)
  proof = (e, s)                  # 64 bytes

Verifier:
  A' = G · s - Y_i · e
  B' = c1_agg · s - d_i · e
  Accept iff e == H("harmony/v1/voting/tier3c/dleq" || G || Y_i || c1_agg || d_i || A' || B')
```

### 4.5 Lagrange combine (anyone, post ratification_end + ≥ t shares received)

```text
S = any size-t subset of {committee members with valid kd=ts at chosen epoch}
For each of the `n + C(n,2)` aggregate ciphertexts C_agg = (c1_agg, c2_agg):
  D = Σ_{i ∈ S} λ_i(0) · d_i
  m_point = c2_agg - D            # = G · (Σ m_j)
  m_sum = BSGS(m_point, bound)
```

Lagrange coefficient:

```text
λ_i(0) = ∏_{j ∈ S, j ≠ i} (-j) / (i - j)   (mod q)
```

### 4.6 Baby-step-giant-step (BSGS) for discrete log recovery

Given `P = G · m` and a bound `M`:
- Precompute baby steps: `{j → G · j}` for `j ∈ [0, √M]`
- Giant steps: search `P - G · (k · √M)` for `k ∈ [0, √M]` against the baby table.

Bounds:
- Score-sum bound: `electorate_size × 5` (≈ 5000 for typical communities)
- Indicator-sum bound: `electorate_size` (≈ 1000)
- Time/space: O(√M) — ~70 baby + 70 giant for M=5000. Microseconds.

The BSGS table can be precomputed once per `bound` and reused across all aggregates with the same bound (the n score-sum aggregates share one bound; the C(n,2) indicator-sum aggregates share another).

### 4.7 NIZK proof bundle (per encrypted ballot)

Three sub-protocols, all Fiat-Shamir-transformed sigma protocols composed via Cramer-Damgård-Schoenmakers (CDS) OR-composition.

#### 4.7.1 Score range proof — `c_score` encrypts `m ∈ {0..5}`

6-way OR-composition: "c encrypts 0 OR 1 OR 2 OR 3 OR 4 OR 5". For each branch `j ∈ {0..5}`, prove knowledge of `r` such that:
- `c1 = G · r`
- `c2 - G · j = Y · r`

This is an equality-of-discrete-logs proof on the same `r`. Per branch ≈ 64 bytes; 6 branches with CDS sharing the challenge → ~384 bytes per range proof, 5 per ballot.

Fiat-Shamir tag: `"harmony/v1/voting/tier3c/range5"`.

#### 4.7.2 Indicator-consistency proof — `c_indicator_AB` encrypts `b = [score_A > score_B]`

For each unordered pair `(A, B)` with `A < B` (smaller-index-wins canonical orientation), 2-way OR-composition:

- **Branch 1 (b = 1, score_A > score_B):**
  - `c_indicator_AB` encrypts `1`
  - `(score_A − score_B − 1) ∈ {0..4}` proven via range proof on the homomorphically-derived ciphertext `c_A − c_B − E(1)`
- **Branch 2 (b = 0, score_A ≤ score_B):**
  - `c_indicator_AB` encrypts `0`
  - `(score_B − score_A) ∈ {0..5}` proven via range proof on `c_B − c_A`

The score-difference ciphertexts are computed by the verifier from `c_A` and `c_B` (which are part of the ballot). Each branch is a bit-proof on the indicator + a range-proof on the derived difference. CDS OR-composition shares the challenge across branches. Per pair ≈ 768 bytes.

Fiat-Shamir tag: `"harmony/v1/voting/tier3c/cons"`.

#### 4.7.3 Bundle structure

```text
BallotNIZKProof {
  range_proofs: [RangeProof; n],                      # n score range proofs
  consistency_proofs: [ConsistencyProof; C(n,2)],     # C(n,2) pair consistency proofs
}
```

The bundle is verified atomically — any sub-proof failure → silent-drop (per §3.2 step 4).

### 4.8 Crate strategy

- `curve25519-dalek = "=4.1.3"` — already in `Cargo.toml`. Provides all point/scalar operations.
- `merlin = "3"` — to be added in the implementation PR. Strobe-based Fiat-Shamir transcripts with domain separation. Small crate, well-audited (used by dalek-cryptography ecosystem).
- `frost-ristretto255 = "3.0.0"` — already in `Cargo.toml`. Source of `KeyPackage.signing_share` (the `x_i` value) and `PublicKeyPackage.verifying_shares` (the `Y_i` values). No extensions to FROST itself.

Rolled by hand: all sigma protocols, CDS OR-composition, Chaum-Pedersen DLEQ, BSGS, exponential ElGamal. Auditable; rejection of `bulletproofs` (overkill for our small ranges) and `zkp` macros (hides the proven statements) detailed in §1.

### 4.9 Domain-separation tags

Every Fiat-Shamir transcript begins with a unique domain tag to prevent cross-protocol confusion:

- `"harmony/v1/voting/tier3c/dleq"` — Chaum-Pedersen DLEQ for share validity (T2)
- `"harmony/v1/voting/tier3c/range5"` — Score range proofs over {0..5}
- `"harmony/v1/voting/tier3c/cons"` — Indicator-vs-score consistency proofs
- `"harmony/v1/voting/tier3c/bundle"` — Per-ballot bundle (outer domain wrapping the above)

The domain tag is the first input to every Fiat-Shamir hash.

## 5. Engine-auto orchestration & CHURP rotation

### 5.1 Engine hooks

Two new post-apply hooks in `community_voting_log_engine.rs`, matching the existing kd=ss/kd=cl/kd=rs patterns from Phase 4:

#### `maybe_emit_tally_share`

Fires after kd=rb apply, kd=cl apply, kd=ss/sf apply, AND on each periodic timer tick. Local-side decision; each replica acts independently for itself.

Conditions to emit:
1. `poll.privacy_mode == "se"`
2. `ratification_end_ms ≤ now.wall_ms` (local clock past ratification end)
3. `our_addr ∈ committee_at(now)` (we are a current committee member)
4. We haven't yet published kd=ts for the current `committee_epoch` in this poll (lookup in `poll.secret_tally.tally_shares`)
5. The kd=cl close event has fired (so the ballot set is canonical at close)

On emit:
1. Compute `n + C(n,2)` aggregate ciphertexts from the deduped ballots (homomorphic sum).
2. Compute `n + C(n,2)` partial-decryption shares: `d_i = c1_agg · x_i`.
3. Compute `n + C(n,2)` Chaum-Pedersen DLEQ proofs (§4.4).
4. Sign and publish kd=ts via `engine.publish_event(...)`.

#### `maybe_emit_tier3_result_secret`

Fires after kd=ts apply. Local-side decision; each replica acts independently.

Conditions:
1. `poll.privacy_mode == "se"`
2. `recover_secret_tally(...).is_some()` (a single epoch has ≥ threshold shares AND tally recovers)
3. No kd=rs yet for this poll

On emit: sign and publish kd=rs with the recovered `StarResult`. The existing kd=rs apply path (unchanged from Phase 4) transitions to `Stage::Finalized`.

Race-condition handling: multiple replicas may race to publish kd=rs. Existing kd=rs apply-time gating ensures uniqueness (only the first wins; subsequent are dropped as duplicates).

### 5.2 CHURP rotation

CHURP refreshes the Shamir polynomial without changing the secret. Pre-rotation polynomial `f` and post-rotation `f'` both satisfy `f(0) = f'(0) = x`. The committee public key `Y = G·x` is stable across rotations.

**Key invariant:** shares from different polynomials cannot be mixed in Lagrange interpolation. Combining `(i, x_i)` from `f` with `(j, x_j')` from `f'` does NOT reconstruct `x`. Tally recovery must use shares from a single epoch.

This is the design rationale for the `ce: u32` field on `kd=ts` (introduced in §2.2): each kd=ts is tagged with the CHURP epoch that produced it. T1/T2 verify against the committee state at that specific epoch.

### 5.3 Multi-epoch tally recovery

Pseudocode in §3.4. The recovery function:
1. Groups received shares by epoch.
2. Iterates epochs in **descending order** (latest first — CHURP rotates forward; post-rotation shares are the "primary" answer).
3. For each epoch, if `shares.len() >= threshold_at_epoch(epoch)`, attempts recovery. Successful recovery returns `Some(StarResult)`; failed recovery (e.g., share-aggregation mismatch) falls through to the next epoch.

The fall-through handles the edge case where rotation happens mid-tally: the new committee may not yet have published a complete set, but the previous committee's pre-rotation shares may already satisfy the older epoch's threshold.

### 5.4 Failure mode — committee unavailability (ZEB-295 acceptance #6)

If no single epoch has ≥ threshold valid shares published:
- `recover_secret_tally` returns `None`
- `decrypted_result` stays `None`
- No kd=rs emitted
- Poll stays in `Stage::Ratification`
- UI surfaces "ballots closed, awaiting committee tally — `k/t` of `n` committee members have published shares" (per §6.3)

Recovery: when more committee members come online and publish kd=ts, the engine's `maybe_emit_tier3_result_secret` hook fires on each new kd=ts apply. Once a single epoch crosses threshold, tally recovers automatically.

### 5.5 Beyond-threshold composition change (out of scope for Phase 6)

If the committee composition changes (members leave) such that fewer than `t_old` pre-rotation members AND fewer than `t_new` post-rotation members remain, the poll stalls indefinitely. Recovery requires a separate community-governance ceremony (re-DKG with a new composition), which is out of scope for v1.

### 5.6 Timer-tick frequency

Engine already has a polling tick for time-based stage advancement (existing kd=cl auto-emit). The kd=ts emit hook piggybacks on the same tick — no new timer infrastructure. Tick interval is configurable but typically ~5–30 seconds, matching the existing pu-mode latency between ratification_end and kd=rs emission.

## 6. UI delta

### 6.1 Poll-create form

Extend `Tier3ProposalCreateForm.svelte` (or wherever the create form lives) to expose `privacy_mode`:

- Two-option toggle: **Public** (`"pu"`) | **Ballot-secret** (`"se"`).
- Third future option **Receipt-free** (`"rf"`) shown disabled with tooltip "Coming in Phase 7 (TRIP via civic infrastructure)".
- Default value sourced from `CommunityVotingPolicy.tier3_privacy_mode_default` (already in schema per umbrella spec §7).
- Tooltip on "Ballot-secret": "Encrypted ballots; only the aggregate tally is revealed. Requires the community's D-FROST committee to perform decryption after the window closes."

### 6.2 `StarRatificationBallot.svelte` — `"se"`-mode rendering

- Pre-submit indicator: small lock-icon banner "Your ballot will be encrypted to the community committee. The tally is revealed only after the ratification window closes."
- Submit path: existing IPC `voting_cast_ratification_ballot` reused. Backend branches on `poll.privacy_mode` — for `"se"` it invokes the new encryption + NIZK module, returns a hash receipt to the frontend (same shape as pu mode).
- Progress indicator: NIZK bundle generation takes ~100-500ms at max candidates (n=5). Subtle spinner while submitting; no separate UX state needed.
- Post-submit: same "ballot cast" confirmation as pu mode. No revealing the scores or receipt details to the UI (those exist in secure storage for the future "prove-your-vote" feature, out of scope for v1).

### 6.3 `RatificationView.svelte` — three new se-mode states

| `now` vs `ratification_end` | `decrypted_result` | UI state |
|------|------|------|
| `now < end` | `None` | Existing: "Cast your ballot" / "X ballots cast" (no scores shown) |
| `now ≥ end` | `None` | NEW: "Ballots closed. Awaiting committee tally — `k/t` of `n` committee members have published shares." |
| any | `Some(StarResult)` | Existing: Show winner + finalists + score totals (recovered tally, identical shape to pu mode) |

The middle state shows incremental committee progress — count of `tally_shares` BTreeMap entries (for the latest epoch) vs threshold. No timeline estimate (committee progress is unpredictable).

### 6.4 Privacy-mode badge

A `🔒 ballot-secret` chip rendered on the poll card in any list view (`CommunityProposalsPanel`, `Tier3ProposalPanel`, etc.) when `privacy_mode == "se"`. Minimal — a span with `aria-label="ballot-secret poll"`.

### 6.5 Committee-member view (optional polish)

No new component required. Engine-auto handles kd=ts publication transparently. Optional small indicator on the poll detail view: "✓ tally share published" when `myRole == "committee_member"` AND `tally_shares` contains our addr. Can ship as a follow-up.

### 6.6 `Tier3PollExport` extension

New fields in `src/lib/types/voting.ts`:

```typescript
type Tier3PollExport = {
  // ...existing fields...
  privacyMode: 'pu' | 'se' | 'rf';
  // se-mode-only fields (None / missing when pu):
  encryptedTallyShareCount: number;      // number of committee members who've published for latest epoch
  encryptedTallyThreshold: number;       // t (from committee config at latest epoch)
  encryptedTallyCommitteeSize: number;   // n
};
```

Backend builds these via the existing `build_tier3_export` projection. Pure read-only additive change to the wire DTO.

## 7. Testing

### 7.1 Unit tests (module-level `#[cfg(test)]`)

`community_voting_tier3_crypto.rs`:
- ElGamal encrypt/decrypt round-trip (single ciphertext, known message)
- Homomorphic add: `encrypt(m1) + encrypt(m2)` aggregates to `encrypt(m1 + m2)`
- Threshold combine: 2-of-3, 3-of-5, 4-of-7 committee subsets all recover the same plaintext
- BSGS correctness over bounded ranges (and rejection past the bound)
- Negative: tampered ciphertext fails decryption recovery

`community_voting_tier3_nizk.rs`:
- **Range proof on {0..5}:** honest prover passes for each `m ∈ {0..5}`; malicious prover with `m = 6` fails; deterministic given same nonces
- **Indicator-consistency proof:** honest passes for every `(score_A, score_B)` ∈ {0..5}² combination; malicious prover with mismatched indicator fails
- **DLEQ proof:** honest passes; tampered share fails; tampered `Y_i` fails
- **Composed bundle:** full 5-candidate ballot NIZK round-trip — honest passes; per-clause tampering fails (e.g., flip one indicator bit, change one score)

**Soundness sentinel:** a small set of "I tried to cheat" tests exercising each silent-drop edge.

### 7.2 Wire-format pinning (`tests/wire_format_voting_tier3_secret_fixtures.rs`)

Pin canonical CBOR for:
- Encrypted `RatificationBallot` at n=3 and n=5
- `TallyShare` at n=5 (1 committee member × 15 entries × DLEQ proofs); also one at n=3 (committee member × 6 entries) as a smaller fixture
- Pre-rotation and post-rotation `TallyShare` (same poll, different `ce`) to exercise the CHURP-epoch field

Regen pattern matches `wire_format_voting_tier3_fixtures.rs`. Comment in regen instructions: `--features test-fixtures --all-targets`.

### 7.3 IPC integration (`tests/community_voting_tier3_secret_ipc_integration.rs`)

- **Happy path:** create poll w/ `privacy_mode="se"`, drive sortition → deliberation → drafting → ratification, cast 5+ encrypted ballots, committee publishes shares, tally recovers, kd=rs emitted, recovered `StarResult` matches the expected.
- **Verify rule coverage** (one test per silent-drop branch):
  - B5: pu-mode ballot payload rejected when poll is se-mode (and vice versa)
  - kd=rb wrong ciphertext-shape rejected
  - kd=rb invalid NIZK proof rejected (per sub-proof: bad range, bad consistency, bad bit)
  - kd=ts too-early (before `ratification_end`) rejected
  - kd=ts from non-committee actor rejected (T1)
  - kd=ts invalid DLEQ proof rejected (T2)
  - kd=ts in pu-mode poll rejected
- **Threshold-not-reached:** only `t-1` committee shares published → `decrypted_result` stays `None`, no kd=rs; once the `t`-th arrives, kd=rs fires.

### 7.4 Multi-engine determinism (`tests/community_voting_tier3_secret_multi_engine_integration.rs`) — **load-bearing per ZEB-295 acceptance #7**

- Two independent `VotingLogEngine` instances apply the same event log (deliberately interleaved arrival order). Assert: bit-identical `Tier3PollExport`, bit-identical recovered `StarResult`.
- **Subset-of-shares variation:** engine A combines shares from members {1,2,3}, engine B combines from members {2,3,4}. Both recover the same plaintext (Lagrange invariance).
- **CHURP rotation mid-test** (acceptance #5): pre-rotation ballots cast → rotation event → post-rotation ballots cast → both pre- and post-rotation committee members publish shares → tally still recovers correctly.

### 7.5 Plaintext-equivalence test

Cast the same per-voter scores in two parallel polls (one `"pu"`, one `"se"`). Assert recovered `StarResult` equality. The "doesn't break STAR semantics" sentinel — ensures the Eager-STAR-with-NIZK path computes the same final outcome as the plaintext path.

### 7.6 Frontend tests (`src/lib/components/__tests__/`)

- `StarRatificationBallot.test.ts` — extends existing with se-mode lock-banner rendering + adapter mock for the encrypted path
- `RatificationView.test.ts` — three se-mode states (pre-close, awaiting-committee, decrypted)
- `Tier3ProposalCreateForm.test.ts` — `privacy_mode` toggle, default sourced from `CommunityVotingPolicy`
- Wire-DTO type assertions for the new `Tier3PollExport` fields

### 7.7 Performance sanity tests (not gates)

- Single-ballot NIZK bundle generation at n=5: wall-clock < 2s
- BSGS for `electorate_size = 10_000`: wall-clock < 100ms
- Full tally recovery for 1000 ballots: wall-clock < 5s

Not CI gates (machine-dependent); useful for catching `O(n²)`-or-worse regressions during development.

## 8. Acceptance criteria (mapped from ZEB-295)

| Ticket criterion | Spec section | Test |
|---|---|---|
| 1. Five CI gates green | (all) | implementation-time |
| 2. D-FROST committee performs both VRF and threshold-ElGamal | §4 | 7.1 (crypto round-trip) |
| 3. Encrypted ballot ciphertext deterministic per voter's randomness; no info leakage from size | §2.4 (fixed-size encoding) | 7.2 (wire fixture pinning) |
| 4. Tally recoverable from threshold of TallyShares; individual ballots NOT recoverable | §3.4, §4.5 | 7.3 (happy path), 7.4 (multi-engine) |
| 5. CHURP rotation preserves decryptability | §5.2, §5.3 | 7.4 (CHURP rotation mid-test) |
| 6. Committee churn beyond threshold → tally stalls gracefully, recovers on rejoin | §5.4 | 7.3 (threshold-not-reached) |
| 7. Multi-engine: encrypted poll converges on identical decrypted tally | §3.4 determinism property | 7.4 (load-bearing) |

## 9. Out of scope (deferred)

- Receipt-free ratification (`privacy_mode "rf"`) → Phase 7 ([ZEB-296](https://linear.app/zeblith/issue/ZEB-296))
- Voter "prove-your-vote" UI surfacing the randomness `r` values
- Per-poll DKG (community-wide committee reused; trade-off documented in goals/non-goals)
- Cross-epoch share interpolation (resharing protocols — distinct research area)
- Committee composition change ceremonies (community-governance concern; tally stalls if it happens mid-poll)
- Threshold-extension delegation mechanisms (a committee member delegating their share to a substitute)

## 10. References

- Umbrella spec: [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](2026-05-16-zeb-289-voting-polling-design.md) §6.5 (Mode A — Ballot-secret), §3 (wire format), §6.7 (IPC commands)
- Phase 4 D-FROST committee: ZEB-301 / ZEB-303 / ZEB-307 / ZEB-309 (shipped); `community_dfrost_*.rs` modules
- Phase 5 Tier 3b deliberation (shipped): [`docs/specs/2026-05-21-zeb-294-tier3-deliberation-design.md`](2026-05-21-zeb-294-tier3-deliberation-design.md) — pattern source for apply-time silent-drop semantics and engine-auto orchestration
- ZEB-320 watermark hygiene (shipped via PR #154): `last_received_hlc` vs `last_hlc` dual-watermark discipline
- Cramer-Damgård-Schoenmakers (1994), *"Proofs of Partial Knowledge and Simplified Design of Witness Hiding Protocols"* — CDS OR-composition technique
- Chaum-Pedersen (1992), *"Wallet Databases with Observers"* — DLEQ sigma protocol
- Shoup (2000), *"Practical Threshold Signatures"* — threshold-decryption protocol shape (informative; we use the simpler non-robust variant since adversarial committee members are out of v1 scope)
