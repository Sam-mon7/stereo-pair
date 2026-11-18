//! Corrections from an [`Analysis`]:
//!
//! * **Delay**: the latest-arriving speaker gets 0; every other speaker is delayed by
//!   how much earlier it arrives (absolute values, replacing the current delays).
//! * **Gain**: the quietest speaker (band level 200 Hz..4 kHz) gets 0 dB; louder ones
//!   are trimmed down to it (never boosted).
//! * **EQ**: all speakers are matched to one common smooth target: a quadratic in
//!   log-frequency fitted to their mean level-normalised response over the shared
//!   usable band. Using the measurements' own smooth trend as the target keeps the
//!   speakers' overall voicing and cancels any smooth colouration of the (uncalibrated)
//!   mic, which is common to every speaker. Per speaker, up to [`MAX_FIT_BANDS`]
//!   peaking bands are fitted greedily to the 1/6-octave error: cuts up to
//!   [`MAX_CUT_DB`], boosts at most [`MAX_BOOST_DB`] (dips are mostly room
//!   cancellations that boosting cannot fix: only dips at least 1/3 octave wide, by
//!   half their depth, and only above 2 x the roll-off), only between 1.25 x the
//!   speaker's own low-frequency roll-off and [`FIT_HI_HZ`]. Errors under 1 dB are
//!   left alone, and above [`NARROW_BELOW_HZ`] only broad bands (Q <= 2.5) are used.
//!   The target is fitted from 2 x the highest roll-off to 12 kHz and held flat
//!   outside that band.

use serde::Serialize;

use super::analysis::Analysis;
use super::{LEVEL_BAND, TIMELINE_RATE};
use crate::dsp::eq::{BandSource, EqBand};

pub const MAX_FIT_BANDS: usize = 6;
pub const MAX_CUT_DB: f64 = 10.0;
pub const MAX_BOOST_DB: f64 = 3.0;
pub const FIT_HI_HZ: f64 = 12000.0;
pub const FIT_LO_MIN_HZ: f64 = 40.0;
/// Boosts only from this multiple of the roll-off up (no bass-extension boosts).
pub const BOOST_ABOVE_ROLLOFF: f64 = 2.0;
/// Dips narrower than this (octaves at half depth) are not boosted at all.
pub const MIN_BOOST_WIDTH_OCT: f64 = 1.0 / 3.0;
const MIN_ERROR_DB: f64 = 1.5;
/// Above this, only broad bands (the in-room response there depends on the exact
/// mic position; narrow corrections would chase reflections).
pub const NARROW_BELOW_HZ: f64 = 500.0;

fn max_q(f: f64) -> f64 {
    if f < NARROW_BELOW_HZ { 8.0 } else { 2.5 }
}

#[derive(Clone, Debug, Serialize)]
pub struct SpeakerProposal {
    pub delay_ms: f64,
    pub gain_db: f64,
    pub bands: Vec<EqBand>,
    /// Measured response, level-normalised (dB, on the analysis grid).
    pub normalized_db: Vec<f64>,
    /// Response of the proposed EQ (dB, on the grid).
    pub eq_db: Vec<f64>,
    /// normalized + eq: what the speaker should measure like afterwards.
    pub corrected_db: Vec<f64>,
    pub fit_lo_hz: f64,
    pub fit_hi_hz: f64,
    /// RMS deviation from the target inside the fit band, before / after (dB).
    pub error_rms_before: f64,
    pub error_rms_after: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    /// Common target (dB, level-normalised, on the grid).
    pub target_db: Vec<f64>,
    pub target_lo_hz: f64,
    pub speakers: Vec<SpeakerProposal>,
}

fn band_db(b: &EqBand, grid: &[f64]) -> Vec<f64> {
    let c = b.coeffs(TIMELINE_RATE);
    grid.iter().map(|&f| c.magnitude_db(f, TIMELINE_RATE)).collect()
}

