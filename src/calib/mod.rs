//! Acoustic auto-calibration: measure each speaker with exponential sine sweeps
//! played at exact positions of the shared timeline, recorded by a mic at the
//! listening position, and derive delay / level / EQ corrections.
//!
//! ## Procedure (driven by `controller`, analysed here)
//!
//! 1. The programme is muted (ramped) on every speaker.
//! 2. A Farina exponential sine sweep ([`SweepSpec`], 20 Hz..20 kHz, raised-cosine
//!    fades) is scheduled as a [`crate::dsp::Probe`] on one speaker at a time:
//!    speaker 0, speaker 1, speaker 0, speaker 1, ... every [`Schedule::period_frames`]
//!    timeline frames ([`Schedule`]). The probes bypass the speaker's delay / EQ / trim,
//!    so the raw speaker is measured.
//! 3. The mic records continuously (its own clock, arbitrary start).
//! 4. [`analysis::analyze`]: regularised spectral division by the sweep gives the
//!    impulse response train; the known timeline offsets between sweeps identify each
//!    sweep's IR and give the mic-vs-timeline clock ratio (so no clock mapping is
//!    needed and mic clock drift is removed); per-repeat GCC-PHAT between the IRs gives
//!    sub-sample relative arrival times; band levels (200 Hz..4 kHz) and smoothed
//!    responses come from the IR spectra; repeats give consistency figures.
//! 5. [`fit::propose`]: delay for the earlier speaker(s), a trim for the louder, and
//!    a few conservative peaking bands per speaker towards a common smooth target.

pub mod analysis;
pub mod fft;
pub mod fit;
pub mod report;
#[cfg(test)]
mod sim;

use std::f64::consts::PI;

use serde::{Deserialize, Serialize};

/// Rate of the shared timeline (frames per second).
pub const TIMELINE_RATE: f64 = 44100.0;
/// Band used for level matching and for the timing estimate.
pub const LEVEL_BAND: (f64, f64) = (200.0, 4000.0);
pub const GRID_LO: f64 = 20.0;
pub const GRID_HI: f64 = 20000.0;
pub const GRID_PER_OCTAVE: f64 = 24.0;

pub use analysis::{Analysis, Recording, analyze};
pub use fit::{Proposal, propose};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepSpec {
    pub f1: f64,
    pub f2: f64,
    pub seconds: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Peak level of the sweep in dBFS.
    pub level_dbfs: f64,
}

impl SweepSpec {
    pub fn new(seconds: f64, level_dbfs: f64) -> Self {
        Self { f1: 20.0, f2: 20000.0, seconds, fade_in: 0.05, fade_out: 0.01, level_dbfs }
    }

    pub fn amplitude(&self) -> f64 {
        10f64.powf(self.level_dbfs / 20.0)
    }

    /// The sweep as a function of time (seconds from its start); 0 outside.
    pub fn value(&self, t: f64) -> f64 {
        if !(0.0..self.seconds).contains(&t) {
            return 0.0;
        }
        let l = self.seconds / (self.f2 / self.f1).ln();
        let phase = 2.0 * PI * self.f1 * l * ((t / l).exp() - 1.0);
        let mut env = 1.0;
        if t < self.fade_in {
            env *= 0.5 - 0.5 * (PI * t / self.fade_in).cos();
        }
        let left = self.seconds - t;
        if left < self.fade_out {
            env *= 0.5 - 0.5 * (PI * left / self.fade_out).cos();
        }
        self.amplitude() * env * phase.sin()
    }

    /// Sampled at `rate` (e.g. the timeline rate for playback).
    pub fn render(&self, rate: f64) -> Vec<f64> {
        let n = (self.seconds * rate).ceil() as usize;
        (0..n).map(|i| self.value(i as f64 / rate)).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub speaker: usize,
    pub repeat: usize,
    /// Timeline frames from the start of the first sweep.
    pub offset: u64,
}

/// Which speaker plays a sweep when: round-robin over the speakers, `repeats` times.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub sweep: SweepSpec,
    pub speakers: usize,
    pub repeats: usize,
    pub period_frames: u64,
    pub slots: Vec<Slot>,
}

impl Schedule {
    pub fn new(sweep: SweepSpec, speakers: usize, repeats: usize, gap_seconds: f64) -> Self {
        let period_frames = ((sweep.seconds + gap_seconds) * TIMELINE_RATE).round() as u64;
        let mut slots = Vec::new();
        for repeat in 0..repeats {
            for speaker in 0..speakers {
                slots.push(Slot { speaker, repeat, offset: slots.len() as u64 * period_frames });
            }
        }
        Self { sweep, speakers, repeats, period_frames, slots }
    }

    pub fn sweep_frames(&self) -> u64 {
        (self.sweep.seconds * TIMELINE_RATE).ceil() as u64
    }

    /// Frames from the first sweep's start to the last sweep's end.
    pub fn total_frames(&self) -> u64 {
        self.slots.last().map(|s| s.offset).unwrap_or(0) + self.sweep_frames()
    }

