//! Test-only room simulator: renders what a mic would record when the calibration
//! schedule plays through speakers with known delays, levels, filters (resonances,
//! notches, roll-offs) and a reflection, with mic clock drift, an arbitrary recording
//! start and noise. The sweeps are evaluated analytically at the mic's sample times
//! (the speakers reproduce the band-limited 44.1 kHz sweep, i.e. the continuous one).

use super::analysis::Recording;
use super::{Schedule, TIMELINE_RATE};
use crate::dsp::biquad::{Coeffs, FilterKind, State};

#[derive(Clone, Debug, Default)]
pub struct SimSpeaker {
    /// Arrival delay at the mic (ms, any value >= 0).
    pub delay_ms: f64,
    pub gain_db: f64,
    /// (kind, freq, gain dB, Q), designed at the mic rate.
    pub filters: Vec<(FilterKind, f64, f64, f64)>,
    /// One reflection: (extra delay ms, level dB re. direct).
    pub reflection: Option<(f64, f64)>,
}

impl SimSpeaker {
    /// The magnitude response this speaker has at the mic (dB).
    pub fn response_db(&self, f: f64, fs: f64) -> f64 {
        let mut db = self.gain_db;
        for &(k, fc, g, q) in &self.filters {
            db += Coeffs::design(k, fc, g, q, fs).magnitude_db(f, fs);
        }
        if let Some((ms, rdb)) = self.reflection {
            let a = 10f64.powf(rdb / 20.0);
            let w = 2.0 * std::f64::consts::PI * f * ms / 1000.0;
            db += 10.0 * (1.0 + a * a + 2.0 * a * w.cos()).log10();
        }
        db
    }
}

struct Lcg(u64);

impl Lcg {
    fn uniform(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Roughly Gaussian, unit variance (Irwin-Hall with 4 terms).
    fn gauss(&mut self) -> f64 {
        let s: f64 = (0..4).map(|_| self.uniform()).sum();
        (s - 2.0) * 3f64.sqrt()
    }
}

/// Record `sched` played through `speakers` with a mic at `fs` whose clock runs
/// `ppm` fast, starting `lead_s` before the first sweep leaves the speakers, with
/// white noise at `noise_dbfs` RMS; the mic is on channel `mic_ch` of two.
#[allow(clippy::too_many_arguments)]
pub fn simulate(speakers: &[SimSpeaker], sched: &Schedule, fs: f64, ppm: f64, lead_s: f64, noise_dbfs: f64, mic_ch: usize, seed: u64) -> Recording {
    let max_delay = speakers.iter().map(|s| s.delay_ms + s.reflection.map_or(0.0, |r| r.0)).fold(0.0, f64::max) / 1000.0;
    let seconds = lead_s + sched.total_frames() as f64 / TIMELINE_RATE + max_delay + 1.0;
    let mic_hz = fs * (1.0 + ppm * 1e-6);
    let n = (seconds * fs) as usize;
    let mut mix = vec![0.0f64; n];
    let sweep = &sched.sweep;
    for (j, spk) in speakers.iter().enumerate() {
        let mut y = vec![0.0f64; n];
        let mut paths = vec![(spk.delay_ms / 1000.0, 1.0)];
        if let Some((ms, db)) = spk.reflection {
            paths.push(((spk.delay_ms + ms) / 1000.0, 10f64.powf(db / 20.0)));
        }
        for slot in sched.slots.iter().filter(|s| s.speaker == j) {
            for &(delay, amp) in &paths {
                let t0 = lead_s + slot.offset as f64 / TIMELINE_RATE + delay;
                let a = ((t0 * mic_hz).floor() as usize).min(n);
                let b = (((t0 + sweep.seconds) * mic_hz).ceil() as usize + 1).min(n);
                for (i, v) in y.iter_mut().enumerate().take(b).skip(a) {
                    *v += amp * sweep.value(i as f64 / mic_hz - t0);
                }
            }
        }
        for &(k, fc, g, q) in &spk.filters {
            let c = Coeffs::design(k, fc, g, q, fs);
            let mut st = State::default();
            y.iter_mut().for_each(|v| *v = st.process(&c, *v));
        }
        let g = 10f64.powf(spk.gain_db / 20.0);
        mix.iter_mut().zip(&y).for_each(|(m, v)| *m += g * v);
    }
    let mut rng = Lcg(seed);
    let noise = 10f64.powf(noise_dbfs / 20.0);
    let quiet = 10f64.powf(-100.0 / 20.0);
    let mut channels = vec![Vec::with_capacity(n), Vec::with_capacity(n)];
    for m in mix {
        let mic = (m + noise * rng.gauss()) as f32;
        let other = (quiet * rng.gauss()) as f32;
        let (a, b) = if mic_ch == 0 { (mic, other) } else { (other, mic) };
        channels[0].push(a);
        channels[1].push(b);
    }
    Recording { rate: fs, channels }
}
