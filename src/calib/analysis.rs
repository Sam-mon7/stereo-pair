//! Turn a mic recording of a sweep [`Schedule`] into per-speaker impulse responses,
//! relative arrival times, levels and frequency responses, with quality figures.
//!
//! Clock-free timing: the mic runs on its own crystal and starts recording at an
//! arbitrary moment. Only differences between IR positions are used, and the known
//! timeline offsets between sweeps of the same speaker give the mic-samples-per-
//! timeline-frame ratio `r` (drift included). The relative arrival of speaker j vs
//! speaker 0 in repeat m is
//!
//! ```text
//! delta = (pos_j,m - pos_0,m) - (offset_j,m - offset_0,m) * r      [mic samples]
//! ```
//!
//! averaged over repeats. Positions come from the IR train (coarse peak), refined to
//! sub-sample precision with GCC-PHAT (band 200 Hz..4 kHz, 32x interpolated) between
//! direct-sound windows of the IRs.

use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;

use super::fft::{C, irfft, next_pow2, rfft};
use super::{LEVEL_BAND, Schedule, SweepSpec, TIMELINE_RATE, band_level_db, grid, smooth_db};

/// Analysis runs at <= 48 kHz; faster mic rates are decimated first.
const MAX_ANALYSIS_RATE: f64 = 50000.0;
/// IR segment kept per sweep: this much before the arrival ...
pub const IR_PRE_SEC: f64 = 0.1;
/// ... and this much after it.
pub const IR_POST_SEC: f64 = 0.5;
/// Direct-sound window used for timing: arrival -5 ms .. +10 ms.
const DIRECT_PRE_SEC: f64 = 0.005;
const DIRECT_POST_SEC: f64 = 0.010;
const GCC_UPSAMPLE: usize = 32;
/// Deconvolution band edges: flat from f1 * BAND_EDGE to f2 / BAND_EDGE.
const BAND_EDGE: f64 = 1.12;
/// How far a speaker's IR may sit from where the schedule predicts it.
const SEARCH_SEC: f64 = 0.25;

