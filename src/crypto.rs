//! Shared crypto helpers: HKDF-SHA512 key derivation and the ChaCha20-Poly1305
//! nonce conventions used by HAP and AirPlay 2.
//!
//! Nonce layout (pyatv `support/chacha20.py`): 12 bytes = 4 zero bytes followed by
//! either an 8-byte little-endian message counter or an 8-byte ASCII label such as
//! `PV-Msg02`.

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;

pub const TAG_LEN: usize = 16;

/// HKDF-SHA512 with string salt/info, 32 byte output (pyatv `hkdf_expand`).
pub fn hkdf_sha512(salt: &str, info: &str, ikm: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info.as_bytes(), &mut okm)
        .expect("32 bytes is a valid HKDF-SHA512 output length");
    okm
}

/// Nonce built from a message counter: `00 00 00 00 || counter (u64 LE)`.
pub fn counter_nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// Nonce built from an 8-byte label (e.g. `PV-Msg02`): `00 00 00 00 || label`.
pub fn label_nonce(label: &[u8]) -> [u8; 12] {
    assert!(label.len() <= 12);
    let mut n = [0u8; 12];
    n[12 - label.len()..].copy_from_slice(label);
    n
}

fn aead(key: &[u8]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(key))
}

/// Encrypt with an explicit label nonce, returning ciphertext || tag.
pub fn seal_label(key: &[u8], label: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    let tag = aead(key)
        .encrypt_in_place_detached(Nonce::from_slice(&label_nonce(label)), b"", &mut buf)
        .expect("chacha encrypt");
    buf.extend_from_slice(&tag);
    buf
}

/// Decrypt ciphertext || tag with an explicit label nonce.
pub fn open_label(key: &[u8], label: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < TAG_LEN {
        return Err(anyhow!("ciphertext too short"));
    }
    let (ct, tag) = data.split_at(data.len() - TAG_LEN);
    let mut buf = ct.to_vec();
    aead(key)
        .decrypt_in_place_detached(
            Nonce::from_slice(&label_nonce(label)),
            b"",
            &mut buf,
            Tag::from_slice(tag),
        )
        .map_err(|_| anyhow!("decryption failed (bad key or tag)"))?;
    Ok(buf)
}

/// One direction of a counter-nonce ChaCha20-Poly1305 stream.
pub struct CounterCipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

impl CounterCipher {
    pub fn new(key: &[u8]) -> Self {
        Self { aead: aead(key), counter: 0 }
    }

    /// Encrypt `buf` in place with the next counter nonce; returns the tag and the
    /// counter that was used.
    pub fn seal_in_place(&mut self, aad: &[u8], buf: &mut [u8]) -> ([u8; TAG_LEN], u64) {
        let used = self.counter;
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&counter_nonce(used)), aad, buf)
            .expect("chacha encrypt");
        self.counter += 1;
        (tag.into(), used)
    }

    /// Encrypt returning ciphertext || tag.
    pub fn seal(&mut self, aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let mut buf = plain.to_vec();
        let (tag, _) = self.seal_in_place(aad, &mut buf);
        buf.extend_from_slice(&tag);
        buf
    }

    /// Decrypt ciphertext || tag with the next counter nonce.
    pub fn open(&mut self, aad: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < TAG_LEN {
            return Err(anyhow!("ciphertext too short"));
        }
        let (ct, tag) = data.split_at(data.len() - TAG_LEN);
        let mut buf = ct.to_vec();
        self.aead
            .decrypt_in_place_detached(
                Nonce::from_slice(&counter_nonce(self.counter)),
                aad,
                &mut buf,
                Tag::from_slice(tag),
            )
            .map_err(|_| anyhow!("decryption failed at counter {}", self.counter))?;
        self.counter += 1;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn hkdf_matches_pyatv() {
        let v = vectors();
        for case in v["hkdf"].as_array().unwrap() {
            let okm = hkdf_sha512(
                case["salt"].as_str().unwrap(),
                case["info"].as_str().unwrap(),
                &unhex(&case["ikm"]),
            );
            assert_eq!(okm.to_vec(), unhex(&case["okm"]));
        }
    }

    #[test]
    fn chacha_nonce_layouts_match_pyatv() {
        let v = vectors();
        let c = &v["chacha"];
        let key = unhex(&c["key"]);
        assert_eq!(seal_label(&key, b"PV-Msg02", b"hello pair verify"), unhex(&c["pv_msg02"]));
        let mut cc = CounterCipher::new(&key);
        assert_eq!(cc.seal(&[6, 0], b"frame0"), unhex(&c["counter0_aad"]));
        assert_eq!(cc.seal(&[6, 0], b"frame1"), unhex(&c["counter1_aad"]));
        // round trip
        let ct = seal_label(&key, b"PS-Msg05", b"abc");
        assert_eq!(open_label(&key, b"PS-Msg05", &ct).unwrap(), b"abc");
        assert!(open_label(&key, b"PS-Msg06", &ct).is_err());
    }
}
