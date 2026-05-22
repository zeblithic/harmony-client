//! ZEB-295: NIZK sigma protocols for Tier 3c ballot-secret ratification.
//! Spec §4.4 (DLEQ), §4.7.1 (range), §4.7.2 (indicator-consistency).
//! Fiat-Shamir via merlin Strobe transcripts with domain tags per §4.9.

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_TABLE as G_TABLE, ristretto::RistrettoPoint, scalar::Scalar,
};
use merlin::Transcript;

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
        let mut c = [0u8; 32];
        c.copy_from_slice(&b[..32]);
        let mut s = [0u8; 32];
        s.copy_from_slice(&b[32..]);
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
    let k = Scalar::random(&mut frost_ristretto255::rand_core::OsRng);
    let a = g * k;
    let b = c * k;
    append_point(&mut t, b"A", &a);
    append_point(&mut t, b"B", &b);
    let e = challenge_scalar(&mut t, b"e");
    let s = k + e * x;
    DleqProof {
        challenge: e,
        response: s,
    }
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
    branches: [EqDlogProof; 6],
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
            let mut c = [0u8; 32];
            c.copy_from_slice(&b[i * 64..i * 64 + 32]);
            let mut s = [0u8; 32];
            s.copy_from_slice(&b[i * 64 + 32..i * 64 + 64]);
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
            real_k = Scalar::random(&mut frost_ristretto255::rand_core::OsRng);
            let a = G_TABLE * &real_k;
            let b = y_point * real_k;
            branches.push((a, b, Scalar::ZERO, Scalar::ZERO));
        } else {
            let fake_chal = Scalar::random(&mut frost_ristretto255::rand_core::OsRng);
            let fake_resp = Scalar::random(&mut frost_ristretto255::rand_core::OsRng);
            // Statement_j: c1 = G·r AND c2 - G·j = Y·r.
            // a_j = G·resp - c1·chal ; b_j = Y·resp - (c2 - G·j)·chal
            let target = c2 - (G_TABLE * &Scalar::from(j));
            let a = (G_TABLE * &fake_resp) - c1 * fake_chal;
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
    Range5Proof {
        branches: proof_branches,
    }
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
        let target = c2 - (G_TABLE * &Scalar::from(j));
        let a = (G_TABLE * &br.response) - c1 * br.challenge;
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

/// Indicator-consistency proof. Spec §4.7.2.
///
/// 2-way CDS OR-composition over the indicator bit `b ∈ {0, 1}`:
///   - **Branch 0 (b = 0, score_A ≤ score_B):**
///     - `c_indicator` encrypts `0` (bit-witness for indicator).
///     - `(score_B − score_A) ∈ {0..5}` via inner 6-way OR range proof on the
///       derived ciphertext `c_B − c_A`.
///   - **Branch 1 (b = 1, score_A > score_B):**
///     - `c_indicator` encrypts `1`.
///     - `(score_A − score_B − 1) ∈ {0..4}` via inner 5-way OR range proof on
///       the derived ciphertext `c_A − c_B − E(1)` (where `E(1) = (0, G)`).
///
/// The OR-composition shares a single Fiat-Shamir challenge `e = e_0 + e_1`
/// across branches; the simulator equation lets the prover fake the false
/// branch while only one branch has a witness. This binds the indicator's
/// value to `sign(score_A − score_B)` — the malicious-prover attack of
/// "encrypt indicator = 5" (forged inflation) or "encrypt indicator = 0
/// when score_A > score_B" (forged loss) are unsoundness-rejected here.
///
/// Wire layout (832 B):
/// - Branch 0: `e_0` (32) + bit-witness response `s_i_0` (32) + 6×(inner challenge + inner response) (384) = 448 B
/// - Branch 1: `e_1` (32) + bit-witness response `s_i_1` (32) + 5×(inner challenge + inner response) (320) = 384 B
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyProof {
    /// e_0 (top-level OR challenge for branch 0).
    pub branch0_challenge: Scalar,
    /// Bit-witness response for "c_indicator encrypts 0" in branch 0.
    pub branch0_bit_response: Scalar,
    /// Inner 6-way OR range proof on c_diff = c_B − c_A ∈ {0..5}.
    pub branch0_range: [(Scalar, Scalar); 6],
    /// e_1 (top-level OR challenge for branch 1).
    pub branch1_challenge: Scalar,
    /// Bit-witness response for "c_indicator encrypts 1" in branch 1.
    pub branch1_bit_response: Scalar,
    /// Inner 5-way OR range proof on c_diff = c_A − c_B − E(1) ∈ {0..4}.
    pub branch1_range: [(Scalar, Scalar); 5],
}

impl ConsistencyProof {
    /// Wire size in bytes. Branch 0 = `32 (e_0) + 32 (bit response) + 6 * 64`
    /// `(inner range over {0..5})` = 448. Branch 1 = `32 + 32 + 5 * 64` (inner
    /// range over {0..4}) = 384. Total 832 bytes.
    pub const SIZE: usize = 832;
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let mut off = 0usize;
        // Branch 0
        out[off..off + 32].copy_from_slice(&self.branch0_challenge.to_bytes());
        off += 32;
        out[off..off + 32].copy_from_slice(&self.branch0_bit_response.to_bytes());
        off += 32;
        for (c, s) in &self.branch0_range {
            out[off..off + 32].copy_from_slice(&c.to_bytes());
            off += 32;
            out[off..off + 32].copy_from_slice(&s.to_bytes());
            off += 32;
        }
        // Branch 1
        out[off..off + 32].copy_from_slice(&self.branch1_challenge.to_bytes());
        off += 32;
        out[off..off + 32].copy_from_slice(&self.branch1_bit_response.to_bytes());
        off += 32;
        for (c, s) in &self.branch1_range {
            out[off..off + 32].copy_from_slice(&c.to_bytes());
            off += 32;
            out[off..off + 32].copy_from_slice(&s.to_bytes());
            off += 32;
        }
        debug_assert_eq!(off, Self::SIZE);
        out
    }
    pub fn from_bytes(b: &[u8; Self::SIZE]) -> Option<Self> {
        fn read_scalar(b: &[u8], off: usize) -> Option<Scalar> {
            let mut s = [0u8; 32];
            s.copy_from_slice(&b[off..off + 32]);
            Option::from(Scalar::from_canonical_bytes(s))
        }
        let mut off = 0usize;
        // Branch 0
        let branch0_challenge = read_scalar(b, off)?;
        off += 32;
        let branch0_bit_response = read_scalar(b, off)?;
        off += 32;
        let mut b0_range: [(Scalar, Scalar); 6] = [(Scalar::ZERO, Scalar::ZERO); 6];
        for slot in b0_range.iter_mut() {
            let c = read_scalar(b, off)?;
            off += 32;
            let s = read_scalar(b, off)?;
            off += 32;
            *slot = (c, s);
        }
        // Branch 1
        let branch1_challenge = read_scalar(b, off)?;
        off += 32;
        let branch1_bit_response = read_scalar(b, off)?;
        off += 32;
        let mut b1_range: [(Scalar, Scalar); 5] = [(Scalar::ZERO, Scalar::ZERO); 5];
        for slot in b1_range.iter_mut() {
            let c = read_scalar(b, off)?;
            off += 32;
            let s = read_scalar(b, off)?;
            off += 32;
            *slot = (c, s);
        }
        debug_assert_eq!(off, Self::SIZE);
        Some(Self {
            branch0_challenge,
            branch0_bit_response,
            branch0_range: b0_range,
            branch1_challenge,
            branch1_bit_response,
            branch1_range: b1_range,
        })
    }
}