/// Least-squares quadratic in u = log2(f / 1 kHz).
fn quad_fit(grid: &[f64], y: &[f64], lo: f64, hi: f64) -> [f64; 3] {
    let mut m = [[0.0; 3]; 3];
    let mut v = [0.0; 3];
    for (f, y) in grid.iter().zip(y) {
        if *f < lo || *f > hi {
            continue;
        }
        let u = (f / 1000.0).log2();
        let p = [1.0, u, u * u];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += p[i] * p[j];
            }
            v[i] += p[i] * y;
        }
    }
    // Gaussian elimination (3x3)
    for c in 0..3 {
        let piv = (c..3).max_by(|&a, &b| m[a][c].abs().total_cmp(&m[b][c].abs())).unwrap();
        m.swap(c, piv);
        v.swap(c, piv);
        if m[c][c].abs() < 1e-12 {
            return [0.0; 3];
        }
        for r in 0..3 {
            if r != c {
                let k = m[r][c] / m[c][c];
                for j in 0..3 {
                    m[r][j] -= k * m[c][j];
                }
                v[r] -= k * v[c];
            }
        }
    }
    [v[0] / m[0][0], v[1] / m[1][1], v[2] / m[2][2]]
}

fn rms_in(grid: &[f64], e: &[f64], lo: f64, hi: f64) -> f64 {
    let (s, n) = grid.iter().zip(e).filter(|(f, _)| **f >= lo && **f <= hi).fold((0.0, 0), |(s, n), (_, e)| (s + e * e, n + 1));
    (s / n.max(1) as f64).sqrt()
}

/// Greedy peaking-band fit to `err` (measured - target, dB): cuts anywhere in
/// `lo..hi`, boosts only from `boost_lo` up and only for broad dips.
fn fit_bands(grid: &[f64], err: &[f64], lo: f64, boost_lo: f64, hi: f64) -> Vec<EqBand> {
    let idx: Vec<usize> = (0..grid.len()).filter(|&i| grid[i] >= lo && grid[i] <= hi).collect();
    if idx.is_empty() {
        return Vec::new();
    }
    let cost = |r: &[f64]| idx.iter().map(|&i| r[i] * r[i]).sum::<f64>();
    let mut resid = err.to_vec();
    let mut bands: Vec<EqBand> = Vec::new();
    // points that may no longer seed a band (dips already handled or rejected)
    let mut skip = vec![false; grid.len()];
    // peaks count fully, dips half (mostly room nulls), dips below boost_lo not at all
    let weight = |i: usize, e: f64| if e > 0.0 { e } else if grid[i] >= boost_lo { -0.5 * e } else { 0.0 };
    for _attempt in 0..4 * MAX_FIT_BANDS {
        if bands.len() >= MAX_FIT_BANDS {
            break;
        }
        let Some(&i0) = idx.iter().filter(|&&i| !skip[i]).max_by(|&&a, &&b| weight(a, resid[a]).total_cmp(&weight(b, resid[b])))
        else {
            break;
        };
        let e0 = resid[i0];
        if weight(i0, e0) < MIN_ERROR_DB {
            break;
        }
        // width at half height (RBJ bandwidth is between the half-gain points)
        let same = |i: usize| resid[i].signum() == e0.signum() && resid[i].abs() >= e0.abs() / 2.0;
        let (mut il, mut ir) = (i0, i0);
        while il > 0 && grid[il - 1] >= lo && same(il - 1) {
            il -= 1;
        }
        while ir + 1 < grid.len() && grid[ir + 1] <= hi && same(ir + 1) {
            ir += 1;
        }
        let bw = (grid[ir] / grid[il]).log2().max(1.0 / 12.0);
        let boost = e0 < 0.0;
        if boost && bw < MIN_BOOST_WIDTH_OCT {
            skip[il..=ir].iter_mut().for_each(|s| *s = true);
            continue;
        }
        let q0 = (2f64.powf(bw).sqrt() / (2f64.powf(bw) - 1.0)).clamp(0.6, 8.0);
        let g0 = if boost { (-0.5 * e0).min(MAX_BOOST_DB) } else { -e0.min(MAX_CUT_DB) };
        let f_lo = if boost { boost_lo } else { lo };
        // small local search around the first guess
        let before = cost(&resid);
        let mut best: Option<(f64, EqBand, Vec<f64>)> = None;
        for gm in [0.6, 0.8, 1.0, 1.15] {
            for qm in [0.7, 1.0, 1.4] {
                for fo in [-1.0 / 24.0, 0.0, 1.0 / 24.0] {
                    let f = (grid[i0] * 2f64.powf(fo)).clamp(f_lo, hi);
                    let g = (g0 * gm).clamp(-MAX_CUT_DB, MAX_BOOST_DB);
                    // boosts stay local (no broad bass lifts towards the roll-off)
                    let q_min = if boost { 1.0 } else { 0.5 };
                    let b = EqBand {
                        source: BandSource::Calibration,
                        ..EqBand::peaking(f as f32, g as f32, (q0 * qm).clamp(q_min, max_q(f)) as f32)
                    };
                    let bd = band_db(&b, grid);
                    let r: Vec<f64> = resid.iter().zip(&bd).map(|(a, b)| a + b).collect();
                    let c = cost(&r);
                    if best.as_ref().is_none_or(|(bc, _, _)| c < *bc) {
                        best = Some((c, b, r));
                    }
                }
            }
        }
        let (c, b, r) = best.unwrap();
        if c > before * 0.98 || b.gain_db.abs() < 0.5 {
            skip[il..=ir].iter_mut().for_each(|s| *s = true);
            continue;
        }
        if boost {
            // one boost per dip
            skip[il..=ir].iter_mut().for_each(|s| *s = true);
        }
        resid = r;
        bands.push(b);
    }
    // overlapping boosts must not add up beyond the boost limit
    let total: Vec<f64> = grid.iter().map(|&f| bands.iter().map(|b| b.coeffs(TIMELINE_RATE).magnitude_db(f, TIMELINE_RATE)).sum()).collect();
    let max_boost = total.iter().cloned().fold(0.0, f64::max);
    if max_boost > MAX_BOOST_DB + 0.25 {
        let k = MAX_BOOST_DB / max_boost;
        for b in bands.iter_mut().filter(|b| b.gain_db > 0.0) {
            b.gain_db *= k as f32;
        }
    }
    for b in bands.iter_mut() {
        b.freq_hz = (b.freq_hz * 10.0).round() / 10.0;
        if b.gain_db > 0.0 && (b.freq_hz as f64) < boost_lo {
            b.freq_hz = ((boost_lo * 10.0).ceil() / 10.0) as f32;
        }
        b.gain_db = (b.gain_db * 10.0).round() / 10.0;
        b.q = (b.q * 100.0).round() / 100.0;
    }
    bands.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    bands
}

