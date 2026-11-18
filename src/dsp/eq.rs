//! Per-speaker parametric EQ: settings (persisted in config.toml) and the runtime
//! biquad chain run by [`super::SpeakerDsp`].
//!
//! Click-free live edits: each band's parameters (log frequency, gain in dB, log Q)
//! glide exponentially towards new values (time constant [`GLIDE_SEC`]) and the
//! coefficients are redesigned from the gliding parameters every [`BLOCK`] frames, so
//! every intermediate filter is a valid, stable RBJ design. Enabling, disabling,
//! bypassing, adding or removing a band crossfades its output with its input (wet/dry
//! mix ramped over [`MIX_SEC`]); changing a band's type fades it out, swaps the type
//! and fades it back in.
//!
//! Bands are matched to runtime slots by position. Removing a band in the middle of
//! the list therefore makes the following slots glide to their new neighbour's
//! parameters (still click-free; the chain is linear, so the order of the bands does
//! not change the result once the glide has settled).

use serde::{Deserialize, Serialize};

use super::biquad::{Coeffs, FilterKind, State};

pub const MAX_BANDS: usize = 10;
pub const GLIDE_SEC: f64 = 0.015;
pub const MIX_SEC: f64 = 0.02;
pub const BLOCK: usize = 32;

fn default_q() -> f32 {
    std::f32::consts::FRAC_1_SQRT_2
}
fn default_true() -> bool {
    true
}

/// Where a band came from: added by hand, or proposed by the calibration routine
/// (re-running calibration replaces only the `calibration` bands).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BandSource {
    #[default]
    Manual,
    Calibration,
}

impl BandSource {
    fn is_manual(&self) -> bool {
        *self == BandSource::Manual
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    #[serde(rename = "type", default)]
    pub kind: FilterKind,
    pub freq_hz: f32,
    /// Peaking / shelves only (ignored for high-/low-pass).
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default = "default_q")]
    pub q: f32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BandSource::is_manual")]
    pub source: BandSource,
}

impl EqBand {
    pub fn peaking(freq_hz: f32, gain_db: f32, q: f32) -> Self {
        Self { kind: FilterKind::Peaking, freq_hz, gain_db, q, enabled: true, source: BandSource::Manual }
    }

    pub fn coeffs(&self, sample_rate: f64) -> Coeffs {
        Coeffs::design(self.kind, self.freq_hz as f64, self.gain_db as f64, self.q as f64, sample_rate)
    }
}

