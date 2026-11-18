//! Pair-wide spatial stage: runs once on the shared stereo input in the sender, before
//! each speaker's routing / EQ / gain / delay (see `engine::spawn_sender`).
//!
//! Order: bass mono -> stereo width -> crosstalk cancellation -> balance.
//!
//! * **Bass mono**: Linkwitz-Riley 24 dB/oct crossover (two cascaded Butterworth
//!   biquads per side); below the crossover both channels get (L+R)/2, above it they
//!   stay stereo. LR4 low + high sum to an all-pass, so the magnitude stays flat.
//! * **Width**: mid/side, side scaled by 0..2 (0 % = mono, 100 % = unchanged,
//!   200 % = side doubled).
//! * **Crosstalk cancellation (experimental "3D")**: RACE-style recursive canceller
//!   (Glasgal's Recursive Ambiophonic Crosstalk Elimination): each output subtracts the
//!   other output, delayed by the extra path length to the far ear and attenuated by
//!   `a`, band-limited to 200 Hz..6 kHz. The delay comes from the speaker spacing and
//!   listening distance (Woodworth spherical-head model, head radius 8.75 cm), `a`
//!   from the strength control (max 0.8). The loop gain per round trip is
//!   `a^2 * |band-pass|^2 < 1`, so it is always stable; a soft limiter catches the
//!   side boost (up to `1/(1-a)`) it adds.
//! * **Balance**: attenuates the opposite side (no boost), -100 = left only.
//!
//! Live and click-free like the per-speaker DSP: every effect has a wet/dry mix that
//! ramps over [`MIX_SEC`] when toggled, and its parameters ramp or glide per packet.
//! With everything off the stage is bit-transparent.
//!
//! Windows already downmixes 5.1/7.1 to the 2-channel loopback, so this stage only
//! ever sees stereo; there is no upmixer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::biquad::{Coeffs, FilterKind, State, StereoBiquad};

pub const MIX_SEC: f64 = 0.02;
pub const WIDTH_RANGE: std::ops::RangeInclusive<f32> = 0.0..=200.0;
pub const BALANCE_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
pub const BASS_MONO_RANGE: std::ops::RangeInclusive<f32> = 60.0..=250.0;
pub const XTC_SPACING_RANGE: std::ops::RangeInclusive<f32> = 0.2..=4.0;
pub const XTC_DISTANCE_RANGE: std::ops::RangeInclusive<f32> = 0.3..=5.0;
/// Strength 100 % -> attenuation a = 0.8.
pub const XTC_MAX_A: f64 = 0.8;
const HEAD_RADIUS_M: f64 = 0.0875;
const SPEED_OF_SOUND: f64 = 343.0;
const XTC_LOW_HZ: f64 = 200.0;
const XTC_HIGH_HZ: f64 = 6000.0;
const HIST: usize = 128;
const HIST_MASK: usize = HIST - 1;
const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpatialSettings {
    pub width_enabled: bool,
    /// 0..200 %, 100 = unchanged.
    pub width_pct: f32,
    pub balance_enabled: bool,
    /// -100 (left only) .. +100 (right only).
    pub balance: f32,
    pub bass_mono_enabled: bool,
    /// Crossover frequency, 60..250 Hz.
    pub bass_mono_hz: f32,
    /// Experimental crosstalk cancellation ("3D").
    pub xtc_enabled: bool,
    /// Distance between the two speakers (m).
    pub xtc_spacing_m: f32,
    /// Distance from the listener to the line between the speakers (m).
    pub xtc_distance_m: f32,
    /// 0..100 %.
    pub xtc_strength_pct: f32,
}

impl Default for SpatialSettings {
    fn default() -> Self {
        Self {
            width_enabled: false,
            width_pct: 100.0,
            balance_enabled: false,
            balance: 0.0,
            bass_mono_enabled: false,
            bass_mono_hz: 120.0,
            xtc_enabled: false,
            xtc_spacing_m: 1.2,
            xtc_distance_m: 1.5,
            xtc_strength_pct: 50.0,
        }
    }
}

impl SpatialSettings {
    pub fn is_default(&self) -> bool {
        *self == SpatialSettings::default()
    }

