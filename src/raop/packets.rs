//! RAOP/AirPlay RTP packet layouts (pyatv `protocols/raop/packets.py`). All fields
//! big-endian.

pub const FRAMES_PER_PACKET: usize = 352;
pub const AUDIO_HEADER_LEN: usize = 12;

/// RTP audio header: 0x80, 0xE0 (first, marker) / 0x60, seq, timestamp, ssrc.
pub fn audio_header(first: bool, seq: u16, timestamp: u32, ssrc: u32) -> [u8; AUDIO_HEADER_LEN] {
    let mut h = [0u8; AUDIO_HEADER_LEN];
    h[0] = 0x80;
    h[1] = if first { 0xE0 } else { 0x60 };
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// Sync packet sent on the control port about once a second:
/// 0x90 (first) / 0x80, 0xD4, 0x0007, rtp-now-minus-latency, NTP sec, NTP frac, rtp-now.
pub fn sync_packet(first: bool, now_minus_latency: u32, ntp_sec: u32, ntp_frac: u32, now: u32) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = if first { 0x90 } else { 0x80 };
    p[1] = 0xD4;
    p[2..4].copy_from_slice(&7u16.to_be_bytes());
    p[4..8].copy_from_slice(&now_minus_latency.to_be_bytes());
    p[8..12].copy_from_slice(&ntp_sec.to_be_bytes());
    p[12..16].copy_from_slice(&ntp_frac.to_be_bytes());
    p[16..20].copy_from_slice(&now.to_be_bytes());
    p
}

/// Answer an NTP-style timing request (32 bytes): reference = the request's send
/// time, receive = transmit = now. Returns None for malformed requests.
pub fn timing_response(request: &[u8], now_sec: u32, now_frac: u32) -> Option<[u8; 32]> {
    if request.len() < 32 {
        return None;
    }
    let mut p = [0u8; 32];
    p[0] = request[0];
    p[1] = 0x53 | 0x80;
    p[2..4].copy_from_slice(&7u16.to_be_bytes());
    // 4..8 padding = 0
    p[8..16].copy_from_slice(&request[24..32]);
    p[16..20].copy_from_slice(&now_sec.to_be_bytes());
    p[20..24].copy_from_slice(&now_frac.to_be_bytes());
    p[24..28].copy_from_slice(&now_sec.to_be_bytes());
    p[28..32].copy_from_slice(&now_frac.to_be_bytes());
    Some(p)
}

/// Retransmit request on the control port: (first lost seq, count).
pub fn parse_retransmit_request(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 8 || data[1] & 0x7F != 0x55 {
        return None;
    }
    Some((u16::from_be_bytes([data[4], data[5]]), u16::from_be_bytes([data[6], data[7]])))
}

/// Retransmit reply: 0x80 0xD6 + original seq + the whole original packet.
pub fn retransmit_reply(original: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(original.len() + 4);
    out.extend_from_slice(&[0x80, 0xD6, original[2], original[3]]);
    out.extend_from_slice(original);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn timing_and_sync_match_pyatv() {
        let v = vectors();
        let t = &v["timing_packet"];
        let resp = timing_response(
            &unhex(&t["request"]),
            t["now_sec"].as_u64().unwrap() as u32,
            t["now_frac"].as_u64().unwrap() as u32,
        )
        .unwrap();
        assert_eq!(resp.to_vec(), unhex(&t["response"]));
        assert_eq!(sync_packet(true, 1000, 0xE1E2E3E4, 0x01020304, 67150).to_vec(), unhex(&v["sync_packet"]));
    }

    #[test]
    fn retransmit_round_trip() {
        let req = [0x80, 0xD5, 0, 1, 0x03, 0xE8, 0, 2];
        assert_eq!(parse_retransmit_request(&req), Some((1000, 2)));
        let pkt = audio_header(false, 1000, 5, 6);
        let r = retransmit_reply(&pkt);
        assert_eq!(&r[..4], &[0x80, 0xD6, 0x03, 0xE8]);
        assert_eq!(&r[4..], &pkt);
    }
}
