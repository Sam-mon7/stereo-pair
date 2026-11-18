//! A running calibration, driven by the controller's supervisor every ~100 ms.
//!
//! [`Job::start`] (stream running, every target speaker streaming):
//! 1. renders the sweep and schedules it as probes on the targets, round-robin, the
//!    first one `lead_seconds` ahead of the next packet to be sent;
//! 2. ramps the programme to silence and starts the mic recording.
//!
//! [`Job::tick`] then watches the stream (same timeline, every target still
//! streaming, mic healthy) until the timeline has sent the last sweep plus latency
//! and a tail; then it unmutes, stops the recording and analyses it on a worker
//! thread. Any problem aborts: probes are cancelled (one-packet fade), the programme
//! is unmuted and the recording dropped.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tracing::{info, warn};

use super::analysis::{Analysis, Recording, analyze};
use super::fit::{Proposal, propose};
use super::{Schedule, SweepSpec, TIMELINE_RATE};
use crate::config::Config;
use crate::dsp::LiveDsp;
use crate::dsp::eq::BandSource;
use crate::engine::{Engine, SlotId};
use crate::raop::packets::FRAMES_PER_PACKET;
use crate::raop::timeline::Timeline;

/// Where the calibration reads the mic from (a capture device in production).
pub trait MicInput: Send + Sync {
    fn start_recording(&self);
    fn stop_recording(&self) -> Result<Recording>;
    /// A stream failure, if any.
    fn error(&self) -> Option<String>;
}

impl MicInput for crate::audio::mic::Mic {
    fn start_recording(&self) {
        crate::audio::mic::Mic::start_recording(self)
    }
    fn stop_recording(&self) -> Result<Recording> {
        crate::audio::mic::Mic::stop_recording(self)
    }
    fn error(&self) -> Option<String> {
        crate::audio::mic::Mic::error(self)
    }
}

pub struct CalibrationRequest {
    pub mic: Arc<dyn MicInput>,
    /// Peak level of the sweeps (dBFS, raw: not scaled by the Windows volume).
    pub level_dbfs: f64,
    pub repeats: usize,
    pub sweep_seconds: f64,
    pub gap_seconds: f64,
    /// Muted, recorded silence before the first sweep (noise floor).
    pub lead_seconds: f64,
}

pub const DEFAULT_LEVEL_DBFS: f64 = -20.0;
pub const DEFAULT_REPEATS: usize = 3;
pub const DEFAULT_SWEEP_SECONDS: f64 = 3.0;
pub const DEFAULT_GAP_SECONDS: f64 = 1.5;
pub const DEFAULT_LEAD_SECONDS: f64 = 2.0;
/// Recorded after the last sweep has played (room decay).
const TAIL_SECONDS: f64 = 1.0;

impl CalibrationRequest {
    pub fn new(mic: Arc<dyn MicInput>, level_dbfs: f64, repeats: usize) -> Self {
        Self {
            mic,
            level_dbfs,
            repeats,
            sweep_seconds: DEFAULT_SWEEP_SECONDS,
            gap_seconds: DEFAULT_GAP_SECONDS,
            lead_seconds: DEFAULT_LEAD_SECONDS,
        }
    }

    /// Total time from Start until the analysis begins.
    pub fn duration(&self, speakers: usize, latency_s: f64) -> f64 {
        let n = (speakers * self.repeats) as f64;
        self.lead_seconds + (n - 1.0) * (self.sweep_seconds + self.gap_seconds) + self.sweep_seconds + latency_s + TAIL_SECONDS
    }
}

/// One speaker being calibrated.
#[derive(Clone)]
pub struct Target {
    pub key: String,
    pub name: String,
    pub slot: SlotId,
    pub live: Arc<LiveDsp>,
}

#[derive(Debug)]
pub struct CalibrationResult {
    /// Config keys of the speakers, in schedule order.
    pub keys: Vec<String>,
    pub schedule: Schedule,
    pub analysis: Analysis,
    pub proposal: Proposal,
    /// Timeline frame of the first sweep.
    pub first_frame: u64,
}