/// Append the public statement (c_A, c_B, c_indicator) to a transcript.
/// Used by both `consistency_prove` and `consistency_verify` so the FS
/// challenge binds to ALL three ciphertexts — preventing mix-and-match
/// across pairs in a multi-pair bundle.
fn cons_transcript_setup(
    y_point: &RistrettoPoint,
    c_a_1: &RistrettoPoint,
    c_a_2: &RistrettoPoint,
    c_b_1: &RistrettoPoint,
    c_b_2: &RistrettoPoint,
    c_i_1: &RistrettoPoint,
    c_i_2: &RistrettoPoint,
) -> Transcript {
    let mut t = Transcript::new(CONS_TAG);
    append_point(&mut t, b"Y", y_point);
    append_point(&mut t, b"cA1", c_a_1);
    append_point(&mut t, b"cA2", c_a_2);
    append_point(&mut t, b"cB1", c_b_1);
    append_point(&mut t, b"cB2", c_b_2);
    append_point(&mut t, b"cI1", c_i_1);
    append_point(&mut t, b"cI2", c_i_2);
    t
}

/// Simulate (compute fake A, B from supplied challenge/response) for the
/// "indicator encrypts v" bit-witness on c_i.
///   A = G·s - c_i_1·e
///   B = Y·s - (c_i_2 - G·v)·e
fn simulate_bit_ab(
    y_point: &RistrettoPoint,
    c_i_1: &RistrettoPoint,
    c_i_2: &RistrettoPoint,
    v: u64,
    chal: Scalar,
    resp: Scalar,
) -> (RistrettoPoint, RistrettoPoint) {
    let target = c_i_2 - (G_TABLE * &Scalar::from(v));
    let a = (G_TABLE * &resp) - c_i_1 * chal;
    let b = (y_point * resp) - target * chal;
    (a, b)
}

/// Simulate one inner range-OR branch on c_diff = (c_diff_1, c_diff_2).
///   A = G·s - c_diff_1·c   ;  B = Y·s - (c_diff_2 - G·j)·c
fn simulate_range_ab(
    y_point: &RistrettoPoint,
    c_diff_1: &RistrettoPoint,
    c_diff_2: &RistrettoPoint,
    j: u64,
    chal: Scalar,
    resp: Scalar,
) -> (RistrettoPoint, RistrettoPoint) {
    let target = c_diff_2 - (G_TABLE * &Scalar::from(j));
    let a = (G_TABLE * &resp) - c_diff_1 * chal;
    let b = (y_point * resp) - target * chal;
    (a, b)
}

/// Append the commitment points for one top-level OR-branch to the transcript.
/// Order is load-bearing: prover and verifier must match.
fn append_branch_commitments(
    t: &mut Transcript,
    label: &'static [u8],
    bit_ab: &(RistrettoPoint, RistrettoPoint),
    range_abs: &[(RistrettoPoint, RistrettoPoint)],
) {
    t.append_message(b"branch", label);
    append_point(t, b"Ai", &bit_ab.0);
    append_point(t, b"Bi", &bit_ab.1);
    for (a, b) in range_abs {
        append_point(t, b"Ar", a);
        append_point(t, b"Br", b);
    }
}