    pub fn any_enabled(&self) -> bool {
        self.width_enabled || self.balance_enabled || self.bass_mono_enabled || self.xtc_enabled
    }

    fn width(&self) -> f64 {
        if self.width_enabled { clamp_f(self.width_pct, WIDTH_RANGE, 100.0) as f64 / 100.0 } else { 1.0 }
    }

    /// Linear gains for (left, right).
    fn balance_gains(&self) -> [f64; 2] {
        if !self.balance_enabled {
            return [1.0, 1.0];
        }
        let b = clamp_f(self.balance, BALANCE_RANGE, 0.0) as f64 / 100.0;
        [(1.0 - b).min(1.0), (1.0 + b).min(1.0)]
    }

    fn xtc_a(&self) -> f64 {
        (clamp_f(self.xtc_strength_pct, 0.0..=100.0, 50.0) as f64 / 100.0) * XTC_MAX_A
    }

    /// (full angle between the speakers seen from the listener in degrees, extra
    /// path delay to the far ear in microseconds).
    pub fn xtc_geometry(&self) -> (f64, f64) {
        let spacing = clamp_f(self.xtc_spacing_m, XTC_SPACING_RANGE, 1.2) as f64;
        let dist = clamp_f(self.xtc_distance_m, XTC_DISTANCE_RANGE, 1.5) as f64;
        let theta = (spacing / 2.0).atan2(dist);
        let delay = HEAD_RADIUS_M * (theta + theta.sin()) / SPEED_OF_SOUND;
        (2.0 * theta.to_degrees(), delay * 1e6)
    }
}

fn clamp_f(v: f32, r: std::ops::RangeInclusive<f32>, fallback: f32) -> f32 {
    if v.is_finite() { v.clamp(*r.start(), *r.end()) } else { fallback }
}

/// Shared, live-changeable spatial settings (UI writes, sender reads per packet).
#[derive(Debug, Default)]
pub struct LiveSpatial {
    settings: Mutex<SpatialSettings>,
    generation: AtomicU64,
}

impl LiveSpatial {
    pub fn new(s: SpatialSettings) -> Arc<Self> {
        Arc::new(Self { settings: Mutex::new(s), generation: AtomicU64::new(0) })
    }

    pub fn get(&self) -> SpatialSettings {
        self.settings.lock().unwrap().clone()
    }

