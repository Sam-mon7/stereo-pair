//! RBJ "Audio EQ Cookbook" biquads (Robert Bristow-Johnson), f64 coefficients and
//! state, transposed direct form II.
//!
//! Used by the per-speaker EQ ([`super::eq`]), the spatial stage's crossovers and
//! band limits ([`super::spatial`]) and the calibration EQ fitter (which evaluates
//! [`Coeffs::magnitude_db`] to predict the corrected response).

use std::f64::consts::PI;

use serde::{Deserialize, Serialize};

/// Filter shapes. Gain applies to peaking and shelves only; Q is the RBJ Q (for
/// shelves 0.707 gives the classic S = 1 slope; for HP/LP 0.707 is Butterworth).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    #[default]
    Peaking,
    LowShelf,
    HighShelf,
    HighPass,
    LowPass,
}

impl FilterKind {
    pub const ALL: [FilterKind; 5] =
        [FilterKind::Peaking, FilterKind::LowShelf, FilterKind::HighShelf, FilterKind::HighPass, FilterKind::LowPass];

    pub fn label(self) -> &'static str {
        match self {
            FilterKind::Peaking => "Peak",
            FilterKind::LowShelf => "Low shelf",
            FilterKind::HighShelf => "High shelf",
            FilterKind::HighPass => "High-pass",
            FilterKind::LowPass => "Low-pass",
        }
    }

    pub fn has_gain(self) -> bool {
        matches!(self, FilterKind::Peaking | FilterKind::LowShelf | FilterKind::HighShelf)
    }
}

pub const MIN_FREQ: f64 = 10.0;
pub const MIN_Q: f64 = 0.1;
pub const MAX_Q: f64 = 20.0;
pub const MAX_GAIN_DB: f64 = 24.0;

/// Normalised coefficients (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    pub const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// Design a filter. Out-of-range inputs are clamped (frequency to
    /// 10 Hz..0.45 fs, Q to 0.1..20, gain to +-24 dB), so the result is always stable.
    pub fn design(kind: FilterKind, freq: f64, gain_db: f64, q: f64, sample_rate: f64) -> Coeffs {
        let f = if freq.is_finite() { freq.clamp(MIN_FREQ, 0.45 * sample_rate) } else { 1000.0 };
        let q = if q.is_finite() { q.clamp(MIN_Q, MAX_Q) } else { 0.707 };
        let g = if gain_db.is_finite() { gain_db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB) } else { 0.0 };
        let w0 = 2.0 * PI * f / sample_rate;
        let (sn, cs) = w0.sin_cos();
        let alpha = sn / (2.0 * q);
        let a = 10f64.powf(g / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            FilterKind::LowPass => ((1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            FilterKind::HighPass => ((1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0, 1.0 + alpha, -2.0 * cs, 1.0 - alpha),
            FilterKind::Peaking => (1.0 + alpha * a, -2.0 * cs, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cs, 1.0 - alpha / a),
            FilterKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cs + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                    a * ((a + 1.0) - (a - 1.0) * cs - s),
                    (a + 1.0) + (a - 1.0) * cs + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                    (a + 1.0) + (a - 1.0) * cs - s,
                )
            }
            FilterKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cs + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                    a * ((a + 1.0) + (a - 1.0) * cs - s),
                    (a + 1.0) - (a - 1.0) * cs + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cs),
                    (a + 1.0) - (a - 1.0) * cs - s,
                )
            }
        };
        Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// |H(f)|^2 at `freq` Hz.
    pub fn power(&self, freq: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * PI * freq / sample_rate;
        let (c1, c2) = (w.cos(), (2.0 * w).cos());
        let Coeffs { b0, b1, b2, a1, a2 } = *self;
        let num = b0 * b0 + b1 * b1 + b2 * b2 + 2.0 * (b0 * b1 + b1 * b2) * c1 + 2.0 * b0 * b2 * c2;
        let den = 1.0 + a1 * a1 + a2 * a2 + 2.0 * (a1 + a1 * a2) * c1 + 2.0 * a2 * c2;
        num / den.max(1e-300)
    }

    pub fn magnitude_db(&self, freq: f64, sample_rate: f64) -> f64 {
        10.0 * self.power(freq, sample_rate).max(1e-30).log10()
    }
}

/// One channel of biquad state (TDF-II).
#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    s1: f64,
    s2: f64,
}

impl State {
    #[inline]
    pub fn process(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        y
    }

    /// Flush denormal-range state to zero (call once per block; after long silence
    /// the state decays towards denormals, which are very slow on x86).
    #[inline]
    pub fn flush_denormals(&mut self) {
        if self.s1.abs() < 1e-25 {
            self.s1 = 0.0;
        }
        if self.s2.abs() < 1e-25 {
            self.s2 = 0.0;
        }
    }

    pub fn reset(&mut self) {
        *self = State::default();
    }
}

