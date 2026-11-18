//! NTP time base and RTP timestamp math (pyatv `protocols/raop/timing.py`).
//!
//! There is exactly ONE [`NtpClock`] per process. The NTP timing server answers
//! receivers' clock queries from it, the sender paces packets on it, and sync packets
//! map RTP time onto it - so every speaker locks to the same clock.
//!
//! The clock is monotonic (QueryPerformanceCounter via `Instant`) anchored to the wall
//! clock once at start, so a Windows time sync mid-stream cannot make it jump.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seconds between 1900-01-01 (NTP epoch) and 1970-01-01.
const NTP_UNIX_OFFSET: u64 = 0x83AA_7E80;

pub struct NtpClock {
    base_ntp: u64,
    base: Instant,
}

impl NtpClock {
    fn new() -> Self {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let base = Instant::now();
        Self { base_ntp: duration_to_ntp(wall) + (NTP_UNIX_OFFSET << 32), base }
    }

    /// Current time as 64-bit NTP (32.32 fixed point).
    pub fn now(&self) -> u64 {
        self.base_ntp + duration_to_ntp(self.base.elapsed())
    }

    pub fn instant_of(&self, ntp: u64) -> Instant {
        if ntp >= self.base_ntp {
            self.base + ntp_to_duration(ntp - self.base_ntp)
        } else {
            self.base
        }
    }
}

/// The process-wide clock.
pub fn clock() -> &'static NtpClock {
    static CLOCK: OnceLock<NtpClock> = OnceLock::new();
    CLOCK.get_or_init(NtpClock::new)
}

pub fn duration_to_ntp(d: Duration) -> u64 {
    (d.as_secs() << 32) + (((d.subsec_nanos() as u64) << 32) / 1_000_000_000)
}

pub fn ntp_to_duration(ntp: u64) -> Duration {
    let secs = ntp >> 32;
    let nanos = ((ntp & 0xFFFF_FFFF) * 1_000_000_000) >> 32;
    Duration::new(secs, nanos as u32)
}

pub fn ntp_parts(ntp: u64) -> (u32, u32) {
    ((ntp >> 32) as u32, ntp as u32)
}

/// NTP time -> sample timestamp (`int((ntp >> 16) * rate) >> 16`).
pub fn ntp2ts(ntp: u64, rate: u32) -> u64 {
    (((ntp >> 16) as u128 * rate as u128) >> 16) as u64
}

/// Sample timestamp -> NTP time (`int((ts << 16) / rate) << 16`). Exact integer
/// division here; pyatv divides as a float, which can differ by one unit of 2^-16 s
/// for absolute (since-1900) timestamps.
pub fn ts2ntp(ts: u64, rate: u32) -> u64 {
    ((((ts as u128) << 16) / rate as u128) << 16) as u64
}

#[cfg(test)]
pub fn ntp2ms(ntp: u64) -> u64 {
    (((ntp >> 10) as u128 * 1000) >> 22) as u64
}

#[cfg(test)]
pub fn ts2ms(ts: u64, rate: u32) -> u64 {
    ntp2ms(ts2ntp(ts, rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::vectors;

    #[test]
    fn conversions_match_pyatv() {
        let v = vectors();
        let t = &v["timing"];
        for pair in t["ntp2ts"].as_array().unwrap() {
            assert_eq!(ntp2ts(pair[0].as_u64().unwrap(), 44100), pair[1].as_u64().unwrap());
        }
        for pair in t["ts2ntp"].as_array().unwrap() {
            let got = ts2ntp(pair[0].as_u64().unwrap(), 44100);
            let want = pair[1].as_u64().unwrap();
            // float division in pyatv: allow one 2^-16 s step on huge values
            assert!(got.abs_diff(want) <= 1 << 16, "ts2ntp({}) = {got:#x}, pyatv {want:#x}", pair[0]);
        }
        for pair in t["ts2ms"].as_array().unwrap() {
            assert_eq!(ts2ms(pair[0].as_u64().unwrap(), 44100), pair[1].as_u64().unwrap());
        }
    }

    #[test]
    fn clock_is_monotonic_and_round_trips() {
        let c = clock();
        let a = c.now();
        std::thread::sleep(Duration::from_millis(5));
        let b = c.now();
        assert!(b > a);
        let d = ntp_to_duration(b - a);
        assert!(d >= Duration::from_millis(4) && d < Duration::from_millis(500));
        let ms = ntp_to_duration(duration_to_ntp(Duration::from_micros(1_234_567)));
        assert!(ms.as_micros().abs_diff(1_234_567) <= 1);
    }
}