    pub fn set(&self, s: SpatialSettings) -> bool {
        let mut cur = self.settings.lock().unwrap();
        if *cur == s {
            return false;
        }
        *cur = s;
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

fn ramp(cur: f64, goal: f64, step: f64) -> f64 {
    if cur < goal { (cur + step).min(goal) } else { (cur - step).max(goal) }
}

struct BassMono {
    mix: f64,
    /// ln(crossover) currently in use.
    lf: f64,
    lp: [StereoBiquad; 2],
    hp: [StereoBiquad; 2],
}

impl BassMono {
    fn new(hz: f64, fs: f64) -> Self {
        let mut b = Self {
            mix: 0.0,
            lf: hz.ln(),
            lp: [StereoBiquad::new(Coeffs::IDENTITY), StereoBiquad::new(Coeffs::IDENTITY)],
            hp: [StereoBiquad::new(Coeffs::IDENTITY), StereoBiquad::new(Coeffs::IDENTITY)],
        };
        b.redesign(fs);
        b
    }

    fn redesign(&mut self, fs: f64) {
        let f = self.lf.exp();
        let lp = Coeffs::design(FilterKind::LowPass, f, 0.0, BUTTERWORTH_Q, fs);
        let hp = Coeffs::design(FilterKind::HighPass, f, 0.0, BUTTERWORTH_Q, fs);
        for i in 0..2 {
            self.lp[i].coeffs = lp;
            self.hp[i].coeffs = hp;
        }
    }

    #[inline]
    fn process(&mut self, x: [f64; 2]) -> [f64; 2] {
        let [lp0, lp1] = &mut self.lp;
        let low = lp1.process(lp0.process(x));
        let [hp0, hp1] = &mut self.hp;
        let high = hp1.process(hp0.process(x));
        let m = 0.5 * (low[0] + low[1]);
        [m + high[0], m + high[1]]
    }

    fn reset(&mut self) {
        self.lp.iter_mut().chain(self.hp.iter_mut()).for_each(StereoBiquad::reset);
    }

    fn flush(&mut self) {
        self.lp.iter_mut().chain(self.hp.iter_mut()).for_each(StereoBiquad::flush_denormals);
    }
}

struct Xtc {
    mix: f64,
    a: f64,
    /// Delay in samples (fractional), >= 1.
    delay: f64,
    hist: [[f64; 2]; HIST],
    write: usize,
    band: [Coeffs; 2],
    /// Band-limit states for the two feedback paths: [path][hp, lp].
    state: [[State; 2]; 2],
}

impl Xtc {
    fn new(fs: f64) -> Self {
        Self {
            mix: 0.0,
            a: 0.0,
            delay: 1.0,
            hist: [[0.0; 2]; HIST],
            write: 0,
            band: [
                Coeffs::design(FilterKind::HighPass, XTC_LOW_HZ, 0.0, BUTTERWORTH_Q, fs),
                Coeffs::design(FilterKind::LowPass, XTC_HIGH_HZ, 0.0, BUTTERWORTH_Q, fs),
            ],
            state: [[State::default(); 2]; 2],
        }
    }

    fn reset(&mut self) {
        self.hist = [[0.0; 2]; HIST];
        self.state = [[State::default(); 2]; 2];
    }

    /// Output channel `ch`, `delay` samples ago (linear interpolation).
    #[inline]
    fn past(&self, ch: usize, delay: f64) -> f64 {
        let d = delay.clamp(1.0, (HIST - 2) as f64);
        let i = d.floor() as usize;
        let frac = d - i as f64;
        // hist[write] is the previous output (1 sample ago)
        let a = self.hist[self.write.wrapping_add(1).wrapping_sub(i) & HIST_MASK][ch];
        let b = self.hist[self.write.wrapping_sub(i) & HIST_MASK][ch];
        a + (b - a) * frac
    }

    #[inline]
    fn process(&mut self, x: [f64; 2], a: f64, delay: f64) -> [f64; 2] {
        // feedback into left comes from the right output and vice versa
        let mut fb = [self.past(1, delay), self.past(0, delay)];
        for (p, v) in fb.iter_mut().enumerate() {
            *v = self.state[p][0].process(&self.band[0], *v);
            *v = self.state[p][1].process(&self.band[1], *v);
        }
        let out = [x[0] - a * fb[0], x[1] - a * fb[1]];
        self.write = self.write.wrapping_add(1);
        self.hist[self.write & HIST_MASK] = out;
        out
    }

    fn flush(&mut self) {
        for p in self.state.iter_mut() {
            p.iter_mut().for_each(State::flush_denormals);
        }
    }
}

/// Gentle limiter used while crosstalk cancellation is active (transparent below 0.8).
#[inline]
fn soft_clip(x: f64) -> f64 {
    const KNEE: f64 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

pub struct SpatialStage {
    fs: f64,
    live: Option<Arc<LiveSpatial>>,
    seen_generation: u64,
    target: SpatialSettings,
    width: f64,
    balance: [f64; 2],
    bass: BassMono,
    xtc: Xtc,
    mix_step: f64,
    glide_k: f64,
    buf: Vec<[f64; 2]>,
}

impl SpatialStage {
    pub fn new(settings: &SpatialSettings, sample_rate: u32) -> Self {
        let fs = sample_rate as f64;
        let mut s = Self {
            fs,
            live: None,
            seen_generation: 0,
            target: settings.clone(),
            width: settings.width(),
            balance: settings.balance_gains(),
            bass: BassMono::new(clamp_f(settings.bass_mono_hz, BASS_MONO_RANGE, 120.0) as f64, fs),
            xtc: Xtc::new(fs),
            mix_step: 1.0 / (MIX_SEC * fs),
            glide_k: 0.25,
            buf: Vec::new(),
        };
        s.bass.mix = if settings.bass_mono_enabled { 1.0 } else { 0.0 };
        s.xtc.mix = if settings.xtc_enabled { 1.0 } else { 0.0 };
        s.xtc.a = settings.xtc_a();
        s.xtc.delay = s.xtc_delay_samples(settings);
        s
    }

    pub fn with_live(live: Arc<LiveSpatial>, sample_rate: u32) -> Self {
        let generation = live.generation();
        let mut s = Self::new(&live.get(), sample_rate);
        s.seen_generation = generation;
        s.live = Some(live);
        s
    }

    fn xtc_delay_samples(&self, s: &SpatialSettings) -> f64 {
        (s.xtc_geometry().1 * 1e-6 * self.fs).max(1.0)
    }

    pub fn update(&mut self, s: &SpatialSettings) {
        self.target = s.clone();
    }

    fn is_transparent(&self) -> bool {
        let t = &self.target;
        !t.any_enabled() && self.width == 1.0 && self.balance == [1.0, 1.0] && self.bass.mix == 0.0 && self.xtc.mix == 0.0
    }

    /// Process one packet in place.
    pub fn process(&mut self, frames: &mut [[f32; 2]]) {
        if let Some(live) = &self.live {
            let g = live.generation();
            if g != self.seen_generation {
                self.seen_generation = g;
                let s = live.get();
                self.update(&s);
            }
        }
        if self.is_transparent() {
            return;
        }
        let t = self.target.clone();
        let n = frames.len().max(1) as f64;
        // per-packet ramps / glides
        let (w0, w1) = (self.width, t.width());
        let (b0, b1) = (self.balance, t.balance_gains());
        let bass_goal = if t.bass_mono_enabled { 1.0 } else { 0.0 };
        let xtc_goal = if t.xtc_enabled { 1.0 } else { 0.0 };
        let bass_lf = (clamp_f(t.bass_mono_hz, BASS_MONO_RANGE, 120.0) as f64).ln();
        if (self.bass.lf - bass_lf).abs() > 1e-6 {
            self.bass.lf = if self.bass.mix == 0.0 { bass_lf } else { self.bass.lf + (bass_lf - self.bass.lf) * self.glide_k };
            if (self.bass.lf - bass_lf).abs() < 1e-4 {
                self.bass.lf = bass_lf;
            }
            self.bass.redesign(self.fs);
        }
        let (a0, a1) = (self.xtc.a, t.xtc_a());
        let (d0, d1) = (self.xtc.delay, self.xtc_delay_samples(&t));
        let bass_on = self.bass.mix > 0.0 || bass_goal > 0.0;
        let xtc_on = self.xtc.mix > 0.0 || xtc_goal > 0.0;

        self.buf.clear();
        self.buf.extend(frames.iter().map(|f| [f[0] as f64, f[1] as f64]));
        for (i, x) in self.buf.iter_mut().enumerate() {
            let u = (i + 1) as f64 / n;
            let mut v = *x;
            if bass_on {
                self.bass.mix = ramp(self.bass.mix, bass_goal, self.mix_step);
                let wet = self.bass.process(v);
                v = [v[0] + self.bass.mix * (wet[0] - v[0]), v[1] + self.bass.mix * (wet[1] - v[1])];
            }
            let w = w0 + (w1 - w0) * u;
            if w != 1.0 {
                let m = 0.5 * (v[0] + v[1]);
                let s = 0.5 * (v[0] - v[1]) * w;
                v = [m + s, m - s];
            }
            if xtc_on {
                self.xtc.mix = ramp(self.xtc.mix, xtc_goal, self.mix_step);
                let a = a0 + (a1 - a0) * u;
                let d = d0 + (d1 - d0) * u;
                let wet = self.xtc.process(v, a, d);
                let m = self.xtc.mix;
                v = [soft_clip(v[0] + m * (wet[0] - v[0])), soft_clip(v[1] + m * (wet[1] - v[1]))];
            }
            let bl = [b0[0] + (b1[0] - b0[0]) * u, b0[1] + (b1[1] - b0[1]) * u];
            *x = [v[0] * bl[0], v[1] * bl[1]];
        }
        for (f, x) in frames.iter_mut().zip(&self.buf) {
            *f = [x[0] as f32, x[1] as f32];
        }
        self.width = w1;
        self.balance = b1;
        self.xtc.a = a1;
        self.xtc.delay = d1;
        if self.bass.mix == 0.0 && bass_goal == 0.0 {
            self.bass.reset();
        } else {
            self.bass.flush();
        }
        if self.xtc.mix == 0.0 && xtc_goal == 0.0 {
            self.xtc.reset();
        } else {
            self.xtc.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const FS: u32 = 44100;

    fn tone(f: f64, start: usize, n: usize, l: f64, r: f64) -> Vec<[f32; 2]> {
        (start..start + n)
            .map(|i| {
                let s = (2.0 * PI * f * i as f64 / FS as f64).sin();
                [(l * s) as f32, (r * s) as f32]
            })
            .collect()
    }

    fn rms(b: &[[f32; 2]], ch: usize) -> f64 {
        (b.iter().map(|x| (x[ch] as f64).powi(2)).sum::<f64>() / b.len() as f64).sqrt()
    }

    fn run(stage: &mut SpatialStage, f: f64, l: f64, r: f64, packets: usize) -> Vec<[f32; 2]> {
        let mut all = Vec::new();
        for p in 0..packets {
            let mut buf = tone(f, p * 352, 352, l, r);
            stage.process(&mut buf);
            all.extend(buf);
        }
        all
    }

    #[test]
    fn defaults_are_bit_transparent() {
        let mut st = SpatialStage::new(&SpatialSettings::default(), FS);
        let mut buf = tone(440.0, 0, 352, 0.3, -0.7);
        let dry = buf.clone();
        st.process(&mut buf);
        assert_eq!(buf, dry);
        let s: SpatialSettings = toml::from_str("width_enabled = true").unwrap();
        assert_eq!(s.width_pct, 100.0, "missing fields default");
        assert!(SpatialSettings::default().is_default());
    }

    #[test]
    fn width_and_balance() {
        let s = SpatialSettings { width_enabled: true, width_pct: 0.0, ..Default::default() };
        let mut st = SpatialStage::new(&s, FS);
        let out = run(&mut st, 440.0, 0.5, -0.1, 4);
        // width 0 -> mono: both channels (0.5 - 0.1) / 2 = 0.2 * sine
        assert!(out[704..].iter().all(|f| (f[0] - f[1]).abs() < 1e-6));
        assert!((rms(&out[704..], 0) - 0.2 / 2f64.sqrt()).abs() < 2e-3);
        let s = SpatialSettings { width_enabled: true, width_pct: 200.0, ..Default::default() };
        let mut st = SpatialStage::new(&s, FS);
        let out = run(&mut st, 440.0, 0.5, 0.1, 4);
        // mid 0.3, side 0.2*2 -> L 0.7, R -0.1
        assert!((rms(&out[704..], 0) - 0.7 / 2f64.sqrt()).abs() < 3e-3);
        assert!((rms(&out[704..], 1) - 0.1 / 2f64.sqrt()).abs() < 3e-3);
        let s = SpatialSettings { balance_enabled: true, balance: -50.0, ..Default::default() };
        let mut st = SpatialStage::new(&s, FS);
        let out = run(&mut st, 440.0, 0.5, 0.5, 4);
        assert!((rms(&out[704..], 0) - 0.5 / 2f64.sqrt()).abs() < 2e-3, "left untouched");
        assert!((rms(&out[704..], 1) - 0.25 / 2f64.sqrt()).abs() < 2e-3, "right -6 dB");
    }

    #[test]
    fn bass_mono_monos_the_lows_and_keeps_the_highs() {
        let s = SpatialSettings { bass_mono_enabled: true, bass_mono_hz: 120.0, ..Default::default() };
        // 40 Hz on the left only -> half on each side (well below the crossover)
        let mut st = SpatialStage::new(&s, FS);
        let out = run(&mut st, 40.0, 0.5, 0.0, 400);
        let tail = &out[out.len() - 44100..];
        assert!((rms(tail, 0) / rms(tail, 1) - 1.0).abs() < 0.03, "lows mono: {} {}", rms(tail, 0), rms(tail, 1));
        assert!((rms(tail, 0) - 0.25 / 2f64.sqrt()).abs() < 0.01);
        // 2 kHz on the left only stays on the left (flat magnitude through LR4)
        let mut st = SpatialStage::new(&s, FS);
        let out = run(&mut st, 2000.0, 0.5, 0.0, 200);
        let tail = &out[out.len() - 22050..];
        assert!((rms(tail, 0) - 0.5 / 2f64.sqrt()).abs() < 0.005);
        assert!(rms(tail, 1) < 0.003);
    }

    #[test]
    fn crosstalk_canceller_is_stable_and_cancels() {
        let s = SpatialSettings { xtc_enabled: true, xtc_strength_pct: 100.0, ..Default::default() };
        let (angle, delay_us) = s.xtc_geometry();
        assert!((angle - 43.6).abs() < 0.5, "{angle}");
        assert!((150.0..250.0).contains(&delay_us), "{delay_us}");
        // white-ish noise at full scale for 5 s: bounded and finite (soft limiter)
        let mut st = SpatialStage::new(&s, FS);
        let mut seed = 1u32;
        for _ in 0..(5 * FS as usize / 352) {
            let mut buf: Vec<[f32; 2]> = (0..352)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let a = (seed >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let b = (seed >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
                    [a, b]
                })
                .collect();
            st.process(&mut buf);
            assert!(buf.iter().all(|f| f[0].is_finite() && f[0].abs() <= 1.0 && f[1].abs() <= 1.0));
        }
        // the impulse response decays (stable recursion)
        let mut st = SpatialStage::new(&s, FS);
        let mut buf = vec![[0.0f32; 2]; 352];
        buf[0] = [0.5, 0.0];
        st.process(&mut buf);
        for _ in 0..200 {
            buf.fill([0.0; 2]);
            st.process(&mut buf);
        }
        assert!(buf.iter().all(|f| f[0].abs() < 1e-6 && f[1].abs() < 1e-6));
        // it emits an inverted, delayed copy on the other side (the cancellation signal)
        let mut st = SpatialStage::new(&s, FS);
        let mut buf = vec![[0.0f32; 2]; 352];
        buf[0] = [0.5, 0.0];
        st.process(&mut buf);
        let min_r = buf.iter().map(|f| f[1]).fold(0.0f32, f32::min);
        assert!(min_r < -0.05, "cancellation pulse on the right: {min_r}");
        let at = buf.iter().position(|f| f[1] == min_r).unwrap();
        assert!((5..30).contains(&at), "around the geometric delay: {at}");
    }

    #[test]
    fn toggling_and_dragging_everything_is_click_free() {
        let live = LiveSpatial::new(SpatialSettings::default());
        let mut st = SpatialStage::with_live(live.clone(), FS);
        let mut s = SpatialSettings::default();
        let mut prev: Option<f32> = None;
        let mut worst = 0.0f32;
        for p in 0..600usize {
            match p % 25 {
                0 => s.width_enabled = !s.width_enabled,
                3 => s.width_pct = (p * 7 % 200) as f32,
                5 => s.balance_enabled = !s.balance_enabled,
                7 => s.balance = (p as f32 * 13.0) % 200.0 - 100.0,
                10 => s.bass_mono_enabled = !s.bass_mono_enabled,
                12 => s.bass_mono_hz = 60.0 + (p * 11 % 190) as f32,
                15 => s.xtc_enabled = !s.xtc_enabled,
                18 => s.xtc_strength_pct = (p * 17 % 100) as f32,
                20 => s.xtc_distance_m = 0.5 + (p % 4) as f32,
                _ => {}
            }
            live.set(s.clone());
            let mut buf = tone(150.0, p * 352, 352, 0.4, 0.1);
            st.process(&mut buf);
            for f in &buf {
                if let Some(q) = prev {
                    worst = worst.max((f[0] - q).abs());
                }
                prev = Some(f[0]);
            }
        }
        // 150 Hz at <= 0.4 (x side boost) moves ~0.01-0.03 per sample; clicks are bigger
        assert!(worst < 0.06, "discontinuity {worst}");
    }
}
