//! Minimal complex FFT (iterative radix-2, f64) for the calibration analysis. Sizes
//! are powers of two; the forward transform is unnormalised, the inverse divides by n.

use std::f64::consts::PI;
use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct C {
    pub re: f64,
    pub im: f64,
}

impl C {
    pub const ZERO: C = C { re: 0.0, im: 0.0 };

    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    pub fn conj(self) -> Self {
        Self { re: self.re, im: -self.im }
    }

    pub fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    pub fn abs(self) -> f64 {
        self.norm_sqr().sqrt()
    }

    pub fn scale(self, k: f64) -> Self {
        Self { re: self.re * k, im: self.im * k }
    }

    /// e^(i*phi)
    pub fn cis(phi: f64) -> Self {
        let (s, c) = phi.sin_cos();
        Self { re: c, im: s }
    }
}

impl Add for C {
    type Output = C;
    fn add(self, o: C) -> C {
        C { re: self.re + o.re, im: self.im + o.im }
    }
}

impl Sub for C {
    type Output = C;
    fn sub(self, o: C) -> C {
        C { re: self.re - o.re, im: self.im - o.im }
    }
}

impl Mul for C {
    type Output = C;
    fn mul(self, o: C) -> C {
        C { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }
}

pub fn next_pow2(n: usize) -> usize {
    n.max(2).next_power_of_two()
}

/// In-place FFT; `buf.len()` must be a power of two.
pub fn fft(buf: &mut [C], inverse: bool) {
    let n = buf.len();
    assert!(n.is_power_of_two(), "fft size {n} is not a power of two");
    if n < 2 {
        return;
    }
    // bit reversal
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            buf.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let twiddle: Vec<C> = (0..n / 2).map(|k| C::cis(sign * 2.0 * PI * k as f64 / n as f64)).collect();
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let w = twiddle[k * step];
                let a = buf[start + k];
                let b = buf[start + k + half] * w;
                buf[start + k] = a + b;
                buf[start + k + half] = a - b;
            }
        }
        len <<= 1;
    }
    if inverse {
        let k = 1.0 / n as f64;
        for x in buf.iter_mut() {
            *x = x.scale(k);
        }
    }
}

/// FFT of real samples zero-padded to `n`.
pub fn rfft(x: &[f64], n: usize) -> Vec<C> {
    let mut buf = vec![C::ZERO; n];
    for (b, &v) in buf.iter_mut().zip(x) {
        b.re = v;
    }
    fft(&mut buf, false);
    buf
}

/// Real part of the inverse FFT.
pub fn irfft(mut spec: Vec<C>) -> Vec<f64> {
    fft(&mut spec, true);
    spec.into_iter().map(|c| c.re).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_naive_dft_and_round_trips() {
        let n = 64;
        let x: Vec<C> = (0..n).map(|i| C::new((i as f64 * 0.37).sin() + 0.1 * i as f64, (i as f64 * 1.3).cos())).collect();
        let mut f = x.clone();
        fft(&mut f, false);
        for (k, fk) in f.iter().enumerate() {
            let mut acc = C::ZERO;
            for (j, xj) in x.iter().enumerate() {
                acc = acc + *xj * C::cis(-2.0 * PI * (j * k) as f64 / n as f64);
            }
            assert!((acc - *fk).abs() < 1e-9, "bin {k}");
        }
        fft(&mut f, true);
        for (a, b) in f.iter().zip(&x) {
            assert!((*a - *b).abs() < 1e-12);
        }
        let r = irfft(rfft(&[1.0, 2.0, 3.0], 8));
        assert!((r[2] - 3.0).abs() < 1e-12 && r[5].abs() < 1e-12);
        assert_eq!(next_pow2(1000), 1024);
    }
}