pub fn propose(a: &Analysis) -> Proposal {
    let grid = &a.grid_hz;
    let n = a.speakers.len();
    let latest = a.speakers.iter().map(|s| s.arrival_ms).fold(f64::NEG_INFINITY, f64::max);
    let quietest = a.speakers.iter().map(|s| s.level_db).fold(f64::INFINITY, f64::min);
    let normalized: Vec<Vec<f64>> = a.speakers.iter().map(|s| s.response_db.iter().map(|v| v - s.level_db).collect()).collect();
    // the target is fitted where every speaker is well inside its passband
    let target_lo = a.speakers.iter().map(|s| s.lf_rolloff_hz * BOOST_ABOVE_ROLLOFF).fold(FIT_LO_MIN_HZ, f64::max);
    let mean: Vec<f64> = (0..grid.len()).map(|i| normalized.iter().map(|r| r[i]).sum::<f64>() / n.max(1) as f64).collect();
    let c = quad_fit(grid, &mean, target_lo, FIT_HI_HZ);
    // outside the band it was fitted on, the target is held flat (no extrapolation)
    let mut target: Vec<f64> = grid
        .iter()
        .map(|f| {
            let u = (f.clamp(target_lo, FIT_HI_HZ) / 1000.0).log2();
            c[0] + c[1] * u + c[2] * u * u
        })
        .collect();
    // level-neutral: the target averages 0 dB over the level band, like the measurements
    let t_mean = mean_in(grid, &target, LEVEL_BAND);
    target.iter_mut().for_each(|t| *t -= t_mean);

    let speakers = a
        .speakers
        .iter()
        .zip(&normalized)
        .map(|(s, norm)| {
            let lo = (s.lf_rolloff_hz * 1.25).max(FIT_LO_MIN_HZ);
            let mut err: Vec<f64> = norm.iter().zip(&target).map(|(m, t)| m - t).collect();
            let off = mean_in(grid, &err, LEVEL_BAND);
            err.iter_mut().for_each(|e| *e -= off);
            let normalized_db: Vec<f64> = norm.iter().map(|v| v - off).collect();
            let bands = fit_bands(grid, &err, lo, (s.lf_rolloff_hz * BOOST_ABOVE_ROLLOFF).max(lo), FIT_HI_HZ);
            let eq_db: Vec<f64> = grid.iter().map(|&f| bands.iter().map(|b| b.coeffs(TIMELINE_RATE).magnitude_db(f, TIMELINE_RATE)).sum()).collect();
            let after: Vec<f64> = err.iter().zip(&eq_db).map(|(e, q)| e + q).collect();
            SpeakerProposal {
                delay_ms: ((latest - s.arrival_ms) * 100.0).round() / 100.0,
                gain_db: ((quietest - s.level_db) * 10.0).round() / 10.0,
                corrected_db: normalized_db.iter().zip(&eq_db).map(|(m, q)| m + q).collect(),
                error_rms_before: rms_in(grid, &err, lo, FIT_HI_HZ),
                error_rms_after: rms_in(grid, &after, lo, FIT_HI_HZ),
                normalized_db,
                eq_db,
                bands,
                fit_lo_hz: lo,
                fit_hi_hz: FIT_HI_HZ,
            }
        })
        .collect();
    Proposal { target_db: target, target_lo_hz: target_lo, speakers }
}

