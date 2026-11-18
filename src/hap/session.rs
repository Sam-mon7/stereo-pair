//! HAP framed transport encryption (pyatv `auth/hap_session.py`): data is split in
//! blocks of at most 1024 bytes, each sent as `len (u16 LE) || ciphertext || tag`,
//! with the 2 length bytes as AAD and a per-direction counter nonce.

use anyhow::Result;

use crate::crypto::{CounterCipher, TAG_LEN};

const FRAME_LENGTH: usize = 1024;

pub struct HapSession {
    out: CounterCipher,
    inp: CounterCipher,
    pending: Vec<u8>,
}

impl HapSession {
    pub fn new(output_key: &[u8], input_key: &[u8]) -> Self {
        Self { out: CounterCipher::new(output_key), inp: CounterCipher::new(input_key), pending: Vec::new() }
    }

    pub fn encrypt(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + (data.len() / FRAME_LENGTH + 1) * (2 + TAG_LEN));
        for frame in data.chunks(FRAME_LENGTH) {
            let len = (frame.len() as u16).to_le_bytes();
            out.extend_from_slice(&len);
            out.extend_from_slice(&self.out.seal(&len, frame));
        }
        out
    }

    /// Feed received bytes; returns all plaintext from complete blocks.
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.pending.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.pending.len() < 2 {
                break;
            }
            let len = u16::from_le_bytes([self.pending[0], self.pending[1]]) as usize;
            let total = 2 + len + TAG_LEN;
            if self.pending.len() < total {
                break;
            }
            let aad = [self.pending[0], self.pending[1]];
            let plain = self.inp.open(&aad, &self.pending[2..total])?;
            out.extend_from_slice(&plain);
            self.pending.drain(..total);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn framing_matches_pyatv_and_round_trips() {
        let v = vectors();
        let s = &v["hap_session"];
        let key = unhex(&s["out_key"]);
        let rev: Vec<u8> = key.iter().rev().copied().collect();
        let plain = unhex(&s["plain"]);
        let mut tx = HapSession::new(&key, &rev);
        let enc = tx.encrypt(&plain);
        assert_eq!(enc, unhex(&s["encrypted"]));

        // receiver side has the keys swapped; feed in awkward pieces
        let mut rx = HapSession::new(&rev, &key);
        let mut got = Vec::new();
        for piece in enc.chunks(7) {
            got.extend(rx.decrypt(piece).unwrap());
        }
        assert_eq!(got, plain);
    }
}
