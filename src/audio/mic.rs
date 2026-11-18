//! Capture-device input for the calibration mic (e.g. the Focusrite "Analogue 1 + 2"
//! input): a level meter per channel that is always running while open, and a
//! recording buffer that is filled only between `start_recording` and
//! `stop_recording` (kept in memory, never written to disk).
//!
//! cpal streams are not `Send` on every backend, so the stream lives on its own thread
//! for the whole life of the [`Mic`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use tracing::{info, warn};

use crate::calib::Recording;

/// Preferred calibration input when none is configured.
pub const DEFAULT_MIC_HINT: &str = "Analogue 1 + 2";
/// Upper bound on a recording (memory guard).
pub const MAX_RECORD_SECONDS: f64 = 120.0;

/// Names of the capture (input) devices.
pub fn input_device_names() -> Vec<String> {
    cpal::default_host().input_devices().map(|it| it.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
}

/// Pick the device: one whose name contains `hint` (case-insensitive), else one
/// containing "Analogue 1 + 2", else the default input.
pub fn find_input(hint: Option<&str>) -> Option<String> {
    let names = input_device_names();
    let find = |h: &str| names.iter().find(|n| n.to_lowercase().contains(&h.to_lowercase())).cloned();
    hint.filter(|h| !h.is_empty())
        .and_then(find)
        .or_else(|| find(DEFAULT_MIC_HINT))
        .or_else(|| cpal::default_host().default_input_device().and_then(|d| d.name().ok()))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MicLevels {
    pub channels: usize,
    /// Peak and RMS since the previous call (dBFS; -inf-ish at -150 when silent).
    pub peak_dbfs: [f32; 2],
    pub rms_dbfs: [f32; 2],
}

#[derive(Default)]
struct Meter {
    peak: [f32; 2],
    sumsq: [f64; 2],
    n: u64,
}

#[derive(Default)]
struct State {
    rate: u32,
    channels: usize,
    format: String,
    error: Option<String>,
    meter: Meter,
    recording: Option<[Vec<f32>; 2]>,
    max_frames: usize,
    overflowed: bool,
}

pub struct Mic {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn on_data<T>(state: &Mutex<State>, data: &[T], channels: usize)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let Ok(mut s) = state.lock() else { return };
    let s = &mut *s;
    let max = s.max_frames;
    let mut rec = s.recording.as_mut().filter(|r| r[0].len() < max);
    for frame in data.chunks_exact(channels) {
        let l = frame[0].to_sample::<f32>();
        let r = if channels > 1 { frame[1].to_sample::<f32>() } else { l };
        for (c, v) in [l, r].into_iter().enumerate() {
            s.meter.peak[c] = s.meter.peak[c].max(v.abs());
            s.meter.sumsq[c] += (v as f64) * (v as f64);
        }
        s.meter.n += 1;
        if let Some(rec) = rec.as_mut() {
            rec[0].push(l);
            rec[1].push(r);
        }
    }
    if s.recording.as_ref().is_some_and(|r| r[0].len() >= s.max_frames) {
        s.overflowed = true;
    }
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, state: Arc<Mutex<State>>, failed: Arc<AtomicBool>) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let st2 = state.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| on_data(&state, data, channels),
        move |err| {
            warn!("mic stream error: {err}");
            if let Ok(mut s) = st2.lock() {
                s.error = Some(format!("{err}"));
            }
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )?)
}

fn open_stream(name: &str, state: &Arc<Mutex<State>>, failed: &Arc<AtomicBool>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .input_devices()?
        .find(|d| d.name().is_ok_and(|n| n == name))
        .ok_or_else(|| anyhow!("capture device \"{name}\" not found"))?;
    let supported = device.default_input_config()?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, state.clone(), failed.clone())?,
        SampleFormat::I16 => build::<i16>(&device, &config, state.clone(), failed.clone())?,
        SampleFormat::I32 => build::<i32>(&device, &config, state.clone(), failed.clone())?,
        SampleFormat::U16 => build::<u16>(&device, &config, state.clone(), failed.clone())?,
        other => bail!("unsupported capture format {other:?}"),
    };
    stream.play()?;
    let mut s = state.lock().unwrap();
    s.rate = config.sample_rate.0;
    s.channels = config.channels as usize;
    s.format = format!("{} ch, {} Hz, {:?}", config.channels, config.sample_rate.0, supported.sample_format());
    info!("mic capture: \"{name}\" ({})", s.format);
    Ok(stream)
}