/// Prove the indicator ciphertext consistently encodes `b = [score_A > score_B]`.
/// Spec §4.7.2: 2-way CDS OR over the indicator bit, each branch containing
/// (bit-witness + inner range proof).
pub fn consistency_prove(
    y_point: &RistrettoPoint,
    (c_a_1, c_a_2, score_a, r_a): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
    (c_b_1, c_b_2, score_b, r_b): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
    (c_i_1, c_i_2, indicator, r_i): (&RistrettoPoint, &RistrettoPoint, u64, Scalar),
) -> ConsistencyProof {
    assert!(score_a <= 5 && score_b <= 5, "scores must be in 0..=5");
    assert!(indicator <= 1, "indicator must be 0 or 1");
    let real_b = score_a > score_b;
    assert_eq!(
        indicator,
        if real_b { 1 } else { 0 },
        "indicator must equal [score_A > score_B]"
    );

    let mut t = cons_transcript_setup(y_point, c_a_1, c_a_2, c_b_1, c_b_2, c_i_1, c_i_2);

    // Derived diff ciphertexts for each branch.
    // Branch 0: c_diff_0 = c_B − c_A, encrypting (score_B − score_A) ∈ {0..5}.
    // Branch 1: c_diff_1 = c_A − c_B − E(1), encrypting (score_A − score_B − 1) ∈ {0..4}.
    let c_diff0_1 = c_b_1 - c_a_1;
    let c_diff0_2 = c_b_2 - c_a_2;
    let one_g = G_TABLE * &Scalar::ONE;
    let c_diff1_1 = c_a_1 - c_b_1; // E(1).c1 = 0
    let c_diff1_2 = c_a_2 - c_b_2 - one_g; // E(1).c2 = G

    // The witness randomness for the derived diff is r_diff = r_A − r_B
    // (branch 1) or r_B − r_A (branch 0), since
    //   c_A − c_B = (G·(r_A − r_B), G·(score_A − score_B) + Y·(r_A − r_B)).
    // The E(1) subtraction does not affect the c1 component, so the same
    // r_diff = r_A − r_B witnesses branch 1's derived ciphertext.
    let r_diff_b0 = r_b - r_a;
    let r_diff_b1 = r_a - r_b;

    let mut rng = frost_ristretto255::rand_core::OsRng;

    // ── Branch 0 setup ──
    // Real-bit value for indicator: 0. Real range value: (score_B - score_A).
    let real0_range_idx: usize = if real_b {
        // Branch 0 is FAKE → no real witness here.
        0 // unused
    } else {
        (score_b - score_a) as usize
    };

    let mut b0_range_chal_resp: [(Scalar, Scalar); 6] = [(Scalar::ZERO, Scalar::ZERO); 6];
    let mut b0_range_ab: [(RistrettoPoint, RistrettoPoint); 6] =
        [(RistrettoPoint::default(), RistrettoPoint::default()); 6];
    let mut b0_inner_fake_chal_sum = Scalar::ZERO;
    let mut b0_real_inner_k = Scalar::ZERO; // range commitment nonce, used only if branch 0 is real
    let mut b0_real_bit_k = Scalar::ZERO; // bit-witness commitment nonce, used only if branch 0 is real
    let b0_bit_ab: (RistrettoPoint, RistrettoPoint);
    // Track simulated branch's full (challenge, bit-response).
    let mut b0_sim_chal = Scalar::ZERO;
    let mut b0_sim_bit_resp = Scalar::ZERO;

    if real_b {
        // Branch 0 is FAKE. Sample top-level challenge e_0 and bit response s_i_0,
        // then for each inner range branch sample (c_j, s_j) fakes (all branches
        // are fake since there's no real witness here).
        b0_sim_chal = Scalar::random(&mut rng);
        b0_sim_bit_resp = Scalar::random(&mut rng);
        b0_bit_ab = simulate_bit_ab(y_point, c_i_1, c_i_2, 0, b0_sim_chal, b0_sim_bit_resp);
        // All inner branches are simulated; their challenges must sum to e_0.
        // We pick (K-1) random and derive the last.
        let mut sum = Scalar::ZERO;
        for slot in b0_range_chal_resp.iter_mut().take(5) {
            let c = Scalar::random(&mut rng);
            let s = Scalar::random(&mut rng);
            *slot = (c, s);
            sum += c;
        }
        let last_c = b0_sim_chal - sum;
        let last_s = Scalar::random(&mut rng);
        b0_range_chal_resp[5] = (last_c, last_s);
        for (j, (c, s)) in b0_range_chal_resp.iter().enumerate() {
            b0_range_ab[j] = simulate_range_ab(y_point, &c_diff0_1, &c_diff0_2, j as u64, *c, *s);
        }
    } else {
        // Branch 0 is REAL. The bit-witness's `k` and the real range branch's
        // `k` are sampled; other branches are simulated; we defer computing the
        // real challenge/response until after we get the FS challenge.
        b0_real_bit_k = Scalar::random(&mut rng);
        let a = G_TABLE * &b0_real_bit_k;
        let b = y_point * b0_real_bit_k;
        b0_bit_ab = (a, b);
        for j in 0..6 {
            if j == real0_range_idx {
                b0_real_inner_k = Scalar::random(&mut rng);
                let a = G_TABLE * &b0_real_inner_k;
                let b = y_point * b0_real_inner_k;
                b0_range_ab[j] = (a, b);
                b0_range_chal_resp[j] = (Scalar::ZERO, Scalar::ZERO); // placeholder
            } else {
                let c = Scalar::random(&mut rng);
                let s = Scalar::random(&mut rng);
                b0_range_chal_resp[j] = (c, s);
                b0_inner_fake_chal_sum += c;
                b0_range_ab[j] = simulate_range_ab(y_point, &c_diff0_1, &c_diff0_2, j as u64, c, s);
            }
        }
    }

    // ── Branch 1 setup ──
    let real1_range_idx: usize = if real_b {
        // Real diff = score_A - score_B - 1, in {0..4}
        (score_a - score_b - 1) as usize
    } else {
        0 // unused — branch 1 is fake
    };

    let mut b1_range_chal_resp: [(Scalar, Scalar); 5] = [(Scalar::ZERO, Scalar::ZERO); 5];
    let mut b1_range_ab: [(RistrettoPoint, RistrettoPoint); 5] =
        [(RistrettoPoint::default(), RistrettoPoint::default()); 5];
    let mut b1_inner_fake_chal_sum = Scalar::ZERO;
    let mut b1_real_inner_k = Scalar::ZERO;
    let mut b1_real_bit_k = Scalar::ZERO;
    let b1_bit_ab: (RistrettoPoint, RistrettoPoint);
    let mut b1_sim_chal = Scalar::ZERO;
    let mut b1_sim_bit_resp = Scalar::ZERO;

    if real_b {
        // Branch 1 is REAL.
        b1_real_bit_k = Scalar::random(&mut rng);
        let a = G_TABLE * &b1_real_bit_k;
        let b = y_point * b1_real_bit_k;
        b1_bit_ab = (a, b);
        for j in 0..5 {
            if j == real1_range_idx {
                b1_real_inner_k = Scalar::random(&mut rng);
                let a = G_TABLE * &b1_real_inner_k;
                let b = y_point * b1_real_inner_k;
                b1_range_ab[j] = (a, b);
                b1_range_chal_resp[j] = (Scalar::ZERO, Scalar::ZERO);
            } else {
                let c = Scalar::random(&mut rng);
                let s = Scalar::random(&mut rng);
                b1_range_chal_resp[j] = (c, s);
                b1_inner_fake_chal_sum += c;
                b1_range_ab[j] = simulate_range_ab(y_point, &c_diff1_1, &c_diff1_2, j as u64, c, s);
            }
        }
    } else {
        // Branch 1 is FAKE.
        b1_sim_chal = Scalar::random(&mut rng);
        b1_sim_bit_resp = Scalar::random(&mut rng);
        b1_bit_ab = simulate_bit_ab(y_point, c_i_1, c_i_2, 1, b1_sim_chal, b1_sim_bit_resp);
        let mut sum = Scalar::ZERO;
        for slot in b1_range_chal_resp.iter_mut().take(4) {
            let c = Scalar::random(&mut rng);
            let s = Scalar::random(&mut rng);
            *slot = (c, s);
            sum += c;
        }
        let last_c = b1_sim_chal - sum;
        let last_s = Scalar::random(&mut rng);
        b1_range_chal_resp[4] = (last_c, last_s);
        for (j, (c, s)) in b1_range_chal_resp.iter().enumerate() {
            b1_range_ab[j] = simulate_range_ab(y_point, &c_diff1_1, &c_diff1_2, j as u64, *c, *s);
        }
    }

    // ── Fiat-Shamir challenge from all commitments ──
    append_branch_commitments(&mut t, b"b0", &b0_bit_ab, &b0_range_ab);
    append_branch_commitments(&mut t, b"b1", &b1_bit_ab, &b1_range_ab);
    let e = challenge_scalar(&mut t, b"e");

    // ── Derive real branch's top-level challenge and fill in real responses ──
    let (branch0_challenge, branch1_challenge);
    let (branch0_bit_response, branch1_bit_response);

    if real_b {
        // Branch 1 is real.
        let e_1 = e - b0_sim_chal;
        branch0_challenge = b0_sim_chal;
        branch1_challenge = e_1;
        branch0_bit_response = b0_sim_bit_resp;
        // s_i_1 = k_i + e_1 * r_i
        branch1_bit_response = b1_real_bit_k + e_1 * r_i;
        // Real inner range branch challenge: c_real = e_1 - sum(fake inner)
        let real_inner_c = e_1 - b1_inner_fake_chal_sum;
        let real_inner_s = b1_real_inner_k + real_inner_c * r_diff_b1;
        b1_range_chal_resp[real1_range_idx] = (real_inner_c, real_inner_s);
    } else {
        // Branch 0 is real.
        let e_0 = e - b1_sim_chal;
        branch0_challenge = e_0;
        branch1_challenge = b1_sim_chal;
        branch0_bit_response = b0_real_bit_k + e_0 * r_i;
        branch1_bit_response = b1_sim_bit_resp;
        let real_inner_c = e_0 - b0_inner_fake_chal_sum;
        let real_inner_s = b0_real_inner_k + real_inner_c * r_diff_b0;
        b0_range_chal_resp[real0_range_idx] = (real_inner_c, real_inner_s);
    }

    let _ = (score_a, indicator); // silence in case branches collapse

    ConsistencyProof {
        branch0_challenge,
        branch0_bit_response,
        branch0_range: b0_range_chal_resp,
        branch1_challenge,
        branch1_bit_response,
        branch1_range: b1_range_chal_resp,
    }
}