impl Default for EqBand {
    fn default() -> Self {
        Self::peaking(1000.0, 0.0, 1.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct EqSettings {
    /// Bypass the whole chain (bands are kept).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass: bool,
    #[serde(default, rename = "band", skip_serializing_if = "Vec::is_empty")]
    pub bands: Vec<EqBand>,
}

impl EqSettings {
    pub fn is_default(&self) -> bool {
        *self == EqSettings::default()
    }

    /// Bands that currently shape the sound.
    pub fn active(&self) -> impl Iterator<Item = &EqBand> {
        self.bands.iter().filter(move |b| !self.bypass && b.enabled)
    }

    /// Magnitude response of the chain at `freq` (0 dB when bypassed).
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        self.active().map(|b| b.coeffs(sample_rate).magnitude_db(freq, sample_rate)).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Params {
    /// ln(frequency)
    lf: f64,
    gain_db: f64,
    /// ln(Q)
    lq: f64,
}

impl Params {
    fn of(b: &EqBand) -> Self {
        let f = if b.freq_hz.is_finite() { (b.freq_hz as f64).clamp(10.0, 22000.0) } else { 1000.0 };
        let q = if b.q.is_finite() { (b.q as f64).clamp(0.1, 20.0) } else { 0.707 };
        let g = if b.gain_db.is_finite() { (b.gain_db as f64).clamp(-24.0, 24.0) } else { 0.0 };
        Self { lf: f.ln(), gain_db: g, lq: q.ln() }
    }

    fn close_to(&self, o: &Params) -> bool {
        (self.lf - o.lf).abs() < 1e-5 && (self.gain_db - o.gain_db).abs() < 1e-4 && (self.lq - o.lq).abs() < 1e-5
    }
}

struct Slot {
    kind: FilterKind,
    cur: Params,
    target: Params,
    /// Type change waiting for the fade-out to finish.
    next_kind: Option<FilterKind>,
    mix: f64,
    /// 1.0 when the band should sound, 0.0 when bypassed / disabled / removed.
    want: f64,
    removed: bool,
    coeffs: Coeffs,
    state: [State; 2],
}

impl Slot {
    fn new(b: &EqBand, want: f64, fs: f64) -> Self {
        let p = Params::of(b);
        let mut s = Slot {
            kind: b.kind,
            cur: p,
            target: p,
            next_kind: None,
            mix: 0.0,
            want,
            removed: false,
            coeffs: Coeffs::IDENTITY,
            state: [State::default(); 2],
        };
        s.redesign(fs);
        s
    }

    fn redesign(&mut self, fs: f64) {
        self.coeffs = Coeffs::design(self.kind, self.cur.lf.exp(), self.cur.gain_db, self.cur.lq.exp(), fs);
    }

    fn mix_goal(&self) -> f64 {
        if self.next_kind.is_some() || self.removed { 0.0 } else { self.want }
    }

    fn silent(&self) -> bool {
        self.mix == 0.0 && self.mix_goal() == 0.0
    }
}

/// The runtime chain (stereo: both channels of the routed frame).
pub struct EqChain {
    fs: f64,
    slots: Vec<Slot>,
    glide_k: f64,
    mix_step: f64,
}

impl EqChain {
    pub fn new(settings: &EqSettings, sample_rate: u32) -> Self {
        let fs = sample_rate as f64;
        let mut chain = Self {
            fs,
            slots: Vec::new(),
            glide_k: 1.0 - (-(BLOCK as f64) / (GLIDE_SEC * fs)).exp(),
            mix_step: 1.0 / (MIX_SEC * fs),
        };
        chain.update(settings);
        // Initial settings take effect at once (no fade-in on stream start).
        for s in chain.slots.iter_mut() {
            s.mix = s.want;
        }
        chain
    }

    /// Retarget the chain; the change is glided / crossfaded by `process`.
    pub fn update(&mut self, settings: &EqSettings) {
        for (i, b) in settings.bands.iter().take(MAX_BANDS).enumerate() {
            let want = if !settings.bypass && b.enabled { 1.0 } else { 0.0 };
            match self.slots.get_mut(i) {
                Some(slot) => {
                    slot.removed = false;
                    slot.want = want;
                    slot.target = Params::of(b);
                    if b.kind != slot.kind {
                        if slot.mix == 0.0 {
                            // inaudible: swap at once
                            slot.kind = b.kind;
                            slot.cur = slot.target;
                            slot.next_kind = None;
                            slot.state = [State::default(); 2];
                            slot.redesign(self.fs);
                        } else {
                            slot.next_kind = Some(b.kind);
                        }
                    } else {
                        slot.next_kind = None;
                    }
                }
                None => self.slots.push(Slot::new(b, want, self.fs)),
            }
        }
        for slot in self.slots.iter_mut().skip(settings.bands.len().min(MAX_BANDS)) {
            slot.removed = true;
        }
    }

    /// True if the chain currently does nothing (all bands silent).
    pub fn is_idle(&self) -> bool {
        self.slots.iter().all(Slot::silent)
    }

    pub fn process(&mut self, buf: &mut [[f64; 2]]) {
        if self.is_idle() {
            self.slots.retain(|s| !s.removed);
            return;
        }
        let fs = self.fs;
        for chunk in buf.chunks_mut(BLOCK) {
            for slot in self.slots.iter_mut() {
                if slot.silent() {
                    continue;
                }
                // parameter glide (only while the band sounds; otherwise snap)
                if !slot.cur.close_to(&slot.target) {
                    if slot.mix == 0.0 {
                        slot.cur = slot.target;
                    } else {
                        let k = self.glide_k * (chunk.len() as f64 / BLOCK as f64);
                        slot.cur.lf += (slot.target.lf - slot.cur.lf) * k;
                        slot.cur.gain_db += (slot.target.gain_db - slot.cur.gain_db) * k;
                        slot.cur.lq += (slot.target.lq - slot.cur.lq) * k;
                        if slot.cur.close_to(&slot.target) {
                            slot.cur = slot.target;
                        }
                    }
                    slot.redesign(fs);
                }
                let goal = slot.mix_goal();
                for x in chunk.iter_mut() {
                    if slot.mix != goal {
                        slot.mix = if slot.mix < goal {
                            (slot.mix + self.mix_step).min(goal)
                        } else {
                            (slot.mix - self.mix_step).max(goal)
                        };
                    }
                    let c = &slot.coeffs;
                    let y0 = slot.state[0].process(c, x[0]);
                    let y1 = slot.state[1].process(c, x[1]);
                    x[0] += slot.mix * (y0 - x[0]);
                    x[1] += slot.mix * (y1 - x[1]);
                }
                if slot.mix == 0.0 {
                    // faded out: reset, and complete a pending type change
                    slot.state = [State::default(); 2];
                    if let Some(k) = slot.next_kind.take() {
                        slot.kind = k;
                        slot.cur = slot.target;
                        slot.redesign(fs);
                    }
                }
            }
        }
        for slot in self.slots.iter_mut() {
            slot.state.iter_mut().for_each(State::flush_denormals);
        }
        // drop removed bands once faded out (they are always at the end)
        while self.slots.last().is_some_and(|s| s.removed && s.mix == 0.0) {
            self.slots.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const FS: u32 = 44100;

    fn sine(f: f64, start: usize, n: usize) -> Vec<[f64; 2]> {
        (start..start + n)
            .map(|i| {
                let s = 0.5 * (2.0 * PI * f * i as f64 / FS as f64).sin();
                [s, s]
            })
            .collect()
    }

    fn rms(b: &[[f64; 2]]) -> f64 {
        (b.iter().map(|x| x[0] * x[0]).sum::<f64>() / b.len() as f64).sqrt()
    }

    #[test]
    fn chain_matches_the_designed_response() {
        let eq = EqSettings {
            bypass: false,
            bands: vec![
                EqBand::peaking(1000.0, 6.0, 1.0),
                EqBand { kind: FilterKind::HighPass, freq_hz: 80.0, q: 0.707, ..Default::default() },
            ],
        };
        let mut chain = EqChain::new(&eq, FS);
        let mut buf = sine(1000.0, 0, 44100);
        chain.process(&mut buf);
        let gain = 20.0 * (rms(&buf[22050..]) / (0.5 / 2f64.sqrt())).log10();
        let expect = eq.response_db(1000.0, FS as f64);
        assert!((gain - expect).abs() < 0.05, "{gain} vs {expect}");
        assert!((expect - 6.0).abs() < 0.1);
        // bypass: unity (after the fade)
        let mut by = eq.clone();
        by.bypass = true;
        chain.update(&by);
        let mut buf = sine(1000.0, 44100, 8820);
        chain.process(&mut buf);
        assert!(chain.is_idle());
        let mut buf = sine(1000.0, 52920, 4410);
        let dry = buf.clone();
        chain.process(&mut buf);
        assert_eq!(buf, dry, "bypassed chain is bit-transparent");
        assert_eq!(by.response_db(1000.0, FS as f64), 0.0);
    }

    /// Max sample-to-sample step of the output; a click shows up as a jump well above
    /// what the sine itself does.
    fn max_step(out: &[[f64; 2]], prev: &mut Option<f64>) -> f64 {
        let mut m: f64 = 0.0;
        for x in out {
            if let Some(p) = *prev {
                m = m.max((x[0] - p).abs());
            }
            *prev = Some(x[0]);
        }
        m
    }

    #[test]
    fn live_edits_are_click_free() {
        // 200 Hz sine at 0.5 peak moves at most 0.0142 per sample; edits must not add
        // jumps: change gain/freq/type, add/remove/disable bands, toggle bypass,
        // every 10 ms, for two seconds.
        let f = 200.0;
        let mut eq = EqSettings::default();
        let mut chain = EqChain::new(&eq, FS);
        let mut prev = None;
        let mut worst: f64 = 0.0;
        let mut pos = 0;
        for k in 0..200usize {
            match k % 10 {
                0 => eq.bands.push(EqBand::peaking(2000.0 + 300.0 * k as f32, 3.0, 1.0)),
                1 => eq.bands[0].gain_db = -12.0 + (k % 7) as f32 * 3.0,
                2 => eq.bands[0].freq_hz = 100.0 + (k * 37 % 400) as f32,
                3 => eq.bands[0].kind = FilterKind::ALL[k % 5],
                4 => eq.bypass = !eq.bypass,
                5 => eq.bands[0].q = 0.5 + (k % 3) as f32 * 0.5,
                6 => {
                    let n = eq.bands.len();
                    eq.bands[n - 1].enabled = !eq.bands[n - 1].enabled
                }
                7 if eq.bands.len() > 2 => {
                    eq.bands.remove(1);
                }
                _ => {}
            }
            chain.update(&eq);
            let mut buf = sine(f, pos, 441);
            pos += 441;
            chain.process(&mut buf);
            worst = worst.max(max_step(&buf, &mut prev));
            assert!(buf.iter().all(|x| x[0].is_finite() && x[0].abs() < 4.0));
        }
        // a +-12 dB band at 200 Hz can scale the sine by 4: 0.057 per sample. A click
        // (instant swap of a resonant filter) is far bigger than that.
        assert!(worst < 0.09, "discontinuity {worst}");
        // after the edits stop, the chain settles on exactly the requested response
        let mut buf = sine(f, pos, 44100);
        chain.process(&mut buf);
        let mut buf = sine(f, pos + 44100, 44100);
        chain.process(&mut buf);
        let gain = 20.0 * (rms(&buf[22050..]) / (0.5 / 2f64.sqrt())).log10();
        let expect = eq.response_db(f, FS as f64);
        assert!((gain - expect).abs() < 0.1, "settled {gain} vs {expect}");
    }

    #[test]
    fn removed_bands_fade_out_and_are_dropped() {
        let mut eq = EqSettings { bypass: false, bands: vec![EqBand::peaking(500.0, -6.0, 1.0); 3] };
        let mut chain = EqChain::new(&eq, FS);
        assert_eq!(chain.slots.len(), 3);
        eq.bands.truncate(1);
        chain.update(&eq);
        let mut buf = sine(500.0, 0, 4410);
        chain.process(&mut buf);
        assert_eq!(chain.slots.len(), 1);
        // too many bands are ignored beyond MAX_BANDS
        eq.bands = vec![EqBand::peaking(500.0, -1.0, 1.0); MAX_BANDS + 3];
        chain.update(&eq);
        assert_eq!(chain.slots.len(), MAX_BANDS);
    }

    #[test]
    fn eq_settings_serde_defaults() {
        let b: EqBand = toml::from_str("freq_hz = 120.0").unwrap();
        assert_eq!(b.kind, FilterKind::Peaking);
        assert!(b.enabled);
        assert_eq!(b.source, BandSource::Manual);
        let t = toml::to_string(&EqBand { source: BandSource::Calibration, ..b.clone() }).unwrap();
        assert!(t.contains("source = \"calibration\""), "{t}");
        assert!(!toml::to_string(&b).unwrap().contains("source"));
        let s: EqSettings = toml::from_str("[[band]]\ntype = \"high_pass\"\nfreq_hz = 60.0\n").unwrap();
        assert_eq!(s.bands[0].kind, FilterKind::HighPass);
        assert!(!s.bypass);
    }
}
