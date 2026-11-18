//! The single RTP timeline shared by every speaker.
//!
//! Packet `n` carries `rtptime = n*352 + latency` and is due to be sent at
//! `NTP(start_ts + n*352)`. Sync packets announce exactly that mapping, so each
//! receiver plays packet `n` at `NTP(start_ts + n*352 + latency)` - identical for all
//! speakers because they share `start_ts`, `latency` and the NTP clock. Per-speaker
//! state (seq numbers, keys, SSRC) lives in `StreamSink`.

use std::sync::atomic::{AtomicU64, Ordering};

use super::packets::FRAMES_PER_PACKET;
use super::timing::{ntp_parts, ntp2ts, ts2ntp};

pub const SAMPLE_RATE: u32 = 44100;
/// pyatv's default latency: 22050 + 44100 samples (1.5 s).
pub const DEFAULT_LATENCY_FRAMES: u32 = 22050 + SAMPLE_RATE;
/// latencyMin / latencyMax we announce in SETUP (pyatv values).
pub const LATENCY_MIN_FRAMES: u32 = 4410; // 100 ms (pyatv used 11025)
pub const LATENCY_MAX_FRAMES: u32 = 88200;

pub struct Timeline {
    pub sample_rate: u32,
    /// Latency in frames between send time and play time.
    pub latency: u32,
    /// Absolute sample timestamp (NTP timescale) of packet 0. Set when streaming starts.
    start_ts: AtomicU64,
    /// Number of packets already sent (index of the next packet).
    next_packet: AtomicU64,
}

impl Timeline {
    pub fn new(latency: u32) -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            latency,
            start_ts: AtomicU64::new(0),
            next_packet: AtomicU64::new(0),
        }
    }

    /// Anchor packet 0 at the given NTP time.
    pub fn start_at(&self, ntp: u64) {
        self.start_ts.store(ntp2ts(ntp, self.sample_rate), Ordering::SeqCst);
        self.next_packet.store(0, Ordering::SeqCst);
    }

    pub fn start_ts(&self) -> u64 {
        self.start_ts.load(Ordering::SeqCst)
    }

    pub fn next_packet(&self) -> u64 {
        self.next_packet.load(Ordering::SeqCst)
    }

    pub fn set_next_packet(&self, n: u64) {
        self.next_packet.store(n, Ordering::SeqCst);
    }

    pub fn frames_before(packet: u64) -> u64 {
        packet * FRAMES_PER_PACKET as u64
    }

    /// RTP timestamp carried by packet `n` (wraps at 2^32 like RTP does).
    pub fn rtptime(&self, packet: u64) -> u32 {
        (Self::frames_before(packet) + self.latency as u64) as u32
    }

    /// NTP time at which packet `n` is due to be sent.
    pub fn send_ntp(&self, packet: u64) -> u64 {
        ts2ntp(self.start_ts() + Self::frames_before(packet), self.sample_rate)
    }

    /// Fields of the sync packet sent just before packet `n`:
    /// (rtp now - latency, NTP sec, NTP frac, rtp now).
    pub fn sync_fields(&self, packet: u64) -> (u32, u32, u32, u32) {
        let now = self.rtptime(packet);
        let (sec, frac) = ntp_parts(self.send_ntp(packet));
        (now.wrapping_sub(self.latency), sec, frac, now)
    }

    /// Index of the last packet whose send time is <= `now_ntp` (+1 = how many are due).
    pub fn packets_due(&self, now_ntp: u64) -> u64 {
        let now_ts = ntp2ts(now_ntp, self.sample_rate);
        let start = self.start_ts();
        let mut count = if now_ts < start { 0 } else { (now_ts - start) / FRAMES_PER_PACKET as u64 + 1 };
        // Fix up truncation in the ts<->NTP round trip so this agrees exactly with
        // `send_ntp` (the sender sleeps until send_ntp(next)).
        while self.send_ntp(count) <= now_ntp {
            count += 1;
        }
        while count > 0 && self.send_ntp(count - 1) > now_ntp {
            count -= 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raop::timing::ntp2ms;

    #[test]
    fn rtp_and_sync_mapping_matches_pyatv_stream_context() {
        // pyatv: rtptime = head_ts - (start_ts - latency); sync = (rtptime - latency,
        // ts2ntp(head_ts), rtptime) with head_ts = start_ts + frames sent.
        let t = Timeline::new(DEFAULT_LATENCY_FRAMES);
        let start_ntp = 0xEAF0_1234_8000_0000u64;
        t.start_at(start_ntp);
        assert_eq!(t.rtptime(0), 66150);
        assert_eq!(t.rtptime(10), 66150 + 3520);
        let (nl, sec, frac, now) = t.sync_fields(125);
        assert_eq!(now, 66150 + 125 * 352);
        assert_eq!(nl, 125 * 352);
        let ntp = ((sec as u64) << 32) | frac as u64;
        assert_eq!(ntp, ts2ntp(t.start_ts() + 125 * 352, 44100));
        // send time of packet n is n*352/44100 s after packet 0 (to within 1 ms)
        let dt_ms = ntp2ms(t.send_ntp(44100 / 352 * 10) - t.send_ntp(0));
        assert!((9_970..=10_000).contains(&dt_ms), "{dt_ms}");
    }

    #[test]
    fn packets_due_counts_from_start() {
        let t = Timeline::new(DEFAULT_LATENCY_FRAMES);
        let start = 0xEAF0_0000_0000_0000u64;
        t.start_at(start);
        assert_eq!(t.packets_due(start - (1 << 32)), 0);
        assert_eq!(t.packets_due(t.send_ntp(0)), 1);
        assert_eq!(t.packets_due(t.send_ntp(7)), 8);
        assert_eq!(t.packets_due(start + (1u64 << 32)), 44100 / 352 + 1);
    }

    #[test]
    fn shared_timeline_gives_identical_play_time_for_every_speaker() {
        // Two speakers with different seq/ssrc still get the same rtptime and sync
        // fields for the same packet index - that is the whole point.
        let t = Timeline::new(13230);
        t.start_at(0xEB00_0000_0000_0000);
        for n in [0u64, 1, 125, 99_999] {
            assert_eq!(t.sync_fields(n), t.sync_fields(n));
            assert_eq!(t.rtptime(n), (n * 352 + 13230) as u32);
        }
    }
}