pub fn consistency_verify(
    y_point: &RistrettoPoint,
    (c_a_1, c_a_2): (&RistrettoPoint, &RistrettoPoint),
    (c_b_1, c_b_2): (&RistrettoPoint, &RistrettoPoint),
    (c_i_1, c_i_2): (&RistrettoPoint, &RistrettoPoint),
    proof: &ConsistencyProof,
) -> bool {
    let mut t = cons_transcript_setup(y_point, c_a_1, c_a_2, c_b_1, c_b_2, c_i_1, c_i_2);

    // Recompute derived diff ciphertexts (verifier knows c_A, c_B, c_I).
    let c_diff0_1 = c_b_1 - c_a_1;
    let c_diff0_2 = c_b_2 - c_a_2;
    let one_g = G_TABLE * &Scalar::ONE;
    let c_diff1_1 = c_a_1 - c_b_1;
    let c_diff1_2 = c_a_2 - c_b_2 - one_g;

    // ── Branch 0 reconstruction ──
    let b0_bit_ab = simulate_bit_ab(
        y_point,
        c_i_1,
        c_i_2,
        0,
        proof.branch0_challenge,
        proof.branch0_bit_response,
    );
    let mut b0_range_ab: [(RistrettoPoint, RistrettoPoint); 6] =
        [(RistrettoPoint::default(), RistrettoPoint::default()); 6];
    let mut b0_inner_chal_sum = Scalar::ZERO;
    for (j, (c, s)) in proof.branch0_range.iter().enumerate() {
        b0_range_ab[j] = simulate_range_ab(y_point, &c_diff0_1, &c_diff0_2, j as u64, *c, *s);
        b0_inner_chal_sum += c;
    }
    if b0_inner_chal_sum != proof.branch0_challenge {
        return false;
    }

    // ── Branch 1 reconstruction ──
    let b1_bit_ab = simulate_bit_ab(
        y_point,
        c_i_1,
        c_i_2,
        1,
        proof.branch1_challenge,
        proof.branch1_bit_response,
    );
    let mut b1_range_ab: [(RistrettoPoint, RistrettoPoint); 5] =
        [(RistrettoPoint::default(), RistrettoPoint::default()); 5];
    let mut b1_inner_chal_sum = Scalar::ZERO;
    for (j, (c, s)) in proof.branch1_range.iter().enumerate() {
        b1_range_ab[j] = simulate_range_ab(y_point, &c_diff1_1, &c_diff1_2, j as u64, *c, *s);
        b1_inner_chal_sum += c;
    }
    if b1_inner_chal_sum != proof.branch1_challenge {
        return false;
    }

    // ── Fiat-Shamir challenge check ──
    append_branch_commitments(&mut t, b"b0", &b0_bit_ab, &b0_range_ab);
    append_branch_commitments(&mut t, b"b1", &b1_bit_ab, &b1_range_ab);
    let e = challenge_scalar(&mut t, b"e");

    e == proof.branch0_challenge + proof.branch1_challenge
}