/// What the mic captured: one or two channels at `rate`.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub rate: f64,
    pub channels: Vec<Vec<f32>>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Issue {
    /// Fatal issues block applying the result.
    pub fatal: bool,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpeakerResult {
    pub name: String,
    /// Arrival at the mic relative to speaker 0 (ms; positive = later).
    pub arrival_ms: f64,
    /// Max - min of the per-repeat arrivals (ms).
    pub arrival_spread_ms: f64,
    pub repeat_arrivals_ms: Vec<f64>,
    /// Path gain digital-out -> mic in the level band (dB).
    pub level_db: f64,
    pub level_spread_db: f64,
    pub repeat_levels_db: Vec<f64>,
    /// Worst IR peak-to-noise ratio over the repeats (dB).
    pub pnr_db: f64,
    /// Sweep RMS vs. the silence before the first sweep, raw recording (dB).
    pub snr_db: Option<f64>,
    /// RMS level of this speaker's sweeps at the mic (dBFS).
    pub sweep_dbfs: f64,
    /// 1/6-octave smoothed response on [`Analysis::grid_hz`] (dB path gain; NaN
    /// outside the measured band, 25 Hz..17.4 kHz for the default sweep).
    pub response_db: Vec<f64>,
    /// Median over 100 Hz..10 kHz of the per-frequency spread between repeats (dB).
    pub response_spread_db: f64,
    /// -6 dB point of the 1/3-octave response below the level band.
    pub lf_rolloff_hz: f64,
    /// Impulse responses per repeat at `Analysis::rate`, starting IR_PRE_SEC before
    /// the arrival.
    #[serde(skip)]
    pub irs: Vec<Vec<f32>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Analysis {
    /// Analysis rate (the mic rate, or after decimation).
    pub rate: f64,
    pub mic_rate: f64,
    pub mic_channel: usize,
    pub channel_rms_dbfs: Vec<f64>,
    pub peak_dbfs: f64,
    pub noise_dbfs: Option<f64>,
    /// Mic clock vs. the timeline (+ = mic runs fast).
    pub clock_ppm: f64,
    /// Worst deviation of a sweep from the fitted clock (ms); large = dropouts.
    pub drift_residual_ms: f64,
    pub repeats: usize,
    pub grid_hz: Vec<f64>,
    pub speakers: Vec<SpeakerResult>,
    pub issues: Vec<Issue>,
}

impl Analysis {
    pub fn ok(&self) -> bool {
        !self.issues.iter().any(|i| i.fatal)
    }
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

fn rms(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
}

/// Raised-cosine ramp 0..1 for `x` between `a` and `b` (log or linear axis).
fn ramp(x: f64, a: f64, b: f64) -> f64 {
    if x <= a {
        0.0
    } else if x >= b {
        1.0
    } else {
        0.5 - 0.5 * (std::f64::consts::PI * (x - a) / (b - a)).cos()
    }
}

/// Band-pass weight in log frequency: 0 below `lo`, 1 from `lo*k` to `hi/k`, 0 above `hi`.
fn band_weight(f: f64, lo: f64, hi: f64, k: f64) -> f64 {
    if f <= 0.0 {
        return 0.0;
    }
    let lf = f.ln();
    ramp(lf, lo.ln(), (lo * k).ln()) * (1.0 - ramp(lf, (hi / k).ln(), hi.ln()))
}

fn bin_freq(k: usize, n: usize, fs: f64) -> f64 {
    let k = if k <= n / 2 { k } else { n - k };
    k as f64 * fs / n as f64
}

/// Decimate by 2 or 4 (windowed-sinc low-pass) if the mic rate is above 50 kHz.
fn decimate(x: &[f32], rate: f64) -> (Vec<f64>, f64) {
    let factor = if rate > 2.0 * MAX_ANALYSIS_RATE { 4 } else if rate > MAX_ANALYSIS_RATE { 2 } else { 1 };
    if factor == 1 {
        return (x.iter().map(|&v| v as f64).collect(), rate);
    }
    let out_rate = rate / factor as f64;
    let cutoff = 0.45 * out_rate / rate; // cycles per input sample
    let half = 32 * factor as isize;
    let taps: Vec<f64> = (-half..=half)
        .map(|i| {
            let t = i as f64;
            let sinc = if i == 0 { 2.0 * cutoff } else { (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t) };
            let w = 0.42 + 0.5 * (std::f64::consts::PI * t / half as f64).cos() + 0.08 * (2.0 * std::f64::consts::PI * t / half as f64).cos();
            sinc * w
        })
        .collect();
    let n_out = x.len() / factor;
    let out = (0..n_out)
        .map(|j| {
            let c = (j * factor) as isize;
            taps.iter()
                .enumerate()
                .map(|(i, t)| {
                    let idx = c + i as isize - half;
                    if idx >= 0 && (idx as usize) < x.len() { t * x[idx as usize] as f64 } else { 0.0 }
                })
                .sum()
        })
        .collect();
    (out, out_rate)
}

/// Deconvolve the recording spectrum `xs` (size n) by the sweep as it appears at the
/// mic when the mic takes `ratio` samples per timeline frame. Returns the IR train.
fn deconvolve(xs: &[C], sweep: &SweepSpec, fs: f64, ratio: f64) -> Vec<f64> {
    let n = xs.len();
    let mic_per_sec = ratio * TIMELINE_RATE;
    let len = (sweep.seconds * mic_per_sec).ceil() as usize;
    let reference: Vec<f64> = (0..len).map(|i| sweep.value(i as f64 / mic_per_sec)).collect();
    let s = rfft(&reference, n);
    let max_p = s.iter().map(|c| c.norm_sqr()).fold(0.0, f64::max);
    let eps = 1e-7 * max_p;
    let hi = sweep.f2.min(0.47 * fs);
    let d: Vec<C> = xs
        .iter()
        .zip(&s)
        .enumerate()
        .map(|(k, (x, sk))| {
            let w = band_weight(bin_freq(k, n, fs), sweep.f1, hi, BAND_EDGE);
            if w == 0.0 { C::ZERO } else { (*x * sk.conj()).scale(w / (sk.norm_sqr() + eps)) }
        })
        .collect();
    irfft(d)
}

/// Sub-sample peak of |h| near `center` (parabolic interpolation).
fn refine_peak(h: &[f64], center: f64, radius: usize) -> f64 {
    let c = center.round() as isize;
    let lo = (c - radius as isize).max(1) as usize;
    let hi = ((c + radius as isize) as usize).min(h.len() - 2);
    let mut best = lo;
    for i in lo..=hi {
        if h[i].abs() > h[best].abs() {
            best = i;
        }
    }
    let (a, b, cc) = (h[best - 1].abs(), h[best].abs(), h[best + 1].abs());
    let den = a - 2.0 * b + cc;
    let off = if den.abs() > 1e-30 { (0.5 * (a - cc) / den).clamp(-0.5, 0.5) } else { 0.0 };
    best as f64 + off
}

/// Find every sweep's IR in the train: try each hypothesis "the strongest peak is
/// sweep k" and keep the one whose predicted positions hold the most energy.
fn locate(h: &[f64], sched: &Schedule, ratio: f64, fs: f64) -> Result<Vec<f64>> {
    let len = h.len();
    let w = (SEARCH_SEC * fs).min(0.4 * sched.period_frames as f64 * ratio) as isize;
    let g = (0..len).max_by(|&a, &b| h[a].abs().total_cmp(&h[b].abs())).ok_or_else(|| anyhow!("empty recording"))?;
    let mut best: Option<(f64, Vec<usize>)> = None;
    for anchor in &sched.slots {
        let a = g as f64 - anchor.offset as f64 * ratio;
        let mut score = 0.0;
        let mut pos = Vec::new();
        for s in &sched.slots {
            let p = (a + s.offset as f64 * ratio).round() as isize;
            if p - w < 0 || p + w >= len as isize {
                pos.clear();
                break;
            }
            let i = ((p - w) as usize..(p + w) as usize).max_by(|&x, &y| h[x].abs().total_cmp(&h[y].abs())).unwrap();
            score += h[i].abs();
            pos.push(i);
        }
        if pos.len() == sched.slots.len() && best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, pos));
        }
    }
    let (_, pos) = best.ok_or_else(|| anyhow!("the recording does not cover all sweeps (started too late or stopped too early)"))?;
    Ok(pos.into_iter().map(|p| refine_peak(h, p as f64, 2)).collect())
}

