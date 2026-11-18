//! Tiny OPACK encoder: just enough for the `{"name": <display name>}` dictionary
//! pyatv puts in the Name TLV of pair-setup M5.

fn string(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    if b.len() <= 0x20 {
        out.push(0x40 + b.len() as u8);
    } else if b.len() <= 0xFF {
        out.push(0x61);
        out.push(b.len() as u8);
    } else {
        out.push(0x62);
        out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(b);
}

/// OPACK encoding of a flat string->string dictionary (fewer than 15 entries).
pub fn string_dict(entries: &[(&str, &str)]) -> Vec<u8> {
    assert!(entries.len() < 15);
    let mut out = vec![0xE0 + entries.len() as u8];
    for (k, v) in entries {
        string(&mut out, k);
        string(&mut out, v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn name_dict_matches_pyatv() {
        let v = vectors();
        assert_eq!(string_dict(&[("name", "Stereo Pair")]), unhex(&v["opack_name"]));
    }
}