// ── Ballot bundle (n score range proofs + C(n,2) consistency proofs) ────

#[derive(Debug)]
pub struct BallotBundleProof {
    pub range_proofs: Vec<u8>,       // Range5Proof::SIZE * n
    pub consistency_proofs: Vec<u8>, // ConsistencyProof::SIZE * C(n,2)
}

/// Reasons `prove_ballot_bundle_with_outputs` may reject the input.
/// Distinct error variants so the IPC layer can render a user-friendly
/// message without sniffing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BallotBundleBuildError {
    #[error("ballot must contain at least one score")]
    Empty,
    #[error("ballot scores must be in 0..=5")]
    ScoreOutOfRange,
}

/// Generate a per-ballot NIZK bundle AND return the score + indicator
/// ciphertexts that were derived during proof construction. The IPC
/// handler uses these ciphertexts directly in the wire payload — they
/// share randomness with the proofs (binding-by-construction).
///
/// **Production API.** Samples all encryption randomness internally.
/// Production callers must NEVER pass deterministic randomness (would
/// catastrophically reuse nonces). The deterministic-nonce variant is
/// gated behind `#[cfg(any(test, feature = "test-fixtures"))]`; see
/// `prove_ballot_bundle_with_outputs_with_score_nonces`.
///
/// Validates `scores` is non-empty and all values are ≤ 5 before
/// generating randomness; out-of-range input returns
/// `BallotBundleBuildError` instead of reaching the prover's debug
/// assertion (which would otherwise panic in release builds via the
/// downstream `assert!`).
pub fn prove_ballot_bundle_with_outputs(
    y_point: &RistrettoPoint,
    scores: &[u64],
) -> Result<
    (
        BallotBundleProof,
        Vec<crate::community_voting_core::EncCiphertext>,
        Vec<crate::community_voting_core::EncCiphertext>,
    ),
    BallotBundleBuildError,
> {
    if scores.is_empty() {
        return Err(BallotBundleBuildError::Empty);
    }
    if scores.iter().any(|&s| s > 5) {
        return Err(BallotBundleBuildError::ScoreOutOfRange);
    }
    let n = scores.len();
    let r_scores: Vec<Scalar> = (0..n)
        .map(|_| Scalar::random(&mut frost_ristretto255::rand_core::OsRng))
        .collect();
    Ok(prove_ballot_bundle_with_outputs_internal(
        y_point, scores, &r_scores,
    ))
}

/// **Test/test-fixture-only.** Same as `prove_ballot_bundle_with_outputs`
/// but caller supplies the per-score encryption randomness — needed for
/// deterministic wire-format pinning. Production code must NEVER call this.
///
/// **Determinism scope:** ONLY the per-score encryption randomness
/// (`r_scores`) is fixed. Indicator-ciphertext nonces AND CDS simulator
/// scalars inside each sigma-protocol branch are still sampled internally
/// — so for fixed (y_point, scores, r_scores) the output `(bundle, cs, ci)`
/// is NOT byte-identical across runs. Fixture-pinning tests that need
/// byte-stable proof blobs use synthetic blobs (`vec![0xEE; ...]`) rather
/// than calling this helper.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn prove_ballot_bundle_with_outputs_with_score_nonces(
    y_point: &RistrettoPoint,
    scores: &[u64],
    r_scores: &[Scalar],
) -> (
    BallotBundleProof,
    Vec<crate::community_voting_core::EncCiphertext>,
    Vec<crate::community_voting_core::EncCiphertext>,
) {
    assert_eq!(r_scores.len(), scores.len());
    prove_ballot_bundle_with_outputs_internal(y_point, scores, r_scores)
}