/// Least-squares mic-samples-per-frame ratio from pairs of sweeps of the same speaker
/// (which cancels each speaker's own delay). Returns (ratio, worst residual in samples).
fn fit_ratio(pos: &[f64], sched: &Schedule, fallback: f64) -> (f64, f64) {
    let (mut num, mut den) = (0.0, 0.0);
    let mut pairs = Vec::new();
    for spk in 0..sched.speakers {
        let Some(k0) = sched.slot_index(spk, 0) else { continue };
        for rep in 1..sched.repeats {
            let Some(k) = sched.slot_index(spk, rep) else { continue };
            let dp = pos[k] - pos[k0];
            let doff = (sched.slots[k].offset - sched.slots[k0].offset) as f64;
            num += dp * doff;
            den += doff * doff;
            pairs.push((dp, doff));
        }
    }
    if den == 0.0 {
        return (fallback, 0.0);
    }
    let r = num / den;
    let resid = pairs.iter().map(|(dp, doff)| (dp - doff * r).abs()).fold(0.0, f64::max);
    (r, resid)
}

/// GCC-PHAT lag of `b` relative to `a` (samples; positive = `b` later), restricted to
/// `band`, 32x interpolated. Both are windowed already.
fn gcc_phat(a: &[f64], b: &[f64], fs: f64, band: (f64, f64)) -> f64 {
    let n = next_pow2(2 * a.len().max(b.len()));
    let fa = rfft(a, n);
    let fb = rfft(b, n);
    let big_n = n * GCC_UPSAMPLE;
    let mut g = vec![C::ZERO; big_n];
    for k in 0..n {
        if k == n / 2 {
            continue;
        }
        let f = bin_freq(k, n, fs);
        let w = band_weight(f, band.0 / 1.5, band.1 * 1.5, 1.5);
        if w == 0.0 {
            continue;
        }
        let cross = fa[k].conj() * fb[k];
        let mag = cross.abs();
        if mag < 1e-30 {
            continue;
        }
        let v = cross.scale(w / mag);
        let dst = if k < n / 2 { k } else { big_n - (n - k) };
        g[dst] = v;
    }
    let corr = irfft(g);
    let mut best = 0;
    for i in 0..big_n {
        if corr[i].abs() > corr[best].abs() {
            best = i;
        }
    }
    let at = |i: isize| corr[i.rem_euclid(big_n as isize) as usize].abs();
    let (l, c, r) = (at(best as isize - 1), at(best as isize), at(best as isize + 1));
    let den = l - 2.0 * c + r;
    let off = if den.abs() > 1e-30 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
    let mut lag = best as f64 + off;
    if lag > big_n as f64 / 2.0 {
        lag -= big_n as f64;
    }
    lag / GCC_UPSAMPLE as f64
}