impl CalibrationResult {
    /// Write the proposal into `cfg`: absolute delay and trim, calibration EQ bands
    /// replaced (manual bands kept), EQ un-bypassed. Returns how many speakers changed.
    pub fn apply_to(&self, cfg: &mut Config) -> usize {
        let mut n = 0;
        for (key, p) in self.keys.iter().zip(&self.proposal.speakers) {
            let Some(s) = cfg.speakers.iter_mut().find(|s| s.key() == *key) else { continue };
            s.delay_ms = p.delay_ms as f32;
            s.gain_db = p.gain_db as f32;
            s.eq.bands.retain(|b| b.source != BandSource::Calibration);
            let room = crate::dsp::eq::MAX_BANDS.saturating_sub(s.eq.bands.len());
            s.eq.bands.extend(p.bands.iter().take(room).cloned());
            s.eq.bypass = false;
            n += 1;
        }
        n
    }
}

#[derive(Clone, Debug, Default)]
pub enum CalibrationState {
    #[default]
    Idle,
    Measuring {
        progress: f32,
        detail: String,
        first_frame: u64,
    },
    Analyzing,
    Done(Arc<CalibrationResult>),
    Failed(String),
}

impl CalibrationState {
    pub fn busy(&self) -> bool {
        matches!(self, CalibrationState::Measuring { .. } | CalibrationState::Analyzing)
    }
}

type Analysed = Result<(Analysis, Proposal)>;

enum Phase {
    Measuring,
    Analyzing(JoinHandle<Analysed>),
    Finished,
}

pub struct Job {
    mic: Arc<dyn MicInput>,
    targets: Vec<Target>,
    schedule: Schedule,
    timeline: Arc<Timeline>,
    first_frame: u64,
    /// Timeline frame (send side) after which the recording is complete.
    end_frame: u64,
    begin_frame: u64,
    phase: Phase,
    deadline: Instant,
}

impl Job {
    pub fn start(req: CalibrationRequest, targets: Vec<Target>, engine: &Engine) -> Result<(Job, CalibrationState)> {
        if targets.is_empty() {
            bail!("no speaker is streaming");
        }
        if !(1..=5).contains(&req.repeats) {
            bail!("repeats must be 1..5");
        }
        let level = req.level_dbfs.clamp(-60.0, -3.0);
        let sweep = SweepSpec::new(req.sweep_seconds, level);
        let schedule = Schedule::new(sweep.clone(), targets.len(), req.repeats, req.gap_seconds);
        let samples: Arc<Vec<f32>> = Arc::new(sweep.render(TIMELINE_RATE).into_iter().map(|v| v as f32).collect());
        let timeline = engine.timeline();
        let begin_frame = timeline.next_packet() * FRAMES_PER_PACKET as u64;
        let first_frame = begin_frame + (req.lead_seconds * TIMELINE_RATE) as u64;
        let end_frame = first_frame + schedule.total_frames() + timeline.latency as u64 + (TAIL_SECONDS * TIMELINE_RATE) as u64;
        for t in &targets {
            t.live.clear_probes();
        }
        for slot in &schedule.slots {
            targets[slot.speaker].live.schedule_probe(first_frame + slot.offset, samples.clone());
        }
        req.mic.start_recording();
        engine.set_programme_mute(true);
        let seconds = (end_frame - begin_frame) as f64 / TIMELINE_RATE;
        info!(
            "calibration: {} speaker(s) x {} sweep(s) of {:.1} s at {:.0} dBFS, first at frame {first_frame}, {:.0} s",
            targets.len(),
            req.repeats,
            req.sweep_seconds,
            level,
            seconds
        );
        let job = Job {
            mic: req.mic,
            targets,
            schedule,
            timeline,
            first_frame,
            end_frame,
            begin_frame,
            phase: Phase::Measuring,
            deadline: Instant::now() + Duration::from_secs_f64(seconds + 20.0),
        };
        let state = job.measuring_state(begin_frame);
        Ok((job, state))
    }

    pub fn first_frame(&self) -> u64 {
        self.first_frame
    }

    pub fn measuring(&self) -> bool {
        matches!(self.phase, Phase::Measuring)
    }

