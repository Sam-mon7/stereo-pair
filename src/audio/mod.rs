//! Audio sources feeding the shared timeline. A source produces 44.1 kHz stereo float
//! frames on demand, one packet (352 frames) at a time, and never blocks: when it has
//! nothing it returns silence so the stream keeps flowing.

pub mod capture;
pub mod resample;
pub mod tone;
pub mod volume;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tracing::{debug, info};

use capture::CaptureShared;
use resample::Resampler;

pub trait Source: Send {
    /// Fill `out` (44.1 kHz stereo frames). Must not block.
    fn fill(&mut self, out: &mut [[f32; 2]]);
    /// True once a finite source has nothing more to play.
    fn finished(&self) -> bool {
        false
    }
}

/// Keeps an engine input alive (capture thread etc.) and describes it for the UI.
pub struct InputHandle {
    describe: Box<dyn Fn() -> String + Send>,
    _keep: Box<dyn Send>,
}

impl InputHandle {
    pub fn new(describe: impl Fn() -> String + Send + 'static, keep: impl Send + 'static) -> Self {
        Self { describe: Box::new(describe), _keep: Box::new(keep) }
    }

    /// e.g. `loopback: "Speakers (Focusrite USB Audio)" (2 ch, 44100 Hz, F32)`
    pub fn describe(&self) -> String {
        (self.describe)()
    }
}

/// Open the engine input: WASAPI loopback of the default output device.
///
/// Extension point (not implemented yet): a selectable input. A capture-device input
/// (e.g. the Focusrite "Analogue 1 + 2" line input carrying a PS5) is the same shape as
/// [`LoopbackSource`]: open the device with cpal's `build_input_stream` on an *input*
/// device instead of the default output, feed a `CaptureShared` queue, and wrap it in
/// `LoopbackSource` for drift-compensated resampling. A config field (e.g.
/// `input = "loopback" | { device = "..." }`) would select which one this opens; the
/// same capture path would record the measurement mic for the calibration routine.
pub fn open_loopback() -> anyhow::Result<(Box<dyn Source>, InputHandle)> {
    let capture = capture::Capture::start()?;
    let shared = capture.shared.clone();
    let source = Box::new(LoopbackSource::new(capture.shared.clone()));
    let describe = move || {
        let s = shared.lock().unwrap();
        if s.rate == 0 { "loopback: opening default output...".to_string() } else { format!("loopback: \"{}\" ({})", s.device, s.format) }
    };
    Ok((source, InputHandle::new(describe, capture)))
}

/// WASAPI loopback -> drift-compensating resampler.
///
/// The capture device runs on its own crystal; we consume on the NTP clock. A target
/// fill level (in input frames) is held by trimming the resampling ratio by up to
/// +-1000 ppm. When capture stops delivering (Windows sends nothing while the PC is
/// silent) the source plays silence and re-primes once data returns.
pub struct LoopbackSource {
    shared: Arc<Mutex<CaptureShared>>,
    local: VecDeque<[f32; 2]>,
    resampler: Option<Resampler>,
    generation: u64,
    running: bool,
    fill_avg: f64,
    underruns: u64,
}

const TARGET_FILL_SEC: f64 = 0.05;
const MAX_TRIM: f64 = 0.001;

impl LoopbackSource {
    pub fn new(shared: Arc<Mutex<CaptureShared>>) -> Self {
        Self {
            shared,
            local: VecDeque::new(),
            resampler: None,
            generation: u64::MAX,
            running: false,
            fill_avg: 0.0,
            underruns: 0,
        }
    }
}

impl Source for LoopbackSource {
    fn fill(&mut self, out: &mut [[f32; 2]]) {
        // Move whatever the capture thread has into our local queue.
        let rate = {
            let mut s = self.shared.lock().unwrap();
            if s.generation != self.generation {
                // device (re)opened: start over at its rate
                self.generation = s.generation;
                self.local.clear();
                self.resampler = (s.rate > 0).then(|| Resampler::new(s.rate, 44100));
                self.running = false;
                if s.rate > 0 {
                    info!("capture format: {} Hz -> 44100 Hz", s.rate);
                }
            }
            self.local.extend(s.frames.drain(..));
            s.rate
        };
        let Some(rs) = self.resampler.as_mut() else {
            out.fill([0.0; 2]);
            return;
        };
        let target = (rate as f64 * TARGET_FILL_SEC).max(512.0);
        if !self.running {
            if (self.local.len() as f64) < target + rs.input_needed(out.len()) as f64 {
                out.fill([0.0; 2]);
                return;
            }
            rs.reset();
            self.fill_avg = target;
            self.running = true;
            debug!("loopback: audio present, streaming ({} frames buffered)", self.local.len());
        }
        // Drop excess if we fell far behind (e.g. after a stall) to bound latency.
        if self.local.len() as f64 > target * 6.0 {
            let excess = self.local.len() - target as usize;
            self.local.drain(..excess);
            debug!("loopback: dropped {excess} frames of backlog");
        }
        self.fill_avg += 0.02 * (self.local.len() as f64 - self.fill_avg);
        let trim = ((self.fill_avg - target) / target * MAX_TRIM).clamp(-MAX_TRIM, MAX_TRIM);
        let n = rs.process(&mut self.local, out, trim);
        if n < out.len() {
            out[n..].fill([0.0; 2]);
            self.running = false;
            self.underruns += 1;
            debug!("loopback: capture ran dry ({} times); sending silence", self.underruns);
        }
    }
}