/// Tukey-windowed direct-sound part of an IR segment (arrival at `pre`).
fn direct_window(seg: &[f64], pre: usize, fs: f64) -> Vec<f64> {
    let a = pre.saturating_sub((DIRECT_PRE_SEC * fs) as usize);
    let b = (pre + (DIRECT_POST_SEC * fs) as usize).min(seg.len());
    let taper = (0.001 * fs).max(2.0);
    let len = (b - a) as f64;
    seg[a..b]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let t = i as f64;
            v * ramp(t, 0.0, taper) * ramp(len - 1.0 - t, 0.0, taper)
        })
        .collect()
}

/// Power spectrum of an IR segment from 2 ms before the arrival to the end (fade-out
/// over the last 30 %); returns (power per bin, bin width in Hz).
fn ir_power(seg: &[f64], pre: usize, fs: f64) -> (Vec<f64>, f64) {
    let a = pre.saturating_sub((0.002 * fs) as usize);
    let part = &seg[a..];
    let len = part.len() as f64;
    let fade_from = 0.7 * len;
    let w: Vec<f64> = part
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let t = i as f64;
            let fade = if t < fade_from { 1.0 } else { 0.5 + 0.5 * (std::f64::consts::PI * (t - fade_from) / (len - fade_from)).cos() };
            v * fade * ramp(t, 0.0, 0.001 * fs)
        })
        .collect();
    let n = next_pow2(w.len());
    let spec = rfft(&w, n);
    (spec[..=n / 2].iter().map(|c| c.norm_sqr()).collect(), fs / n as f64)
}

/// -6 dB point below 200 Hz of the 1/2-octave-smoothed response, relative to the
/// median of the 1/3-octave response over 200 Hz..2 kHz (both robust to resonances
/// and reflection dips).
fn lf_rolloff(grid: &[f64], resp3: &[f64], resp_half: &[f64]) -> f64 {
    let mut mid: Vec<f64> = grid.iter().zip(resp3).filter(|(f, _)| (200.0..=2000.0).contains(*f)).map(|(_, v)| *v).collect();
    mid.sort_by(f64::total_cmp);
    let reference = mid.get(mid.len() / 2).cloned().unwrap_or(0.0);
    let start = grid.iter().rposition(|&f| f <= LEVEL_BAND.0).unwrap_or(0);
    let mut lo = grid[start];
    for i in (0..=start).rev() {
        if resp_half[i].is_nan() || resp_half[i] < reference - 6.0 {
            break;
        }
        lo = grid[i];
    }
    lo
}

fn spread(v: &[f64]) -> f64 {
    let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
    if v.len() < 2 { 0.0 } else { max - min }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

/// Median: robust to one wobbly repeat (AirPlay NTP-timed playout jitters by ~1 ms).
fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 { s[m] } else { 0.5 * (s[m - 1] + s[m]) }
}