fn mean_in(grid: &[f64], v: &[f64], band: (f64, f64)) -> f64 {
    let (s, n) = grid.iter().zip(v).filter(|(f, _)| **f >= band.0 && **f <= band.1).fold((0.0, 0), |(s, n), (_, v)| (s + v, n + 1));
    s / n.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::super::analysis::analyze;
    use super::super::sim::{SimSpeaker, simulate};
    use super::super::{Schedule, SweepSpec};
    use super::*;
    use crate::dsp::biquad::FilterKind;

    #[test]
    fn proposes_delay_trim_and_a_sensible_conservative_eq() {
        let sched = Schedule::new(SweepSpec::new(1.5, -20.0), 2, 3, 0.8);
        let spk = [
            // left: resonance at 400 Hz, early, louder
            SimSpeaker {
                delay_ms: 1.0,
                gain_db: -1.0,
                filters: vec![(FilterKind::Peaking, 400.0, 8.0, 3.0), (FilterKind::HighPass, 70.0, 0.0, 0.707)],
                ..Default::default()
            },
            // right: deep notch at 2.5 kHz (must not be filled), later, quieter, rolls off at 120 Hz
            SimSpeaker {
                delay_ms: 4.7,
                gain_db: -3.5,
                filters: vec![(FilterKind::Peaking, 2500.0, -12.0, 6.0), (FilterKind::HighPass, 120.0, 0.0, 0.707)],
                ..Default::default()
            },
        ];
        let rec = simulate(&spk, &sched, 48000.0, 12.0, 0.6, -72.0, 0, 11);
        let a = analyze(&rec, &sched, &["L".into(), "R".into()]).unwrap();
        assert!(a.ok(), "{:?}", a.issues);
        let p = propose(&a);
        // delay goes on the earlier (left) speaker, trim on the louder (left) one
        assert!((p.speakers[0].delay_ms - 3.7).abs() < 0.1, "{}", p.speakers[0].delay_ms);
        assert_eq!(p.speakers[1].delay_ms, 0.0);
        assert_eq!(p.speakers[1].gain_db, 0.0);
        let want_trim = a.speakers[1].level_db - a.speakers[0].level_db;
        assert!((p.speakers[0].gain_db - want_trim).abs() < 0.06 && p.speakers[0].gain_db < -1.5, "{}", p.speakers[0].gain_db);

        for (i, s) in p.speakers.iter().enumerate() {
            assert!(s.bands.len() <= MAX_FIT_BANDS);
            for b in &s.bands {
                assert_eq!(b.source, BandSource::Calibration);
                assert_eq!(b.kind, FilterKind::Peaking);
                assert!(b.gain_db <= MAX_BOOST_DB as f32 + 1e-3 && b.gain_db >= -MAX_CUT_DB as f32, "{b:?}");
                assert!(b.freq_hz as f64 >= s.fit_lo_hz - 1e-3 && b.freq_hz as f64 <= FIT_HI_HZ, "speaker {i}: {b:?} outside {}", s.fit_lo_hz);
            }
            let max_boost = s.eq_db.iter().cloned().fold(0.0, f64::max);
            assert!(max_boost <= MAX_BOOST_DB + 0.3, "speaker {i} boosts {max_boost} dB");
            assert!(s.error_rms_after < s.error_rms_before, "speaker {i} gets closer to the target");
        }
        // the left resonance is cut: a band within 1/4 octave of 400 Hz, -4..-10 dB
        let near = |b: &&EqBand| ((b.freq_hz as f64) / 400.0).log2().abs() < 0.25;
        let cut = p.speakers[0].bands.iter().filter(near).min_by(|a, b| a.gain_db.total_cmp(&b.gain_db));
        let cut = cut.unwrap_or_else(|| panic!("no cut near 400 Hz: {:?}", p.speakers[0].bands));
        assert!((-10.0..=-4.0).contains(&cut.gain_db), "{cut:?}");
        // and the corrected response is flatter there
        let g = &a.grid_hz;
        let i400 = g.iter().position(|&f| f >= 400.0).unwrap();
        let dev = |v: &[f64]| (v[i400] - p.target_db[i400]).abs();
        assert!(dev(&p.speakers[0].corrected_db) < dev(&p.speakers[0].normalized_db) / 2.0);
        // between 200 Hz and 8 kHz (away from the roll-off, which is not boosted) the
        // left speaker ends up close to the target
        let dev_in = |v: &[f64]| {
            let pts: Vec<f64> = (0..g.len()).filter(|&i| (200.0..=8000.0).contains(&g[i])).map(|i| (v[i] - p.target_db[i]).abs()).collect();
            (pts.iter().map(|d| d * d).sum::<f64>() / pts.len() as f64).sqrt()
        };
        assert!(dev_in(&p.speakers[0].corrected_db) < 0.6 * dev_in(&p.speakers[0].normalized_db));
        assert!(dev_in(&p.speakers[0].corrected_db) < 1.0);
        // the right notch is not "filled" with more than the boost limit
        let i2k5 = g.iter().position(|&f| f >= 2500.0).unwrap();
        assert!(p.speakers[1].eq_db[i2k5] <= MAX_BOOST_DB + 0.3);
        // nothing below each speaker's roll-off (120 Hz Butterworth HP: -6 dB at ~91 Hz)
        assert!((80.0..100.0).contains(&a.speakers[1].lf_rolloff_hz), "{}", a.speakers[1].lf_rolloff_hz);
        assert!(p.speakers[1].fit_lo_hz > 100.0, "{}", p.speakers[1].fit_lo_hz);
        // no boosts near either roll-off
        for (s, m) in p.speakers.iter().zip(&a.speakers) {
            assert!(s.bands.iter().all(|b| b.gain_db <= 0.0 || b.freq_hz as f64 >= 2.0 * m.lf_rolloff_hz), "{:?}", s.bands);
        }
        // the notch gets at most a gentle boost (checked above: total <= +3 dB)
    }

    #[test]
    fn identical_flat_speakers_get_no_eq() {
        let sched = Schedule::new(SweepSpec::new(1.5, -20.0), 2, 2, 0.8);
        let spk = [SimSpeaker { delay_ms: 3.0, ..Default::default() }, SimSpeaker { delay_ms: 3.0, ..Default::default() }];
        let rec = simulate(&spk, &sched, 48000.0, 0.0, 0.5, -80.0, 0, 2);
        let a = analyze(&rec, &sched, &["L".into(), "R".into()]).unwrap();
        let p = propose(&a);
        for s in &p.speakers {
            assert!(s.bands.is_empty(), "{:?}", s.bands);
            assert!(s.delay_ms.abs() < 0.05 && s.gain_db.abs() < 0.15, "{} {}", s.delay_ms, s.gain_db);
        }
    }
}