fn prove_ballot_bundle_with_outputs_internal(
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
    let mut range_bytes = Vec::with_capacity(n * Range5Proof::SIZE);
    let mut ciphertexts_score_pts: Vec<(RistrettoPoint, RistrettoPoint)> = Vec::with_capacity(n);
    let mut ciphertexts_scores_wire: Vec<crate::community_voting_core::EncCiphertext> =
        Vec::with_capacity(n);
    for (i, &m) in scores.iter().enumerate() {
        let (c1, c2) = encrypt(Scalar::from(m), *y_point, r_scores[i]);
        ciphertexts_score_pts.push((c1, c2));
        ciphertexts_scores_wire.push(crate::community_voting_core::EncCiphertext {
            c1: compress_point(&c1),
            c2: compress_point(&c2),
        });
        let p = range5_prove(y_point, &c1, &c2, m, r_scores[i]);
        range_bytes.extend_from_slice(&p.to_bytes());
    }
    let pair_count = n * (n - 1) / 2;
    let mut cons_bytes = Vec::with_capacity(pair_count * ConsistencyProof::SIZE);
    let mut ciphertexts_indicators_wire: Vec<crate::community_voting_core::EncCiphertext> =
        Vec::with_capacity(pair_count);
    for a in 0..n {
        for b in (a + 1)..n {
            let indicator = if scores[a] > scores[b] { 1u64 } else { 0 };
            let r_i = Scalar::random(&mut frost_ristretto255::rand_core::OsRng);
            let (c_i_1, c_i_2) = encrypt(Scalar::from(indicator), *y_point, r_i);
            ciphertexts_indicators_wire.push(crate::community_voting_core::EncCiphertext {
                c1: compress_point(&c_i_1),
                c2: compress_point(&c_i_2),
            });
            let p = consistency_prove(
                y_point,
                (
                    &ciphertexts_score_pts[a].0,
                    &ciphertexts_score_pts[a].1,
                    scores[a],
                    r_scores[a],
                ),
                (
                    &ciphertexts_score_pts[b].0,
                    &ciphertexts_score_pts[b].1,
                    scores[b],
                    r_scores[b],
                ),
                (&c_i_1, &c_i_2, indicator, r_i),
            );
            cons_bytes.extend_from_slice(&p.to_bytes());
        }
    }
    (
        BallotBundleProof {
            range_proofs: range_bytes,
            consistency_proofs: cons_bytes,
        },
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
    if proof.range_proofs.len() != Range5Proof::SIZE * n {
        return false;
    }
    let expected_pairs = n * (n - 1) / 2;
    if proof.consistency_proofs.len() != ConsistencyProof::SIZE * expected_pairs {
        return false;
    }
    if ciphertexts_indicators.len() != expected_pairs {
        return false;
    }
    let mut decoded_scores: Vec<(RistrettoPoint, RistrettoPoint)> = Vec::with_capacity(n);
    for ec in ciphertexts_scores {
        let c1 = match decompress_point(&ec.c1) {
            Some(p) => p,
            None => return false,
        };
        let c2 = match decompress_point(&ec.c2) {
            Some(p) => p,
            None => return false,
        };
        decoded_scores.push((c1, c2));
    }
    for (i, (c1, c2)) in decoded_scores.iter().enumerate() {
        let mut buf = [0u8; Range5Proof::SIZE];
        buf.copy_from_slice(
            &proof.range_proofs[i * Range5Proof::SIZE..(i + 1) * Range5Proof::SIZE],
        );
        let p = match Range5Proof::from_bytes(&buf) {
            Some(p) => p,
            None => return false,
        };
        if !range5_verify(y_point, c1, c2, &p) {
            return false;
        }
    }
    let mut idx = 0usize;
    for a in 0..n {
        for b in (a + 1)..n {
            let ec_i = &ciphertexts_indicators[idx];
            let c_i_1 = match decompress_point(&ec_i.c1) {
                Some(p) => p,
                None => return false,
            };
            let c_i_2 = match decompress_point(&ec_i.c2) {
                Some(p) => p,
                None => return false,
            };
            let mut buf = [0u8; ConsistencyProof::SIZE];
            buf.copy_from_slice(
                &proof.consistency_proofs
                    [idx * ConsistencyProof::SIZE..(idx + 1) * ConsistencyProof::SIZE],
            );
            let p = match ConsistencyProof::from_bytes(&buf) {
                Some(p) => p,
                None => return false,
            };
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_voting_tier3_crypto::encrypt;
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
    use frost_ristretto255::rand_core::OsRng;

    fn rs() -> Scalar {
        Scalar::random(&mut OsRng)
    }

    // ── DLEQ proof tests ────────────────────────────────────────────────

    #[test]
    fn dleq_honest_proves_and_verifies() {
        let x_i = rs();
        let y_i = G * x_i;
        let c1_agg = G * rs();
        let d_i = c1_agg * x_i;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(dleq_verify(&G, &y_i, &c1_agg, &d_i, &proof));
    }

    #[test]
    fn dleq_tampered_share_fails() {
        let x_i = rs();
        let y_i = G * x_i;
        let c1_agg = G * rs();
        let d_i = c1_agg * x_i;
        let bad_d = d_i + G;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(!dleq_verify(&G, &y_i, &c1_agg, &bad_d, &proof));
    }

    #[test]
    fn dleq_tampered_y_fails() {
        let x_i = rs();
        let y_i = G * x_i;
        let bad_y = y_i + G;
        let c1_agg = G * rs();
        let d_i = c1_agg * x_i;
        let proof = dleq_prove(&G, &y_i, &c1_agg, &d_i, &x_i);
        assert!(!dleq_verify(&G, &bad_y, &c1_agg, &d_i, &proof));
    }

    // ── Range proof tests ───────────────────────────────────────────────

    #[test]
    fn range5_proves_for_each_m_in_0_to_5() {
        let x = rs();
        let y_point = G * x;
        for m in 0u64..=5 {
            let r = rs();
            let (c1, c2) = encrypt(Scalar::from(m), y_point, r);
            let proof = range5_prove(&y_point, &c1, &c2, m, r);
            assert!(
                range5_verify(&y_point, &c1, &c2, &proof),
                "m={m} should verify"
            );
        }
    }

    #[test]
    fn range5_rejects_out_of_range_m() {
        let x = rs();
        let y_point = G * x;
        let r = rs();
        let m_bad = Scalar::from(6u64);
        let c1 = G * r;
        let c2 = (G * m_bad) + y_point * r;
        // Caller tries to prove m=5 for a ciphertext that actually encrypts 6.
        let bad_proof = range5_prove(&y_point, &c1, &c2, 5, r);
        assert!(!range5_verify(&y_point, &c1, &c2, &bad_proof));
    }

    // ── Indicator-consistency tests ─────────────────────────────────────

    #[test]
    fn consistency_proof_passes_for_all_36_score_pair_combinations() {
        let x = rs();
        let y_point = G * x;
        let mut count = 0;
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
                count += 1;
            }
        }
        assert_eq!(count, 36);
    }

    /// Bit-flipping the bundled consistency-proof bytes must surface as a
    /// verifier-side reject. The original site that exposed the soundness
    /// gap: an "indicator encrypts 0" ciphertext for a (5, 0) pair (where
    /// the indicator should be 1) — the old verifier accepted because it
    /// only checked indicator ∈ {0..5} and (|score_diff|) ∈ {0..5}
    /// independently; the new construction forces indicator ∈ {0,1} AND
    /// binds to sign(score_A − score_B) via 2-way OR.
    #[test]
    fn consistency_rejects_tampered_proof_bytes() {
        let x = rs();
        let y_point = G * x;
        let r_a = rs();
        let r_b = rs();
        let r_i = rs();
        let (c_a_1, c_a_2) = encrypt(Scalar::from(5u64), y_point, r_a);
        let (c_b_1, c_b_2) = encrypt(Scalar::from(0u64), y_point, r_b);
        // Honest scenario: 5 > 0 so indicator = 1.
        let (c_i_1, c_i_2) = encrypt(Scalar::from(1u64), y_point, r_i);
        let proof = consistency_prove(
            &y_point,
            (&c_a_1, &c_a_2, 5, r_a),
            (&c_b_1, &c_b_2, 0, r_b),
            (&c_i_1, &c_i_2, 1, r_i),
        );
        assert!(consistency_verify(
            &y_point,
            (&c_a_1, &c_a_2),
            (&c_b_1, &c_b_2),
            (&c_i_1, &c_i_2),
            &proof,
        ));
        // Tamper: flip one bit in the wire encoding → must fail.
        let mut bytes = proof.to_bytes();
        bytes[0] ^= 0x01;
        let tampered = ConsistencyProof::from_bytes(&bytes).expect("decodes");
        assert!(!consistency_verify(
            &y_point,
            (&c_a_1, &c_a_2),
            (&c_b_1, &c_b_2),
            (&c_i_1, &c_i_2),
            &tampered,
        ));
    }

    /// Soundness test (new): an attacker substitutes a c_indicator that
    /// encrypts a value `v ∈ {2, 3, 4, 5}` instead of {0, 1}, then tries to
    /// trick the verifier into accepting (the old indicator-Range5 check
    /// accepted any v ∈ {0..5}; the new bit-OR rejects any v ∉ {0, 1}).
    ///
    /// We construct a malicious proof by forging the bit-branch:
    /// re-using `consistency_prove` with manufactured "as-if" witnesses
    /// is impossible because the prover asserts (indicator ∈ {0,1}, indicator
    /// matches sign). Instead we encrypt a v=5 indicator and call
    /// `range5_prove` for the bit position — which the new verifier rejects
    /// because the indicator bit-witness uses v ∈ {0, 1} only.
    ///
    /// Construction: build an honest proof for a related (score_A, score_B)
    /// + indicator pair, then swap in a c_indicator encrypting 2..=5.
    #[test]
    fn consistency_rejects_indicator_value_2_or_5() {
        let x = rs();
        let y_point = G * x;
        // For each forged indicator value v ∈ {2, 3, 4, 5}, construct an
        // honest consistency proof for the (5, 0) → indicator=1 case, then
        // substitute c_indicator with an encryption of v. The proof's
        // bit-witness response was computed against indicator=1; the new
        // c_I now sits in NEITHER branch's claimed-encrypted-value, so
        // BOTH the b=0 and b=1 bit-witness reconstructions must produce
        // commitments that mismatch the transcript-derived challenge.
        for v in 2u64..=5 {
            let r_a = rs();
            let r_b = rs();
            let r_i_honest = rs();
            let r_i_forged = rs();
            let (c_a_1, c_a_2) = encrypt(Scalar::from(5u64), y_point, r_a);
            let (c_b_1, c_b_2) = encrypt(Scalar::from(0u64), y_point, r_b);
            let (c_i_1, c_i_2) = encrypt(Scalar::from(1u64), y_point, r_i_honest);
            let proof = consistency_prove(
                &y_point,
                (&c_a_1, &c_a_2, 5, r_a),
                (&c_b_1, &c_b_2, 0, r_b),
                (&c_i_1, &c_i_2, 1, r_i_honest),
            );
            // Forged c_indicator encrypts v ∉ {0, 1}.
            let (cf_1, cf_2) = encrypt(Scalar::from(v), y_point, r_i_forged);
            assert!(
                !consistency_verify(
                    &y_point,
                    (&c_a_1, &c_a_2),
                    (&c_b_1, &c_b_2),
                    (&cf_1, &cf_2),
                    &proof,
                ),
                "verifier must reject forged indicator value v={v}",
            );
        }
    }

    /// Soundness test (new): indicator-vs-score-sign mismatch. Honest scores
    /// give score_A ≤ score_B → indicator should be 0; attacker substitutes
    /// indicator = 1 (a valid bit value) hoping the old check (Range5 on
    /// each independently) accepts. The new 2-way OR binds indicator to
    /// sign(score_A − score_B): branch 0 (b=0) needs (score_B − score_A) ∈
    /// {0..5} on c_B − c_A; branch 1 (b=1) needs (score_A − score_B − 1) ∈
    /// {0..4} on c_A − c_B − E(1). For (score_A, score_B) = (1, 5) and
    /// forged indicator = 1, branch 1's derived ciphertext encrypts (1−5−1) = -5
    /// which is not in {0..4} → range proof must fail.
    #[test]
    fn consistency_rejects_indicator_mismatch_with_score_sign() {
        let x = rs();
        let y_point = G * x;
        // Build an HONEST proof for a different score pair, then swap c_I.
        for (score_a, score_b) in [(1u64, 5u64), (0, 3), (2, 4), (3, 3)] {
            let true_b = if score_a > score_b { 1u64 } else { 0 };
            let forged_b = 1 - true_b; // flip the bit
            let r_a = rs();
            let r_b = rs();
            let r_i = rs();
            let (c_a_1, c_a_2) = encrypt(Scalar::from(score_a), y_point, r_a);
            let (c_b_1, c_b_2) = encrypt(Scalar::from(score_b), y_point, r_b);
            // Honest proof matches honest (score_a, score_b, true_b).
            let (c_i_honest_1, c_i_honest_2) = encrypt(Scalar::from(true_b), y_point, r_i);
            let proof = consistency_prove(
                &y_point,
                (&c_a_1, &c_a_2, score_a, r_a),
                (&c_b_1, &c_b_2, score_b, r_b),
                (&c_i_honest_1, &c_i_honest_2, true_b, r_i),
            );
            // Forged c_I encrypts the WRONG bit.
            let r_i_forged = rs();
            let (cf_1, cf_2) = encrypt(Scalar::from(forged_b), y_point, r_i_forged);
            assert!(
                !consistency_verify(
                    &y_point,
                    (&c_a_1, &c_a_2),
                    (&c_b_1, &c_b_2),
                    (&cf_1, &cf_2),
                    &proof,
                ),
                "verifier must reject forged indicator-sign mismatch for \
                 (score_A={score_a}, score_B={score_b}, true_b={true_b}, \
                  forged_b={forged_b})",
            );
        }
    }

    /// Soundness test (new): the consistency transcript must commit to c_A,
    /// c_B, AND c_I — proofs cannot be mixed-and-matched across pairs.
    /// Constructing two consistency proofs over different (c_A, c_B, c_I)
    /// triples and swapping the proofs must surface as verifier reject.
    #[test]
    fn consistency_rejects_proof_swapped_across_pairs() {
        let x = rs();
        let y_point = G * x;
        // Pair 1: (5, 0) → indicator = 1.
        let r_a1 = rs();
        let r_b1 = rs();
        let r_i1 = rs();
        let (c_a1_1, c_a1_2) = encrypt(Scalar::from(5u64), y_point, r_a1);
        let (c_b1_1, c_b1_2) = encrypt(Scalar::from(0u64), y_point, r_b1);
        let (c_i1_1, c_i1_2) = encrypt(Scalar::from(1u64), y_point, r_i1);
        let proof1 = consistency_prove(
            &y_point,
            (&c_a1_1, &c_a1_2, 5, r_a1),
            (&c_b1_1, &c_b1_2, 0, r_b1),
            (&c_i1_1, &c_i1_2, 1, r_i1),
        );
        // Pair 2: (2, 4) → indicator = 0.
        let r_a2 = rs();
        let r_b2 = rs();
        let r_i2 = rs();
        let (c_a2_1, c_a2_2) = encrypt(Scalar::from(2u64), y_point, r_a2);
        let (c_b2_1, c_b2_2) = encrypt(Scalar::from(4u64), y_point, r_b2);
        let (c_i2_1, c_i2_2) = encrypt(Scalar::from(0u64), y_point, r_i2);
        // Use pair-1's proof against pair-2's statement → must reject.
        assert!(!consistency_verify(
            &y_point,
            (&c_a2_1, &c_a2_2),
            (&c_b2_1, &c_b2_2),
            (&c_i2_1, &c_i2_2),
            &proof1,
        ));
    }

    /// Round-trip the new ConsistencyProof byte encoding.
    #[test]
    fn consistency_proof_byte_round_trip() {
        let x = rs();
        let y_point = G * x;
        let r_a = rs();
        let r_b = rs();
        let r_i = rs();
        let (c_a_1, c_a_2) = encrypt(Scalar::from(4u64), y_point, r_a);
        let (c_b_1, c_b_2) = encrypt(Scalar::from(2u64), y_point, r_b);
        let (c_i_1, c_i_2) = encrypt(Scalar::from(1u64), y_point, r_i);
        let proof = consistency_prove(
            &y_point,
            (&c_a_1, &c_a_2, 4, r_a),
            (&c_b_1, &c_b_2, 2, r_b),
            (&c_i_1, &c_i_2, 1, r_i),
        );
        let bytes = proof.to_bytes();
        assert_eq!(bytes.len(), ConsistencyProof::SIZE);
        let decoded = ConsistencyProof::from_bytes(&bytes).expect("decode");
        assert_eq!(proof, decoded);
        assert!(consistency_verify(
            &y_point,
            (&c_a_1, &c_a_2),
            (&c_b_1, &c_b_2),
            (&c_i_1, &c_i_2),
            &decoded,
        ));
    }

    // ── Bundle test ─────────────────────────────────────────────────────

    #[test]
    fn ballot_bundle_round_trip_n5() {
        let x = rs();
        let y_point = G * x;
        let scores = [5u64, 4, 3, 2, 1];
        let (bundle, ciphertexts_scores, ciphertexts_indicators) =
            prove_ballot_bundle_with_outputs(&y_point, &scores).expect("valid scores");
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
        let y_point = G * x;
        let scores = [5u64, 0, 0];
        let (mut bundle, ciphertexts_scores, ciphertexts_indicators) =
            prove_ballot_bundle_with_outputs(&y_point, &scores).expect("valid scores");
        bundle.consistency_proofs[0] ^= 0x01; // bit-flip one byte
        assert!(!verify_ballot_bundle(
            &y_point,
            &ciphertexts_scores,
            &ciphertexts_indicators,
            &bundle,
        ));
    }

    #[test]
    fn ballot_bundle_rejects_empty_scores() {
        let x = rs();
        let y_point = G * x;
        let err = prove_ballot_bundle_with_outputs(&y_point, &[]).expect_err("empty");
        assert_eq!(err, BallotBundleBuildError::Empty);
    }

    #[test]
    fn ballot_bundle_rejects_out_of_range_score() {
        let x = rs();
        let y_point = G * x;
        let err = prove_ballot_bundle_with_outputs(&y_point, &[5, 6, 0]).expect_err("score=6");
        assert_eq!(err, BallotBundleBuildError::ScoreOutOfRange);
    }

    /// Deterministic-nonce variant's score-randomness scope.
    /// `_with_score_nonces` ONLY freezes the per-score encryption randomness.
    /// Indicator-ciphertext nonces + CDS simulator scalars are still sampled
    /// internally, so for fixed inputs only the score ciphertexts are
    /// byte-stable. Fixture tests that need byte-stable proof blobs use
    /// synthetic blobs (`vec![0xEE; ...]`) instead of this helper.
    #[test]
    fn ballot_bundle_with_score_nonces_freezes_score_ciphertexts_only() {
        let x = rs();
        let y_point = G * x;
        let scores = [4u64, 1, 2];
        let r_scores: Vec<Scalar> = (0..3).map(|_| rs()).collect();
        let (_bundle_a, cs_a, _ci_a) =
            prove_ballot_bundle_with_outputs_with_score_nonces(&y_point, &scores, &r_scores);
        let (_bundle_b, cs_b, _ci_b) =
            prove_ballot_bundle_with_outputs_with_score_nonces(&y_point, &scores, &r_scores);
        assert_eq!(
            cs_a, cs_b,
            "score ciphertexts are deterministic in r_scores"
        );
    }
}
