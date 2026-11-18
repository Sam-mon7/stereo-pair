//! TLV8 encoding used by HAP pairing (pyatv `auth/hap_tlv8.py`).
//!
//! Values longer than 255 bytes are split into consecutive fragments with the same
//! tag; the reader concatenates them. Like pyatv, an empty value is not written.

use std::collections::BTreeMap;

pub mod tag {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const SEQ_NO: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0A;
    pub const NAME: u8 = 0x11;
    pub const FLAGS: u8 = 0x13;
}

/// `Flags` value requesting transient pairing.
pub const FLAG_TRANSIENT: u8 = 0x10;

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (t, value) in items {
        for chunk in value.chunks(255) {
            out.push(*t);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

pub type Tlv = BTreeMap<u8, Vec<u8>>;

pub fn decode(data: &[u8]) -> Tlv {
    let mut out: Tlv = BTreeMap::new();
    let mut pos = 0;
    while pos + 2 <= data.len() {
        let t = data[pos];
        let len = data[pos + 1] as usize;
        let end = (pos + 2 + len).min(data.len());
        out.entry(t).or_default().extend_from_slice(&data[pos + 2..end]);
        pos = end;
    }
    out
}

pub fn error_name(code: u8) -> &'static str {
    match code {
        1 => "Unknown",
        2 => "Authentication (wrong PIN or bad credentials)",
        3 => "BackOff (too many attempts, wait)",
        4 => "MaxPeers",
        5 => "MaxTries",
        6 => "Unavailable (already paired / not in pairing mode)",
        7 => "Busy",
        _ => "unrecognised",
    }
}

/// Summary for debug logs: tag=len pairs.
pub fn describe(t: &Tlv) -> String {
    t.iter()
        .map(|(k, v)| {
            if v.len() <= 2 {
                format!("{k:#04x}={}", hex::encode(v))
            } else {
                format!("{k:#04x}=<{} bytes>", v.len())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn matches_pyatv_including_fragmentation() {
        let v = vectors();
        let pk = unhex(&v["tlv"]["pubkey"]);
        let enc = encode(&[(tag::SEQ_NO, &[1]), (tag::PUBLIC_KEY, &pk), (tag::FLAGS, &[0x10])]);
        assert_eq!(enc, unhex(&v["tlv"]["encoded"]));
        let dec = decode(&enc);
        assert_eq!(dec[&tag::PUBLIC_KEY], pk);
        assert_eq!(dec[&tag::SEQ_NO], vec![1]);
        assert_eq!(dec[&tag::FLAGS], vec![0x10]);
    }

    #[test]
    fn empty_value_is_omitted() {
        assert!(encode(&[(1, b"")]).is_empty());
    }
}
