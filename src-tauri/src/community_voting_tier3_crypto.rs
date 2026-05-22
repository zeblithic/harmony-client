//! ZEB-295: Threshold-ElGamal in Ristretto255 + Lagrange combine + BSGS
//! discrete-log recovery for the Tier 3c ballot-secret ratification path.
//! Spec §4.

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_TABLE as G_TABLE,
    ristretto::{CompressedRistretto, RistrettoPoint},
    scalar::Scalar,
};
use std::collections::BTreeMap;

/// Exponential ElGamal encrypt. Returns `(c1, c2) = (G·r, G·m + Y·r)`.
/// Spec §4.2.
pub fn encrypt(m: Scalar, y_point: RistrettoPoint, r: Scalar) -> (RistrettoPoint, RistrettoPoint) {
    let c1 = G_TABLE * &r;
    let c2 = (G_TABLE * &m) + y_point * r;
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
            if j == i {
                continue;
            }
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
        return if *p == RistrettoPoint::default() {
            Some(0)
        } else {
            None
        };
    }
    let sqrt_bound = (bound as f64).sqrt().ceil() as u64 + 1;
    // Baby steps: j → G · j  (table indexed by compressed-point bytes).
    let mut table: std::collections::HashMap<[u8; 32], u64> = std::collections::HashMap::new();
    let mut acc = RistrettoPoint::default();
    for j in 0..=sqrt_bound {
        table.insert(acc.compress().to_bytes(), j);
        acc += G_TABLE * &Scalar::ONE;
    }
    // Giant steps: search P - G · (k * √bound) for k ∈ [0, √bound].
    let m_step = G_TABLE * &Scalar::from(sqrt_bound);
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
        let sqrt_bound = if bound == 0 {
            1
        } else {
            (bound as f64).sqrt().ceil() as u64 + 1
        };
        let mut table = std::collections::HashMap::with_capacity(sqrt_bound as usize + 1);
        let mut acc = RistrettoPoint::default();
        for j in 0..=sqrt_bound {
            table.insert(acc.compress().to_bytes(), j);
            acc += G_TABLE * &Scalar::ONE;
        }
        let m_step = G_TABLE * &Scalar::from(sqrt_bound);
        Self {
            sqrt_bound,
            bound,
            table,
            m_step,
        }
    }
    pub fn solve(&self, p: &RistrettoPoint) -> Option<u64> {
        let mut k_step = RistrettoPoint::default();
        for k in 0..=self.sqrt_bound {
            let candidate = p - k_step;
            if let Some(&j) = self.table.get(&candidate.compress().to_bytes()) {
                let m = k * self.sqrt_bound + j;
                if m <= self.bound {
                    return Some(m);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
    use frost_ristretto255::rand_core::OsRng;

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
        assert_eq!(
            bsgs(&p, 50),
            None,
            "discrete log past the bound must not be returned"
        );
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
        assert_eq!(
            bsgs(&m_point, 10),
            None,
            "tampered c2 must not recover within original bound"
        );
    }
}