pub fn analyze(rec: &Recording, sched: &Schedule, names: &[String]) -> Result<Analysis> {
    ensure!(!rec.channels.is_empty() && rec.rate >= 16000.0, "no usable recording");
    ensure!(sched.speakers >= 1 && sched.repeats >= 1 && names.len() == sched.speakers, "bad schedule");
    // the mic channel: the one with the most energy (the other input is unplugged)
    let ch_rms: Vec<f64> = rec
        .channels
        .iter()
        .map(|c| (c.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / c.len().max(1) as f64).sqrt())
        .collect();
    let ch = (0..ch_rms.len()).max_by(|&a, &b| ch_rms[a].total_cmp(&ch_rms[b])).unwrap();
    let peak = rec.channels[ch].iter().fold(0.0f64, |m, &v| m.max((v as f64).abs()));
    let (x, fs) = decimate(&rec.channels[ch], rec.rate);
    let r0 = fs / TIMELINE_RATE;
    let sweep = &sched.sweep;
    let ref_len = (sweep.seconds * fs * 1.01).ceil() as usize + 2;
    ensure!(
        x.len() as f64 > sched.total_frames() as f64 * r0,
        "recording too short ({:.1} s for {:.1} s of sweeps)",
        x.len() as f64 / fs,
        sched.total_frames() as f64 / TIMELINE_RATE
    );
    if ch_rms[ch] < 1e-7 {
        bail!("the mic recorded silence (is it connected, is the right input selected?)");
    }

    // pass 1: nominal clock -> find every sweep, estimate the clock ratio
    let n = next_pow2(x.len() + ref_len);
    let xs = rfft(&x, n);
    let h1 = deconvolve(&xs, sweep, fs, r0);
    let coarse = locate(&h1[..x.len()], sched, r0, fs)?;
    drop(h1);
    let (r1, _) = fit_ratio(&coarse, sched, r0);
    ensure!(
        ((r1 / r0) - 1.0).abs() < 2e-3,
        "the sweeps are not where the schedule puts them (clock off by {:.0} ppm): wrong mic sample rate, or dropouts",
        ((r1 / r0) - 1.0) * 1e6
    );

    // pass 2: deconvolve with the drift-corrected sweep, cut the IRs
    let h2 = deconvolve(&xs, sweep, fs, r1);
    drop(xs);
    let pre = (IR_PRE_SEC * fs) as usize;
    let post = (IR_POST_SEC * fs) as usize;
    let pos: Vec<f64> = coarse.iter().map(|&c| refine_peak(&h2, c, (0.003 * fs) as usize)).collect();
    let mut starts = Vec::new();
    let mut segs = Vec::new();
    for &p in &pos {
        let b = p.floor() as isize - pre as isize;
        ensure!(b >= 0 && b as usize + pre + post <= h2.len(), "a sweep is too close to the start or end of the recording");
        starts.push(b as usize);
        segs.push(h2[b as usize..b as usize + pre + post].to_vec());
    }
    let direct: Vec<Vec<f64>> = segs.iter().map(|s| direct_window(s, pre, fs)).collect();

    // fine positions of each speaker's repeats relative to its first sweep
    let mut fine = vec![0.0; sched.slots.len()];
    for spk in 0..sched.speakers {
        let k0 = sched.slot_index(spk, 0).unwrap();
        for rep in 0..sched.repeats {
            let k = sched.slot_index(spk, rep).unwrap();
            let lag = if k == k0 { 0.0 } else { gcc_phat(&direct[k0], &direct[k], fs, LEVEL_BAND) };
            fine[k] = starts[k] as f64 + lag;
        }
    }
    let (r, resid) = fit_ratio(&fine, sched, r1);
    let mic_per_ms = r * TIMELINE_RATE / 1000.0;

    // relative arrivals per repeat (vs speaker 0 in the same repeat)
    let mut arrivals = vec![Vec::new(); sched.speakers];
    for rep in 0..sched.repeats {
        let k0 = sched.slot_index(0, rep).unwrap();
        for (spk, arr) in arrivals.iter_mut().enumerate() {
            let k = sched.slot_index(spk, rep).unwrap();
            let lag = if k == k0 { 0.0 } else { gcc_phat(&direct[k0], &direct[k], fs, LEVEL_BAND) };
            let d = (starts[k] as f64 - starts[k0] as f64) + lag - (sched.slots[k].offset as f64 - sched.slots[k0].offset as f64) * r;
            arr.push(d / mic_per_ms);
        }
    }

    // raw-recording SNR: silence before the first arrival vs. each sweep
    let first = pos.iter().cloned().fold(f64::INFINITY, f64::min) as isize;
    let noise_a = (first - (1.2 * fs) as isize).max(0) as usize;
    let noise_b = (first - (0.15 * fs) as isize).max(0) as usize;
    let noise_rms = (noise_b > noise_a + (0.2 * fs) as usize).then(|| rms(&x[noise_a..noise_b]));
    let sweep_len = (sweep.seconds * r * TIMELINE_RATE) as usize;
    let sweep_rms: Vec<f64> = pos
        .iter()
        .map(|&p| {
            let a = (p as usize).min(x.len());
            rms(&x[a..(a + sweep_len).min(x.len())])
        })
        .collect();

    let grid = grid();
    let mut speakers = Vec::new();
    for spk in 0..sched.speakers {
        let ks: Vec<usize> = (0..sched.repeats).map(|rep| sched.slot_index(spk, rep).unwrap()).collect();
        let mut levels = Vec::new();
        let mut resp6 = Vec::new();
        let mut power_sum: Option<(Vec<f64>, f64)> = None;
        let mut pnr = f64::INFINITY;
        for &k in &ks {
            let (p, bin) = ir_power(&segs[k], pre, fs);
            let r6 = smooth_db(&p, bin, &grid, 6.0);
            levels.push(band_level_db(&grid, &r6, LEVEL_BAND));
            resp6.push(r6);
            match &mut power_sum {
                Some((acc, _)) => acc.iter_mut().zip(&p).for_each(|(a, b)| *a += b),
                None => power_sum = Some((p, bin)),
            }
            let noise = rms(&segs[k][..pre - (0.005 * fs) as usize]);
            let pk = segs[k][pre.saturating_sub(3)..pre + 4].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            pnr = pnr.min(db(pk / noise.max(1e-15)));
        }
        let (mut power, bin) = power_sum.unwrap();
        power.iter_mut().for_each(|v| *v /= ks.len() as f64);
        let mut response_db = smooth_db(&power, bin, &grid, 6.0);
        let resp3 = smooth_db(&power, bin, &grid, 3.0);
        let resp_half = smooth_db(&power, bin, &grid, 2.0);
        let level_db = band_level_db(&grid, &response_db, LEVEL_BAND);
        let lf_rolloff_hz = lf_rolloff(&grid, &resp3, &resp_half);
        // outside the sweep's flat band the response is not measured: leave it blank
        let (valid_lo, valid_hi) = (sweep.f1 * 1.25, sweep.f2.min(0.47 * fs) / 1.15);
        for (v, f) in response_db.iter_mut().zip(&grid) {
            if *f < valid_lo || *f > valid_hi {
                *v = f64::NAN;
            }
        }
        let mut spreads: Vec<f64> = grid
            .iter()
            .enumerate()
            .filter(|(_, f)| (100.0..=10000.0).contains(*f))
            .map(|(i, _)| spread(&resp6.iter().map(|r| r[i]).collect::<Vec<_>>()))
            .collect();
        spreads.sort_by(f64::total_cmp);
        let srms = mean(&ks.iter().map(|&k| sweep_rms[k]).collect::<Vec<_>>());
        speakers.push(SpeakerResult {
            name: names[spk].clone(),
            arrival_ms: median(&arrivals[spk]),
            arrival_spread_ms: spread(&arrivals[spk]),
            repeat_arrivals_ms: arrivals[spk].clone(),
            level_db,
            level_spread_db: spread(&levels),
            repeat_levels_db: levels,
            pnr_db: pnr,
            snr_db: noise_rms.map(|nr| db(srms / nr.max(1e-12))),
            sweep_dbfs: db(srms),
            response_spread_db: spreads.get(spreads.len() / 2).cloned().unwrap_or(0.0),
            lf_rolloff_hz,
            response_db,
            irs: ks.iter().map(|&k| segs[k].iter().map(|&v| v as f32).collect()).collect(),
        });
    }

    let mut a = Analysis {
        rate: fs,
        mic_rate: rec.rate,
        mic_channel: ch,
        channel_rms_dbfs: ch_rms.iter().map(|&v| db(v)).collect(),
        peak_dbfs: db(peak),
        noise_dbfs: noise_rms.map(db),
        clock_ppm: ((r / r0) - 1.0) * 1e6,
        drift_residual_ms: resid / mic_per_ms,
        repeats: sched.repeats,
        grid_hz: grid,
        speakers,
        issues: Vec::new(),
    };
    a.issues = quality(&a);
    Ok(a)
}