    fn measuring_state(&self, sent: u64) -> CalibrationState {
        let latency = self.timeline.latency as u64;
        let played = sent.saturating_sub(latency);
        let progress = (sent.saturating_sub(self.begin_frame)) as f32 / (self.end_frame - self.begin_frame).max(1) as f32;
        let sweep = self.schedule.sweep_frames();
        let detail = if played < self.first_frame {
            "Programme muted; recording the room's noise floor...".to_string()
        } else if let Some((i, s)) = self
            .schedule
            .slots
            .iter()
            .enumerate()
            .find(|(_, s)| (self.first_frame + s.offset..self.first_frame + s.offset + sweep).contains(&played))
        {
            format!("Sweep {} of {}: {}", i + 1, self.schedule.slots.len(), self.targets[s.speaker].name)
        } else if played >= self.first_frame + self.schedule.total_frames() {
            "Recording the room decay...".to_string()
        } else {
            "Pause between sweeps...".to_string()
        };
        CalibrationState::Measuring { progress: progress.min(1.0), detail, first_frame: self.first_frame }
    }

    /// Stop everything; the state to publish.
    pub fn abort(&mut self, engine: Option<&Engine>, why: &str) -> CalibrationState {
        if matches!(self.phase, Phase::Measuring) {
            for t in &self.targets {
                t.live.cancel_probes();
            }
            if let Some(e) = engine {
                e.set_programme_mute(false);
            }
            let _ = self.mic.stop_recording();
        }
        self.phase = Phase::Finished;
        warn!("calibration aborted: {why}");
        CalibrationState::Failed(why.to_string())
    }

