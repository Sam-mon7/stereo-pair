//! WASAPI shared-mode loopback capture of the default render endpoint (via cpal: an
//! input stream on an output device enables AUDCLNT_STREAMFLAGS_LOOPBACK).
//!
//! The capture thread only appends frames to a shared queue; pacing is done by the
//! sender on the NTP clock. If the default device changes or the stream errors, the
//! stream is reopened and `generation` bumps so the consumer restarts its resampler.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use tracing::{info, warn};

#[derive(Default)]
pub struct CaptureShared {
    pub frames: VecDeque<[f32; 2]>,
    /// Capture sample rate, 0 while no device is open.
    pub rate: u32,
    pub generation: u64,
    pub device: String,
    /// Mix format of the open device, e.g. `2 ch, 48000 Hz, F32` (empty while closed).
    pub format: String,
}

pub struct Capture {
    pub shared: Arc<Mutex<CaptureShared>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

const MAX_QUEUE_SEC: usize = 2;

impl Capture {
    pub fn start() -> Result<Self> {
        let shared = Arc::new(Mutex::new(CaptureShared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2) = (shared.clone(), stop.clone());
        let handle = std::thread::Builder::new().name("wasapi-loopback".into()).spawn(move || capture_loop(s2, st2))?;
        Ok(Self { shared, stop, handle: Some(handle) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn push_frames<T>(shared: &Mutex<CaptureShared>, data: &[T], channels: usize, rate: u32)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let Ok(mut s) = shared.lock() else { return };
    for frame in data.chunks_exact(channels) {
        let l = frame[0].to_sample::<f32>();
        let r = if channels > 1 { frame[1].to_sample::<f32>() } else { l };
        s.frames.push_back([l, r]);
    }
    let cap = rate as usize * MAX_QUEUE_SEC;
    if s.frames.len() > cap {
        let excess = s.frames.len() - cap;
        s.frames.drain(..excess);
    }
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: Arc<Mutex<CaptureShared>>, failed: Arc<AtomicBool>) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| push_frames(&shared, data, channels, rate),
        move |err| {
            warn!("loopback stream error: {err}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}

fn open(shared: &Arc<Mutex<CaptureShared>>, failed: &Arc<AtomicBool>) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("no default output device"))?;
    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let supported = device.default_output_config()?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, shared.clone(), failed.clone())?,
        SampleFormat::I16 => build::<i16>(&device, &config, shared.clone(), failed.clone())?,
        SampleFormat::I32 => build::<i32>(&device, &config, shared.clone(), failed.clone())?,
        SampleFormat::U16 => build::<u16>(&device, &config, shared.clone(), failed.clone())?,
        other => return Err(anyhow!("unsupported mix format {other:?}")),
    };
    stream.play()?;
    {
        let mut s = shared.lock().unwrap();
        s.frames.clear();
        s.rate = config.sample_rate.0;
        s.generation += 1;
        s.device = name.clone();
        s.format = format!("{} ch, {} Hz, {:?}", config.channels, config.sample_rate.0, supported.sample_format());
    }
    info!(
        "loopback capture: \"{name}\" ({} ch, {} Hz, {:?})",
        config.channels,
        config.sample_rate.0,
        supported.sample_format()
    );
    Ok((stream, name))
}

fn default_device_name() -> Option<String> {
    cpal::default_host().default_output_device().and_then(|d| d.name().ok())
}

fn capture_loop(shared: Arc<Mutex<CaptureShared>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let failed = Arc::new(AtomicBool::new(false));
        let (stream, name) = match open(&shared, &failed) {
            Ok(x) => x,
            Err(e) => {
                warn!("cannot open loopback capture: {e}; retrying");
                shared.lock().unwrap().rate = 0;
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let mut ticks = 0u32;
        while !stop.load(Ordering::Relaxed) && !failed.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            ticks += 1;
            if ticks.is_multiple_of(8)
                && let Some(cur) = default_device_name()
                    && cur != name {
                        info!("default output changed to \"{cur}\"; reopening capture");
                        break;
                    }
        }
        drop(stream);
    }
}
