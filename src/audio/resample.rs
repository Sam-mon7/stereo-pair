//! Variable-ratio resampler (4-point Catmull-Rom cubic). Converts the capture rate
//! (44.1 or 48 kHz on the capture device's own crystal) to 44.1 kHz on our NTP clock,
//! with a small ratio trim driven by the buffer fill level to absorb clock drift.
//!
//! At ratio 1.0 with zero trim the phase stays at 0 and the output is bit-identical
//! to the input.

use std::collections::VecDeque;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Interpolation window x[-1], x[0], x[1], x[2]; output lies between x[0] and x[1].
    win: [[f32; 2]; 4],
    frac: f64,
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { in_rate, out_rate, win: [[0.0; 2]; 4], frac: 0.0, primed: false }
    }

    pub fn reset(&mut self) {
        self.win = [[0.0; 2]; 4];
        self.frac = 0.0;
        self.primed = false;
    }

    /// Input frames needed (roughly) to produce `n` output frames.
    pub fn input_needed(&self, n: usize) -> usize {
        (n as f64 * self.in_rate as f64 / self.out_rate as f64).ceil() as usize + 4
    }

    /// Fill `out` from `input`. `trim` adjusts the step ratio (e.g. +1e-4 consumes
    /// 100 ppm faster). Returns the number of frames written; fewer than `out.len()`
    /// means the input ran dry (remaining output untouched).
    pub fn process(&mut self, input: &mut VecDeque<[f32; 2]>, out: &mut [[f32; 2]], trim: f64) -> usize {
        if !self.primed {
            // need x[0], x[1], x[2]; x[-1] starts as a copy of x[0]
            if input.len() < 3 {
                return 0;
            }
            let a = input.pop_front().unwrap();
            let b = input.pop_front().unwrap();
            let c = input.pop_front().unwrap();
            self.win = [a, a, b, c];
            self.frac = 0.0;
            self.primed = true;
        }
        let step = self.in_rate as f64 / self.out_rate as f64 * (1.0 + trim);
        for (i, o) in out.iter_mut().enumerate() {
            let t = self.frac as f32;
            let [xm1, x0, x1, x2] = self.win;
            for ch in 0..2 {
                *o.get_mut(ch).unwrap() = if t == 0.0 {
                    x0[ch]
                } else {
                    catmull_rom(xm1[ch], x0[ch], x1[ch], x2[ch], t)
                };
            }
            self.frac += step;
            while self.frac >= 1.0 {
                let Some(next) = input.pop_front() else {
                    // keep phase consistent: we consumed up to here
                    self.frac -= 1.0;
                    self.win = [self.win[1], self.win[2], self.win[3], self.win[3]];
                    return i + 1;
                };
                self.frac -= 1.0;
                self.win = [self.win[1], self.win[2], self.win[3], next];
            }
        }
        out.len()
    }
}

fn catmull_rom(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let a = -0.5 * xm1 + 1.5 * x0 - 1.5 * x1 + 0.5 * x2;
    let b = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c = -0.5 * xm1 + 0.5 * x1;
    ((a * t + b) * t + c) * t + x0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_ratio_is_transparent() {
        let mut r = Resampler::new(44100, 44100);
        let src: Vec<[f32; 2]> = (0..1000).map(|i| [(i as f32 * 0.01).sin(), i as f32 / 1000.0]).collect();
        let mut q: VecDeque<_> = src.iter().copied().collect();
        let mut out = vec![[0.0; 2]; 352];
        assert_eq!(r.process(&mut q, &mut out, 0.0), 352);
        assert_eq!(&out[..], &src[..352]);
    }

    #[test]
    fn rate_conversion_preserves_frequency_and_count() {
        // 1 kHz sine at 48 kHz -> 44.1 kHz
        let mut r = Resampler::new(48000, 44100);
        let mut q: VecDeque<[f32; 2]> = (0..48000)
            .map(|i| {
                let s = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin() as f32;
                [s, s]
            })
            .collect();
        let mut out = vec![[0.0; 2]; 44100 - 10];
        assert_eq!(r.process(&mut q, &mut out, 0.0), out.len());
        // consumed ~ 48000/44100 per output frame
        let consumed = 48000 - q.len();
        assert!((consumed as i64 - (out.len() as f64 * 48000.0 / 44100.0) as i64).abs() < 5);
        // compare against ideal 1 kHz at 44.1 kHz
        let mut err: f32 = 0.0;
        for (i, o) in out.iter().enumerate().skip(10) {
            let ideal = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44100.0).sin() as f32;
            err = err.max((o[0] - ideal).abs());
        }
        assert!(err < 0.01, "max error {err}");
    }

    #[test]
    fn underrun_reports_partial() {
        let mut r = Resampler::new(44100, 44100);
        let mut q: VecDeque<[f32; 2]> = (0..100).map(|i| [i as f32, 0.0]).collect();
        let mut out = vec![[0.0; 2]; 352];
        let n = r.process(&mut q, &mut out, 0.0);
        assert!((97..352).contains(&n), "{n}");
    }
}