/// Sanity and consistency checks; fatal ones block applying the result.
pub fn quality(a: &Analysis) -> Vec<Issue> {
    let mut v = Vec::new();
    let mut fatal = |t: String| v.push(Issue { fatal: true, text: t });
    if a.peak_dbfs > -0.2 {
        fatal(format!("The mic signal clipped (peak {:.1} dBFS). Lower the input gain on the Focusrite or the sweep level.", a.peak_dbfs));
    }
    if a.clock_ppm.abs() > 1000.0 {
        fatal(format!("Mic clock off by {:.0} ppm: wrong sample rate or dropouts.", a.clock_ppm));
    }
    // Tolerances reflect real hardware: AirPlay's NTP-timed playout wobbles by
    // ~1 ms between sweeps on some receivers (measured 1.2-3.4 ms on a Home View),
    // so only refuse clearly broken runs and use the median of the repeats.
    if a.drift_residual_ms > 1.5 {
        fatal(format!("Timing jumped by {:.2} ms between sweeps: the recording had dropouts or a speaker re-synced. Try again.", a.drift_residual_ms));
    }
    for s in &a.speakers {
        if s.pnr_db < 20.0 {
            fatal(format!("{}: sweep barely detectable at the mic (IR peak-to-noise {:.0} dB). Raise the sweep level or mic gain, or reduce noise.", s.name, s.pnr_db));
        }
        if s.arrival_spread_ms > 2.5 {
            fatal(format!("{}: arrival time varied by {:.2} ms between repeats.", s.name, s.arrival_spread_ms));
        }
        if s.level_spread_db > 3.0 {
            fatal(format!("{}: level varied by {:.1} dB between repeats (noise or something moved?).", s.name, s.level_spread_db));
        }
        if s.arrival_ms.abs() > 200.0 {
            fatal(format!("{}: implausible arrival offset {:.1} ms.", s.name, s.arrival_ms));
        }
    }
    let mut warn = |t: String| v.push(Issue { fatal: false, text: t });
    if (0.3..=1.5).contains(&a.drift_residual_ms) {
        warn(format!("Speaker timing wobbled by {:.2} ms during the measurement (normal for AirPlay's clock mode); the median is used.", a.drift_residual_ms));
    }
    for s in &a.speakers {
        if s.pnr_db >= 20.0 && s.pnr_db < 35.0 {
            warn(format!("{}: low signal-to-noise (IR peak-to-noise {:.0} dB); results may be rough.", s.name, s.pnr_db));
        }
        if s.sweep_dbfs < -55.0 {
            warn(format!("{}: quiet at the mic ({:.0} dBFS RMS); more mic gain would help.", s.name, s.sweep_dbfs));
        }
        if (0.5..=2.5).contains(&s.arrival_spread_ms) {
            warn(format!("{}: arrival time varied by {:.2} ms between repeats (median used).", s.name, s.arrival_spread_ms));
        }
        if (1.0..=3.0).contains(&s.level_spread_db) {
            warn(format!("{}: level varied by {:.1} dB between repeats.", s.name, s.level_spread_db));
        }
        if s.response_spread_db > 2.0 {
            warn(format!("{}: frequency response differs by {:.1} dB between repeats (noisy room?).", s.name, s.response_spread_db));
        }
    }
    if a.repeats < 2 {
        warn("Only one sweep per speaker: no consistency check and no clock-drift correction.".into());
    }
    if a.speakers.len() >= 2 {
        let lv: Vec<f64> = a.speakers.iter().map(|s| s.level_db).collect();
        if spread(&lv) > 12.0 {
            warn(format!("Speakers differ by {:.1} dB at the mic: check their volume settings.", spread(&lv)));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::super::sim::{SimSpeaker, simulate};
    use super::*;
    use crate::dsp::biquad::FilterKind;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("spk{i}")).collect()
    }

    /// Expected band level of a simulated speaker: gain + filters, averaged the same way.
    fn expected_level(s: &SimSpeaker, fs: f64) -> f64 {
        let g = grid();
        let db: Vec<f64> = g.iter().map(|&f| s.response_db(f, fs)).collect();
        band_level_db(&g, &db, LEVEL_BAND)
    }

    #[test]
    fn recovers_delay_level_and_clock_from_a_drifting_noisy_recording() {
        let sched = Schedule::new(SweepSpec::new(1.5, -20.0), 2, 3, 0.8);
        let fs = 48000.0;
        let spk = [
            SimSpeaker { delay_ms: 2.0, gain_db: -3.0, filters: vec![(FilterKind::Peaking, 400.0, 8.0, 3.0)], ..Default::default() },
            SimSpeaker {
                delay_ms: 5.7,
                gain_db: -5.5,
                filters: vec![(FilterKind::Peaking, 2000.0, -10.0, 5.0), (FilterKind::HighPass, 90.0, 0.0, 0.707)],
                reflection: Some((3.1, -9.0)),
            },
        ];
        // mic clock +43 ppm fast, recording starts 0.737 s before the first sweep
        // reaches it, mic on input 2, noise at -70 dBFS RMS
        let rec = simulate(&spk, &sched, fs, 43.0, 0.737, -70.0, 1, 7);
        let a = analyze(&rec, &sched, &names(2)).unwrap();
        assert_eq!(a.mic_channel, 1, "mic found on input 2");
        assert!((a.clock_ppm - 43.0).abs() < 2.0, "clock {} ppm", a.clock_ppm);
        let d = a.speakers[1].arrival_ms;
        assert!((d - 3.7).abs() < 0.1, "relative delay {d} ms (want 3.7)");
        assert!(a.speakers[1].arrival_spread_ms < 0.02, "{}", a.speakers[1].arrival_spread_ms);
        assert_eq!(a.speakers[0].arrival_ms, 0.0);
        let want = expected_level(&spk[1], fs) - expected_level(&spk[0], fs);
        let got = a.speakers[1].level_db - a.speakers[0].level_db;
        assert!((got - want).abs() < 0.3, "level diff {got} dB (want {want})");
        assert!(a.speakers.iter().all(|s| s.pnr_db > 40.0), "{:?}", a.speakers.iter().map(|s| s.pnr_db).collect::<Vec<_>>());
        assert!(a.ok(), "{:?}", a.issues);
        // the response shows the 400 Hz resonance on speaker 0 relative to 1 kHz
        let g = &a.grid_hz;
        let at = |s: &SpeakerResult, f: f64| s.response_db[g.iter().position(|&x| x >= f).unwrap()];
        let bump = at(&a.speakers[0], 400.0) - at(&a.speakers[0], 1000.0);
        assert!((bump - 8.0).abs() < 2.0, "resonance seen as {bump} dB");
        // the high-pass on speaker 1 shows as a low-frequency roll-off near 90 Hz
        let lo = a.speakers[1].lf_rolloff_hz;
        assert!((60.0..130.0).contains(&lo), "roll-off {lo}");
        assert!(a.speakers[1].irs.len() == 3 && a.speakers[1].irs[0].len() == ((IR_PRE_SEC + IR_POST_SEC) * fs) as usize);
    }

    #[test]
    fn negative_offsets_other_rates_and_slow_clocks() {
        // right speaker EARLIER by 1.234 ms, 44.1 kHz mic running 80 ppm slow,
        // recording at 96 kHz decimated: exercise the other code paths
        let sched = Schedule::new(SweepSpec::new(1.5, -12.0), 2, 2, 0.8);
        let spk = [
            SimSpeaker { delay_ms: 10.0, gain_db: 0.0, ..Default::default() },
            SimSpeaker { delay_ms: 8.766, gain_db: 1.5, ..Default::default() },
        ];
        for (fs, ppm) in [(44100.0, -80.0), (96000.0, 25.0)] {
            let rec = simulate(&spk, &sched, fs, ppm, 1.9, -75.0, 0, 3);
            let a = analyze(&rec, &sched, &names(2)).unwrap();
            assert_eq!(a.mic_channel, 0);
            let d = a.speakers[1].arrival_ms;
            assert!((d + 1.234).abs() < 0.1, "{fs} Hz: delay {d}");
            let l = a.speakers[1].level_db - a.speakers[0].level_db;
            assert!((l - 1.5).abs() < 0.3, "{fs} Hz: level {l}");
            assert!((a.clock_ppm - ppm).abs() < 3.0, "{fs} Hz: clock {}", a.clock_ppm);
        }
    }

    #[test]
    fn refuses_nonsense() {
        let sched = Schedule::new(SweepSpec::new(1.5, -20.0), 2, 2, 0.8);
        let spk = [SimSpeaker::default(), SimSpeaker::default()];
        // pure noise: no sweep found / not consistent -> error or fatal issues
        let mut rec = simulate(&spk, &sched, 48000.0, 0.0, 1.0, -40.0, 0, 5);
        let silent = SimSpeaker { gain_db: -200.0, ..Default::default() };
        let noise_only = simulate(&[silent.clone(), silent], &sched, 48000.0, 0.0, 1.0, -40.0, 0, 5);
        match analyze(&noise_only, &sched, &names(2)) {
            Err(_) => {}
            Ok(a) => assert!(!a.ok(), "noise must not pass: {:?}", a.speakers.iter().map(|s| s.pnr_db).collect::<Vec<_>>()),
        }
        // clipping is flagged
        rec.channels[0][1000] = 1.0;
        let a = analyze(&rec, &sched, &names(2)).unwrap();
        assert!(a.issues.iter().any(|i| i.fatal && i.text.contains("clipped")), "{:?}", a.issues);
        // too short a recording is an error
        let short = Recording { rate: 48000.0, channels: vec![rec.channels[0][..48000].to_vec()] };
        assert!(analyze(&short, &sched, &names(2)).is_err());
    }
}