    /// The slot of `speaker`'s `repeat`-th sweep.
    pub fn slot_index(&self, speaker: usize, repeat: usize) -> Option<usize> {
        self.slots.iter().position(|s| s.speaker == speaker && s.repeat == repeat)
    }
}

/// Log-spaced analysis / plot frequencies (1/24 octave, 20 Hz..20 kHz).
pub fn grid() -> Vec<f64> {
    let n = ((GRID_HI / GRID_LO).log2() * GRID_PER_OCTAVE).floor() as usize;
    (0..=n).map(|i| GRID_LO * 2f64.powf(i as f64 / GRID_PER_OCTAVE)).collect()
}

/// Fractional-octave smoothing of a power spectrum (`power[k]` at `k * bin_hz`) onto
/// `grid`: the mean power over `[f / 2^(1/2n), f * 2^(1/2n)]`, in dB (n = 6 for
/// 1/6 octave).
pub fn smooth_db(power: &[f64], bin_hz: f64, grid: &[f64], n: f64) -> Vec<f64> {
    let mut prefix = Vec::with_capacity(power.len() + 1);
    prefix.push(0.0);
    for p in power {
        prefix.push(prefix.last().unwrap() + p);
    }
    let half = 2f64.powf(1.0 / (2.0 * n));
    grid.iter()
        .map(|&f| {
            let lo = ((f / half / bin_hz).ceil() as usize).min(power.len() - 1);
            let hi = ((f * half / bin_hz).floor() as usize).min(power.len() - 1);
            let mean = if hi >= lo {
                (prefix[hi + 1] - prefix[lo]) / (hi + 1 - lo) as f64
            } else {
                // narrower than a bin: interpolate between the neighbours
                let x = f / bin_hz;
                let k = (x.floor() as usize).min(power.len() - 2);
                let t = x - k as f64;
                power[k] * (1.0 - t) + power[k + 1] * t
            };
            10.0 * mean.max(1e-30).log10()
        })
        .collect()
}

/// Mean power (in dB) of the grid points inside `band`.
pub fn band_level_db(grid: &[f64], db: &[f64], band: (f64, f64)) -> f64 {
    let (sum, n) = grid
        .iter()
        .zip(db)
        .filter(|(f, _)| **f >= band.0 && **f <= band.1)
        .fold((0.0, 0usize), |(s, n), (_, d)| (s + 10f64.powf(d / 10.0), n + 1));
    10.0 * (sum / n.max(1) as f64).max(1e-30).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_is_an_exponential_chirp_with_fades() {
        let s = SweepSpec::new(2.0, -20.0);
        assert!((s.amplitude() - 0.1).abs() < 1e-12);
        let x = s.render(TIMELINE_RATE);
        assert_eq!(x.len(), 88200);
        assert_eq!(x[0], 0.0, "fade-in starts at silence");
        let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= 0.1 + 1e-12 && peak > 0.099);
        assert!(x.last().unwrap().abs() < 1e-3, "fade-out ends near silence");
        // instantaneous frequency f1 * e^(t/L): an octave every L*ln 2 seconds
        let l = 2.0 / 1000f64.ln();
        let t_1k = l * (1000.0f64 / 20.0).ln();
        let i = (t_1k * TIMELINE_RATE) as usize;
        let zero_crossings = x[i - 441..i + 441].windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((19..=21).contains(&zero_crossings), "~1 kHz around t_1k: {zero_crossings} cycles in 20 ms");
        // the rendered samples are the continuous sweep sampled at the timeline rate
        assert_eq!(x[1234], s.value(1234.0 / TIMELINE_RATE));
    }

    #[test]
    fn schedule_round_robins_the_speakers() {
        let s = Schedule::new(SweepSpec::new(3.0, -20.0), 2, 3, 1.5);
        assert_eq!(s.period_frames, 198450);
        let order: Vec<(usize, usize)> = s.slots.iter().map(|x| (x.speaker, x.repeat)).collect();
        assert_eq!(order, [(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)]);
        assert_eq!(s.slots[3].offset, 3 * 198450);
        assert_eq!(s.total_frames(), 5 * 198450 + 132300);
        assert_eq!(s.slot_index(1, 2), Some(5));
    }

    #[test]
    fn grid_and_smoothing() {
        let g = grid();
        assert_eq!(g[0], 20.0);
        assert!(*g.last().unwrap() > 19000.0 && *g.last().unwrap() <= 20000.0);
        assert_eq!(g.len(), 240);
        // flat power -> 0 dB everywhere; band level of a flat curve is 0
        let flat = vec![1.0; 4097];
        let s = smooth_db(&flat, 48000.0 / 8192.0, &g, 6.0);
        assert!(s.iter().all(|v| v.abs() < 1e-9));
        assert!(band_level_db(&g, &s, LEVEL_BAND).abs() < 1e-9);
    }
}
