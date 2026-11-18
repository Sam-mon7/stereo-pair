//! Follows the Windows master volume + mute of the default render endpoint
//! (IAudioEndpointVolume, polled at 20 Hz; the endpoint is re-acquired every 2 s so a
//! default-device change is picked up). Loopback capture is pre-volume, so applying
//! this gain ourselves makes the Windows volume keys work for the speakers.
//!
//! The follower can keep running with following switched off (the tray panel still
//! shows the Windows volume); the published gain is then unity.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, warn};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};

use crate::dsp::master_volume_gain;

/// Bit 33: a reading is present; bit 32: muted; low 32 bits: scalar as f32 bits.
const STATE_VALID: u64 = 1 << 33;
const STATE_MUTED: u64 = 1 << 32;

pub struct VolumeFollower {
    gain_bits: Arc<AtomicU32>,
    follow: Arc<AtomicBool>,
    state: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl VolumeFollower {
    pub fn start() -> Result<Self> {
        Self::start_with(Arc::new(AtomicBool::new(true)))
    }

    /// `follow` false -> publish unity gain (the volume is still read for display).
    pub fn start_with(follow: Arc<AtomicBool>) -> Result<Self> {
        let gain_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let state = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (g2, f2, st2, s2) = (gain_bits.clone(), follow.clone(), state.clone(), stop.clone());
        let handle = std::thread::Builder::new().name("master-volume".into()).spawn(move || run(g2, f2, st2, s2))?;
        Ok(Self { gain_bits, follow, state, stop, handle: Some(handle) })
    }

    pub fn handle(&self) -> Arc<AtomicU32> {
        self.gain_bits.clone()
    }

    pub fn set_follow(&self, on: bool) {
        self.follow.store(on, Ordering::Relaxed);
    }

    /// Last Windows master volume reading: (scalar 0..1, muted).
    pub fn windows_volume(&self) -> Option<(f32, bool)> {
        let s = self.state.load(Ordering::Relaxed);
        (s & STATE_VALID != 0).then(|| (f32::from_bits(s as u32), s & STATE_MUTED != 0))
    }
}

impl Drop for VolumeFollower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn endpoint() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

fn run(gain_bits: Arc<AtomicU32>, follow: Arc<AtomicBool>, state: Arc<AtomicU64>, stop: Arc<AtomicBool>) {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            warn!("COM init failed ({e}); master volume will not be followed");
            return;
        }
    }
    let mut ep: Option<IAudioEndpointVolume> = None;
    let mut last: Option<(u32, bool, bool)> = None;
    let mut tick = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if ep.is_none() || tick.is_multiple_of(40) {
            match endpoint() {
                Ok(e) => ep = Some(e),
                Err(e) => {
                    debug!("no default endpoint volume: {e}");
                    ep = None;
                }
            }
        }
        let following = follow.load(Ordering::Relaxed);
        if let Some(e) = &ep {
            let read = unsafe { (e.GetMasterVolumeLevelScalar(), e.GetMute()) };
            match read {
                (Ok(scalar), Ok(mute)) => {
                    let muted = mute.as_bool();
                    state.store(
                        STATE_VALID | if muted { STATE_MUTED } else { 0 } | scalar.to_bits() as u64,
                        Ordering::Relaxed,
                    );
                    let g = if following { master_volume_gain(scalar, muted) } else { 1.0 };
                    gain_bits.store(g.to_bits(), Ordering::Relaxed);
                    let key = ((scalar * 100.0).round() as u32, muted, following);
                    if last != Some(key) {
                        if following {
                            info!(
                                "Windows volume {}%{} -> gain {:.1} dB",
                                key.0,
                                if muted { " (muted)" } else { "" },
                                if g > 0.0 { 20.0 * g.log10() } else { f32::NEG_INFINITY }
                            );
                        } else {
                            info!("Windows volume {}%{} (not followed: unity gain)", key.0, if muted { " (muted)" } else { "" });
                        }
                        last = Some(key);
                    }
                }
                _ => ep = None,
            }
        } else if !following {
            gain_bits.store(1.0f32.to_bits(), Ordering::Relaxed);
        }
        tick = tick.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(ep);
    unsafe { CoUninitialize() };
}
