//! Save a measurement to `%APPDATA%\stereo-pair\calibration\<timestamp>\`:
//! `measurement.json` (schedule, analysis, proposal), `response.csv` (per-speaker
//! measured / corrected response and the target on the analysis grid) and
//! `ir-<n>-<repeat>.wav` (32-bit float impulse responses, mono). The raw mic
//! recording is not saved.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::analysis::{Analysis, IR_PRE_SEC};
use super::fit::Proposal;
use super::Schedule;

#[derive(Serialize)]
struct Saved<'a> {
    created_utc: String,
    ir_start_before_arrival_ms: f64,
    schedule: &'a Schedule,
    analysis: &'a Analysis,
    proposal: &'a Proposal,
}

pub fn calibration_dir() -> Result<PathBuf> {
    Ok(crate::config::app_dir()?.join("calibration"))
}

/// `YYYYMMDD-HHMMSS` (UTC) of a UNIX time.
pub fn timestamp(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

pub fn write_wav_f32(path: &Path, rate: u32, samples: &[f32]) -> Result<()> {
    let data_len = (samples.len() * 4) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b).with_context(|| format!("write {}", path.display()))
}

/// Write the measurement into a new timestamped folder under `base`; returns it.
pub fn save(base: &Path, sched: &Schedule, a: &Analysis, p: &Proposal) -> Result<PathBuf> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let stamp = timestamp(now);
    let mut dir = base.join(&stamp);
    let mut n = 1;
    while dir.exists() {
        n += 1;
        dir = base.join(format!("{stamp}-{n}"));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let saved = Saved { created_utc: stamp, ir_start_before_arrival_ms: IR_PRE_SEC * 1000.0, schedule: sched, analysis: a, proposal: p };
    std::fs::write(dir.join("measurement.json"), serde_json::to_string_pretty(&saved)?)?;

    let mut csv = Vec::new();
    write!(csv, "freq_hz,target_db")?;
    for s in &a.speakers {
        let n = s.name.replace(',', " ");
        write!(csv, ",{n} measured_db,{n} normalized_db,{n} corrected_db")?;
    }
    writeln!(csv)?;
    for (i, f) in a.grid_hz.iter().enumerate() {
        write!(csv, "{f:.2},{:.2}", p.target_db[i])?;
        for (s, sp) in a.speakers.iter().zip(&p.speakers) {
            write!(csv, ",{:.2},{:.2},{:.2}", s.response_db[i], sp.normalized_db[i], sp.corrected_db[i])?;
        }
        writeln!(csv)?;
    }
    std::fs::write(dir.join("response.csv"), csv)?;

    for (i, s) in a.speakers.iter().enumerate() {
        for (rep, ir) in s.irs.iter().enumerate() {
            write_wav_f32(&dir.join(format!("ir-{}-{}.wav", i + 1, rep + 1)), a.rate.round() as u32, ir)?;
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_and_wav_header() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(1_794_960_000 + 3661), "20261118-010101");
        let dir = crate::testvec::temp_dir("wav");
        let p = dir.join("x.wav");
        write_wav_f32(&p, 48000, &[0.5, -0.25]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(b.len(), 44 + 8);
        assert_eq!(u16::from_le_bytes([b[20], b[21]]), 3);
        assert_eq!(f32::from_le_bytes(b[44..48].try_into().unwrap()), 0.5);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