fn db(x: f64) -> f32 {
    (20.0 * x.max(1e-30).log10()).max(-150.0) as f32
}

impl Mic {
    /// Open the named capture device (see [`find_input`]); waits until the stream runs.
    pub fn open(name: &str) -> Result<Mic> {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (st, sp, dev) = (state.clone(), stop.clone(), name.to_string());
        let thread = std::thread::Builder::new().name("mic-capture".into()).spawn(move || {
            let failed = Arc::new(AtomicBool::new(false));
            let stream = match open_stream(&dev, &st, &failed) {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while !sp.load(Ordering::Relaxed) && !failed.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(stream);
        })?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Mic { state, stop, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(anyhow!("capture device \"{name}\" did not start"))
            }
        }
    }

    /// e.g. `2 ch, 48000 Hz, F32`
    pub fn format(&self) -> String {
        self.state.lock().unwrap().format.clone()
    }

    /// The stream's error, if it failed.
    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// Levels since the previous call.
    pub fn take_levels(&self) -> MicLevels {
        let mut s = self.state.lock().unwrap();
        let m = std::mem::take(&mut s.meter);
        let n = m.n.max(1) as f64;
        MicLevels {
            channels: s.channels,
            peak_dbfs: [db(m.peak[0] as f64), db(m.peak[1] as f64)],
            rms_dbfs: [db((m.sumsq[0] / n).sqrt()), db((m.sumsq[1] / n).sqrt())],
        }
    }

    /// Start (or restart) recording into memory.
    pub fn start_recording(&self) {
        let mut s = self.state.lock().unwrap();
        s.max_frames = (MAX_RECORD_SECONDS * s.rate.max(8000) as f64) as usize;
        s.recording = Some([Vec::new(), Vec::new()]);
        s.overflowed = false;
    }

    /// Stop recording and hand over what was captured (channel 1 and 2).
    pub fn stop_recording(&self) -> Result<Recording> {
        let mut s = self.state.lock().unwrap();
        let rec = s.recording.take().ok_or_else(|| anyhow!("not recording"))?;
        if s.overflowed {
            bail!("recording exceeded {MAX_RECORD_SECONDS} s");
        }
        if let Some(e) = &s.error {
            bail!("the mic stream failed: {e}");
        }
        let channels = if s.channels >= 2 { rec.to_vec() } else { vec![rec[0].clone()] };
        Ok(Recording { rate: s.rate as f64, channels })
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `mic-test`: list inputs, open one and print its per-channel levels (nothing is
/// recorded or saved).
pub fn mic_test(hint: Option<&str>, seconds: f64) -> Result<()> {
    println!("capture devices:");
    for n in input_device_names() {
        println!("  {n}");
    }
    let name = find_input(hint).ok_or_else(|| anyhow!("no capture device"))?;
    let mic = Mic::open(&name)?;
    println!("opened \"{name}\" ({})", mic.format());
    let start = Instant::now();
    mic.take_levels();
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(500));
        let l = mic.take_levels();
        println!(
            "{:>5.1}s  in1 peak {:>6.1} rms {:>6.1} dBFS   in2 peak {:>6.1} rms {:>6.1} dBFS",
            start.elapsed().as_secs_f64(),
            l.peak_dbfs[0],
            l.rms_dbfs[0],
            l.peak_dbfs[1],
            l.rms_dbfs[1]
        );
    }
    Ok(())
}