/// A fixed biquad with stereo state (used for crossovers and band limits).
#[derive(Clone, Debug)]
pub struct StereoBiquad {
    pub coeffs: Coeffs,
    state: [State; 2],
}

impl StereoBiquad {
    pub fn new(coeffs: Coeffs) -> Self {
        Self { coeffs, state: [State::default(); 2] }
    }

    #[inline]
    pub fn process(&mut self, x: [f64; 2]) -> [f64; 2] {
        [self.state[0].process(&self.coeffs, x[0]), self.state[1].process(&self.coeffs, x[1])]
    }

    pub fn flush_denormals(&mut self) {
        self.state.iter_mut().for_each(State::flush_denormals);
    }

    pub fn reset(&mut self) {
        self.state.iter_mut().for_each(State::reset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 44100.0;

    /// Measure the steady-state gain of a filter at `f` by running a sine through it.
    fn measured_db(c: &Coeffs, f: f64) -> f64 {
        let mut st = State::default();
        let n = (FS * 0.5) as usize;
        let (mut e_in, mut e_out) = (0.0, 0.0);
        for i in 0..n {
            let x = (2.0 * PI * f * i as f64 / FS).sin();
            let y = st.process(c, x);
            if i > n / 2 {
                e_in += x * x;
                e_out += y * y;
            }
        }
        10.0 * (e_out / e_in).log10()
    }

    #[test]
    fn cookbook_shapes_have_the_expected_gains() {
        let pk = Coeffs::design(FilterKind::Peaking, 1000.0, 6.0, 1.4, FS);
        assert!((pk.magnitude_db(1000.0, FS) - 6.0).abs() < 1e-6);
        assert!(pk.magnitude_db(50.0, FS).abs() < 0.05);
        assert!(pk.magnitude_db(15000.0, FS).abs() < 0.1);
        assert!((measured_db(&pk, 1000.0) - 6.0).abs() < 0.05);

        let cut = Coeffs::design(FilterKind::Peaking, 300.0, -9.0, 4.0, FS);
        assert!((cut.magnitude_db(300.0, FS) + 9.0).abs() < 1e-6);

        let ls = Coeffs::design(FilterKind::LowShelf, 200.0, 4.0, 0.707, FS);
        assert!((ls.magnitude_db(20.0, FS) - 4.0).abs() < 0.1);
        assert!((ls.magnitude_db(200.0, FS) - 2.0).abs() < 0.05, "half gain at f0");
        assert!(ls.magnitude_db(5000.0, FS).abs() < 0.05);

        let hs = Coeffs::design(FilterKind::HighShelf, 4000.0, -5.0, 0.707, FS);
        assert!((hs.magnitude_db(18000.0, FS) + 5.0).abs() < 0.15);
        assert!(hs.magnitude_db(100.0, FS).abs() < 0.05);

        let hp = Coeffs::design(FilterKind::HighPass, 100.0, 0.0, std::f64::consts::FRAC_1_SQRT_2, FS);
        assert!((hp.magnitude_db(100.0, FS) + 3.01).abs() < 0.02, "Butterworth -3 dB at fc");
        assert!((hp.magnitude_db(50.0, FS) + 12.3).abs() < 0.3, "12 dB/oct");
        assert!(hp.magnitude_db(2000.0, FS).abs() < 0.02);
        assert!((measured_db(&hp, 100.0) + 3.01).abs() < 0.1);

        let lp = Coeffs::design(FilterKind::LowPass, 5000.0, 0.0, std::f64::consts::FRAC_1_SQRT_2, FS);
        assert!((lp.magnitude_db(5000.0, FS) + 3.01).abs() < 0.02);
        assert!(lp.magnitude_db(100.0, FS).abs() < 0.01);
    }

    #[test]
    fn out_of_range_designs_are_clamped_and_stable() {
        for kind in FilterKind::ALL {
            for (f, g, q) in [(0.0, 0.0, 0.0), (1e9, 100.0, 1e9), (f64::NAN, f64::NAN, f64::NAN), (5.0, -80.0, 0.01)] {
                let c = Coeffs::design(kind, f, g, q, FS);
                // poles inside the unit circle: |a2| < 1 and |a1| < 1 + a2
                assert!(c.a2.abs() < 1.0 && c.a1.abs() < 1.0 + c.a2, "{kind:?} {f} {g} {q}: {c:?}");
                let mut st = State::default();
                let mut y = 0.0;
                for i in 0..20000 {
                    y = st.process(&c, if i == 0 { 1.0 } else { 0.0 });
                }
                assert!(y.abs() < 1e-3, "{kind:?} impulse response decays");
            }
        }
    }

    #[test]
    fn denormal_flush_zeroes_tiny_state() {
        let c = Coeffs::design(FilterKind::LowPass, 50.0, 0.0, 0.707, FS);
        let mut st = State::default();
        st.process(&c, 1e-30);
        st.flush_denormals();
        assert_eq!((st.s1, st.s2), (0.0, 0.0));
    }
}