    /// Advance the job. `streaming(slot)` tells whether a speaker is still streaming.
    /// Returns the state to publish, and true once the job is finished.
    pub fn tick(&mut self, engine: Option<&Engine>, streaming: impl Fn(SlotId) -> bool) -> (CalibrationState, bool) {
        match &mut self.phase {
            Phase::Finished => (CalibrationState::Idle, true),
            Phase::Analyzing(h) => {
                if !h.is_finished() {
                    return (CalibrationState::Analyzing, false);
                }
                let Phase::Analyzing(h) = std::mem::replace(&mut self.phase, Phase::Finished) else { unreachable!() };
                let res = h.join().unwrap_or_else(|_| Err(anyhow!("the analysis crashed")));
                match res {
                    Ok((analysis, proposal)) => {
                        info!(
                            "calibration analysed: {}",
                            analysis
                                .speakers
                                .iter()
                                .zip(&proposal.speakers)
                                .map(|(s, p)| format!(
                                    "{}: arrival {:+.2} ms, level {:.1} dB, PNR {:.0} dB -> delay {:.2} ms, trim {:.1} dB, {} bands",
                                    s.name,
                                    s.arrival_ms,
                                    s.level_db,
                                    s.pnr_db,
                                    p.delay_ms,
                                    p.gain_db,
                                    p.bands.len()
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        );
                        for i in &analysis.issues {
                            info!("calibration {}: {}", if i.fatal { "problem" } else { "note" }, i.text);
                        }
                        let result = CalibrationResult {
                            keys: self.targets.iter().map(|t| t.key.clone()).collect(),
                            schedule: self.schedule.clone(),
                            analysis,
                            proposal,
                            first_frame: self.first_frame,
                        };
                        (CalibrationState::Done(Arc::new(result)), true)
                    }
                    Err(e) => {
                        warn!("calibration analysis failed: {e:#}");
                        (CalibrationState::Failed(format!("{e:#}")), true)
                    }
                }
            }
            Phase::Measuring => {
                let Some(engine) = engine else {
                    return (self.abort(None, "Streaming stopped during the measurement."), true);
                };
                if !Arc::ptr_eq(&engine.timeline(), &self.timeline) {
                    return (self.abort(Some(engine), "The stream restarted during the measurement."), true);
                }
                if let Some(t) = self.targets.iter().find(|t| !streaming(t.slot)) {
                    let why = format!("{} disconnected during the measurement.", t.name);
                    return (self.abort(Some(engine), &why), true);
                }
                if let Some(e) = self.mic.error() {
                    return (self.abort(Some(engine), &format!("The mic stopped: {e}")), true);
                }
                if Instant::now() > self.deadline {
                    return (self.abort(Some(engine), "The measurement took too long (stream stalled?)."), true);
                }
                let sent = self.timeline.next_packet() * FRAMES_PER_PACKET as u64;
                if sent < self.end_frame {
                    return (self.measuring_state(sent), false);
                }
                engine.set_programme_mute(false);
                let rec = match self.mic.stop_recording() {
                    Ok(r) => r,
                    Err(e) => {
                        self.phase = Phase::Finished;
                        return (CalibrationState::Failed(format!("Recording failed: {e:#}")), true);
                    }
                };
                info!("calibration: recorded {:.1} s at {} Hz; analysing", rec.channels[0].len() as f64 / rec.rate, rec.rate);
                let sched = self.schedule.clone();
                let names: Vec<String> = self.targets.iter().map(|t| t.name.clone()).collect();
                let spawned = std::thread::Builder::new().name("calibration-analysis".into()).spawn(move || {
                    let a = analyze(&rec, &sched, &names)?;
                    let p = propose(&a);
                    Ok((a, p))
                });
                match spawned {
                    Ok(h) => {
                        self.phase = Phase::Analyzing(h);
                        (CalibrationState::Analyzing, false)
                    }
                    Err(e) => {
                        self.phase = Phase::Finished;
                        (CalibrationState::Failed(format!("cannot start the analysis: {e}")), true)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::analysis::Analysis;
    use crate::calib::fit::{Proposal, SpeakerProposal};
    use crate::dsp::eq::EqBand;

    #[test]
    fn apply_replaces_calibration_bands_and_keeps_manual_ones() {
        let mut cfg = Config::default();
        cfg.speakers[0].eq.bands.push(EqBand::peaking(100.0, 2.0, 1.0));
        cfg.speakers[0].eq.bands.push(EqBand { source: BandSource::Calibration, ..EqBand::peaking(300.0, -3.0, 2.0) });
        cfg.speakers[0].eq.bypass = true;
        cfg.speakers[1].gain_db = -1.0;
        let sp = |d: f64, g: f64, bands: Vec<EqBand>| SpeakerProposal {
            delay_ms: d,
            gain_db: g,
            bands,
            normalized_db: vec![],
            eq_db: vec![],
            corrected_db: vec![],
            fit_lo_hz: 0.0,
            fit_hi_hz: 0.0,
            error_rms_before: 0.0,
            error_rms_after: 0.0,
        };
        let new_band = EqBand { source: BandSource::Calibration, ..EqBand::peaking(410.0, -6.0, 4.0) };
        let result = CalibrationResult {
            keys: vec![cfg.speakers[0].key(), cfg.speakers[1].key()],
            schedule: Schedule::new(SweepSpec::new(1.0, -20.0), 2, 1, 1.0),
            analysis: Analysis {
                rate: 44100.0,
                mic_rate: 44100.0,
                mic_channel: 0,
                channel_rms_dbfs: vec![],
                peak_dbfs: 0.0,
                noise_dbfs: None,
                clock_ppm: 0.0,
                drift_residual_ms: 0.0,
                repeats: 1,
                grid_hz: vec![],
                speakers: vec![],
                issues: vec![],
            },
            proposal: Proposal { target_db: vec![], target_lo_hz: 0.0, speakers: vec![sp(3.7, -2.5, vec![new_band.clone()]), sp(0.0, 0.0, vec![])] },
            first_frame: 0,
        };
        assert_eq!(result.apply_to(&mut cfg), 2);
        let s0 = &cfg.speakers[0];
        assert_eq!((s0.delay_ms, s0.gain_db, s0.eq.bypass), (3.7, -2.5, false));
        assert_eq!(s0.eq.bands, vec![EqBand::peaking(100.0, 2.0, 1.0), new_band]);
        assert_eq!((cfg.speakers[1].delay_ms, cfg.speakers[1].gain_db), (0.0, 0.0));
    }
}
