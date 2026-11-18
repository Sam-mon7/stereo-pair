//! SRP-6a client exactly as `srptools<1.0` computes it for HAP (3072-bit group,
//! generator 5, SHA-512, username `Pair-Setup`).
//!
//! srptools quirks that matter for byte-exactness:
//! * integers are hashed as their *minimal* big-endian bytes (leading zeros dropped),
//!   except where `PAD()` is applied explicitly (k and u);
//! * the salt is hashed as the raw bytes received from the device;
//! * `x = H(salt | int_bytes(H("Pair-Setup:" + pin)))` - the inner digest goes through
//!   int conversion, so it also loses leading zero bytes;
//! * `M1 = H(int_bytes(H(N) xor H(g)) | int_bytes(H(I)) | salt | A | B | K)`;
//! * `M2 = H(A | M1 | K)`;
//! * K = H(S) is the full 64-byte digest.

use anyhow::{Result, bail};
use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha512};

const PRIME_3072_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA6",
    "3B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F2411",
    "7C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08",
    "CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D",
    "04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D8760273",
    "3EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
);
const USERNAME: &str = "Pair-Setup";

/// Minimal big-endian bytes (srptools `int_to_bytes`; zero becomes `00`).
fn int_bytes(v: &BigUint) -> Vec<u8> {
    if v.is_zero() { vec![0] } else { v.to_bytes_be() }
}

/// Strip leading zero bytes (digest -> int -> bytes round trip).
fn strip(b: &[u8]) -> Vec<u8> {
    int_bytes(&BigUint::from_bytes_be(b))
}

fn h(parts: &[&[u8]]) -> Vec<u8> {
    let mut d = Sha512::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().to_vec()
}

pub struct SrpResult {
    /// A, minimal bytes (what goes into the PublicKey TLV).
    pub client_public: Vec<u8>,
    /// M1 (Proof TLV in M3).
    pub client_proof: Vec<u8>,
    /// K = H(S), 64 bytes. The shared secret for transient keys / pair-setup M5.
    pub session_key: Vec<u8>,
    /// Expected M2 = H(A | M1 | K) from the accessory in M4.
    pub expected_server_proof: Vec<u8>,
}

/// Run the client side of SRP for the given private exponent bytes `a`.
pub fn client_process(a_bytes: &[u8], pin: &str, salt: &[u8], server_public: &[u8]) -> Result<SrpResult> {
    let n = BigUint::parse_bytes(PRIME_3072_HEX.as_bytes(), 16).expect("prime");
    let g = BigUint::from(5u32);
    let n_bytes = int_bytes(&n);
    let pad = |v: &BigUint| {
        let b = int_bytes(v);
        let mut out = vec![0u8; n_bytes.len().saturating_sub(b.len())];
        out.extend_from_slice(&b);
        out
    };

    let a = BigUint::from_bytes_be(a_bytes);
    let big_a = g.modpow(&a, &n);
    let big_b = BigUint::from_bytes_be(server_public);
    if (&big_b % &n).is_zero() {
        bail!("invalid SRP server public key");
    }

    // k = H(N | PAD(g))
    let k = BigUint::from_bytes_be(&h(&[&n_bytes, &pad(&g)]));
    // x = H(s | H(I ":" P))
    let inner = strip(&h(&[format!("{USERNAME}:{pin}").as_bytes()]));
    let x = BigUint::from_bytes_be(&h(&[salt, &inner]));
    // u = H(PAD(A) | PAD(B))
    let u = BigUint::from_bytes_be(&h(&[&pad(&big_a), &pad(&big_b)]));
    // S = (B - k*g^x) ^ (a + u*x) mod N  (computed with a non-negative base)
    let v = g.modpow(&x, &n);
    let kv = (&k * &v) % &n;
    let base = ((&big_b % &n) + &n - kv) % &n;
    let s = base.modpow(&(&a + &u * &x), &n);
    let session_key = h(&[&int_bytes(&s)]);

    // M1 = H(H(N) xor H(g) | H(I) | s | A | B | K)
    let hn = BigUint::from_bytes_be(&h(&[&n_bytes]));
    let hg = BigUint::from_bytes_be(&h(&[&int_bytes(&g)]));
    let hxor = int_bytes(&(hn ^ hg));
    let hi = strip(&h(&[USERNAME.as_bytes()]));
    let a_bytes_min = int_bytes(&big_a);
    let b_bytes_min = int_bytes(&big_b);
    let m1 = h(&[&hxor, &hi, salt, &a_bytes_min, &b_bytes_min, &session_key]);
    let m2 = h(&[&a_bytes_min, &m1, &session_key]);

    Ok(SrpResult {
        client_public: a_bytes_min,
        client_proof: m1,
        session_key,
        expected_server_proof: m2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn matches_srptools() {
        let v = vectors();
        let s = &v["srp"];
        let r = client_process(
            &unhex(&s["client_private"]),
            s["pin"].as_str().unwrap(),
            &unhex(&s["salt"]),
            &unhex(&s["server_public"]),
        )
        .unwrap();
        assert_eq!(r.client_public, unhex(&s["client_public"]));
        assert_eq!(r.session_key, unhex(&s["session_key"]));
        assert_eq!(r.client_proof, unhex(&s["client_proof"]));
        assert_eq!(r.expected_server_proof, unhex(&s["server_proof"]));
    }

    #[test]
    fn wrong_pin_changes_proof() {
        let v = vectors();
        let s = &v["srp"];
        let r = client_process(&unhex(&s["client_private"]), "9999", &unhex(&s["salt"]), &unhex(&s["server_public"]))
            .unwrap();
        assert_ne!(r.client_proof, unhex(&s["client_proof"]));
    }
}
