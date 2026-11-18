//! Per-speaker DSP: channel routing, trim gain, sample-accurate delay, and the global
//! (Windows master volume) gain with a per-packet linear ramp; output is 16-bit
//! big-endian PCM, which is what AirPlay `ct=1` expects (pyatv byte-swaps native s16
//! on little-endian hosts before sending).
//!
//! Settings are live: the UI/controller writes a [`LiveDsp`] and the sender thread picks
//! the change up at the next packet. Changes are click-free: gain ramps over one packet,
//! a role change crossfades the old and new routing over one packet, and a delay change
//! crossfades the old and new delay taps over [`DELAY_XFADE_FRAMES`] (changes arriving
//! mid-crossfade are queued, newest wins).
//!
//! ## Stages
//!
//! Per packet, for each speaker: delay line -> routing -> **EQ** ([`eq::EqChain`], a
//! biquad chain on the routed frame, before the gains so a trim or the Windows volume
//! never changes what the EQ sees) -> trim x global gain -> + test-chime overlay ->
//! + calibration probe -> s16.
//!
//! The pair-wide **spatial** stage ([`spatial::SpatialStage`]) is not here: it needs
//! both input channels before any per-speaker routing, so the sender runs it once on
//! the shared input (`engine::spawn_sender`).
//!
//! **Calibration probes** ([`LiveDsp::schedule_probe`]) are samples mixed into this
//! speaker's output at exact timeline frame positions, after every other stage (no
//! delay, EQ, trim or volume): the calibration routine measures the raw speaker, then
//! proposes absolute delay / gain / EQ settings.

pub mod biquad;
pub mod eq;
pub mod spatial;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::audio::Source;
use eq::{EqChain, EqSettings};

pub const MAX_DELAY_MS: f32 = 500.0;
/// Ring buffer for the delay line: power of two above 500 ms at 44.1 kHz (22050 frames).
const RING: usize = 32768;
const RING_MASK: usize = RING - 1;
/// Crossfade length when the delay changes (~16 ms at 44.1 kHz).
pub const DELAY_XFADE_FRAMES: usize = 704;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelSource {
    Left,
    Right,
    #[default]
    Mono,
    Stereo,
}

impl ChannelSource {
    pub const ALL: [ChannelSource; 4] = [ChannelSource::Left, ChannelSource::Right, ChannelSource::Mono, ChannelSource::Stereo];

    pub fn label(self) -> &'static str {
        match self {
            ChannelSource::Left => "Left",
            ChannelSource::Right => "Right",
            ChannelSource::Mono => "Mono (L+R)",
            ChannelSource::Stereo => "Stereo",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DspSettings {
    pub source: ChannelSource,
    pub gain_db: f32,
    pub delay_ms: f32,
    pub eq: EqSettings,
}

impl Default for DspSettings {
    fn default() -> Self {
        Self { source: ChannelSource::Stereo, gain_db: 0.0, delay_ms: 0.0, eq: EqSettings::default() }
    }
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Windows master volume -> linear gain: 10^(((scalar-1)*50)/20), unity at 100 %,
/// silent when muted (and at exactly 0 %, where Windows is silent too).
pub fn master_volume_gain(scalar: f32, muted: bool) -> f32 {
    if muted || scalar <= 0.0 {
        return 0.0;
    }
    let s = scalar.min(1.0);
    10f32.powf(((s - 1.0) * 50.0) / 20.0)
}

fn delay_frames_for(ms: f32, sample_rate: u32) -> usize {
    let ms = if ms.is_finite() { ms.clamp(0.0, MAX_DELAY_MS) } else { 0.0 };
    (ms as f64 * sample_rate as f64 / 1000.0).round() as usize
}

/// Shared, live-changeable DSP state for one speaker. Writers call [`LiveDsp::set`];
/// the sender's [`SpeakerDsp`] notices the new generation at its next packet.
///
/// The overlay slot mixes a short source (the "Test" chime) into this speaker only,
/// after routing, from the next packet on. Probes are the calibration sweeps: mixed in
/// raw at exact timeline frames (see [`LiveDsp::schedule_probe`]).
pub struct LiveDsp {
    settings: Mutex<DspSettings>,
    generation: AtomicU64,
    overlay: Mutex<Option<Box<dyn Source>>>,
    overlay_active: AtomicBool,
    probes: Mutex<Vec<Probe>>,
    probes_active: AtomicBool,
    probe_cancel: AtomicBool,
}

/// Samples to play on one speaker starting at timeline frame `start_frame` (frame `n`
/// of the shared timeline is sample `n % 352` of packet `n / 352`; every speaker
/// plays the same frame at the same moment).
#[derive(Clone, Debug)]
pub struct Probe {
    pub start_frame: u64,
    pub samples: Arc<Vec<f32>>,
}

impl std::fmt::Debug for LiveDsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveDsp").field("settings", &self.get()).field("overlay", &self.overlay_playing()).finish()
    }
}

impl LiveDsp {
    pub fn new(settings: DspSettings) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings),
            generation: AtomicU64::new(0),
            overlay: Mutex::new(None),
            overlay_active: AtomicBool::new(false),
            probes: Mutex::new(Vec::new()),
            probes_active: AtomicBool::new(false),
            probe_cancel: AtomicBool::new(false),
        })
    }

    pub fn get(&self) -> DspSettings {
        self.settings.lock().unwrap().clone()
    }

    /// Replace the settings; returns true if anything changed.
    pub fn set(&self, s: DspSettings) -> bool {
        let mut cur = self.settings.lock().unwrap();
        if *cur == s {
            return false;
        }
        *cur = s;
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Mix `source` into this speaker's output until it finishes (replaces any
    /// overlay already playing).
    pub fn play_overlay(&self, source: Box<dyn Source>) {
        *self.overlay.lock().unwrap() = Some(source);
        self.overlay_active.store(true, Ordering::Release);
    }

    pub fn overlay_playing(&self) -> bool {
        self.overlay_active.load(Ordering::Acquire)
    }

    /// Play `samples` (mono, both output channels) on this speaker from timeline frame
    /// `start_frame` on, unaffected by delay, EQ, trim and volume.
    pub fn schedule_probe(&self, start_frame: u64, samples: Arc<Vec<f32>>) {
        self.probes.lock().unwrap().push(Probe { start_frame, samples });
        self.probes_active.store(true, Ordering::Release);
    }

    /// Stop all probes: one still playing fades out over the next packet.
    pub fn cancel_probes(&self) {
        if self.probes_active.load(Ordering::Acquire) {
            self.probe_cancel.store(true, Ordering::Release);
        }
    }

    /// Drop all probes at once (use only when none can be playing).
    pub fn clear_probes(&self) {
        self.probes.lock().unwrap().clear();
        self.probes_active.store(false, Ordering::Release);
        self.probe_cancel.store(false, Ordering::Release);
    }

    /// True while probes are scheduled or playing.
    #[cfg_attr(not(test), allow(dead_code))] // inspection (tests)
    pub fn probes_pending(&self) -> bool {
        self.probes_active.load(Ordering::Acquire)
    }

    /// Write the probe samples for frames `frame0..frame0 + out.len()` into `out`
    /// (zeros elsewhere). Returns false (and leaves `out` alone) if there are none.
    fn fill_probes(&self, frame0: u64, out: &mut [f32]) -> bool {
        if !self.probes_active.load(Ordering::Acquire) {
            return false;
        }
        let mut probes = self.probes.lock().unwrap();
        out.fill(0.0);
        let end = frame0 + out.len() as u64;
        for p in probes.iter() {
            let p_end = p.start_frame + p.samples.len() as u64;
            if p.start_frame >= end || p_end <= frame0 {
                continue;
            }
            for f in p.start_frame.max(frame0)..p_end.min(end) {
                out[(f - frame0) as usize] += p.samples[(f - p.start_frame) as usize];
            }
        }
        if self.probe_cancel.swap(false, Ordering::AcqRel) {
            let n = out.len() as f32;
            for (i, o) in out.iter_mut().enumerate() {
                *o *= 1.0 - (i + 1) as f32 / n;
            }
            probes.clear();
        }
        probes.retain(|p| p.start_frame + p.samples.len() as u64 > end);
        if probes.is_empty() {
            self.probes_active.store(false, Ordering::Release);
        }
        true
    }

    /// Fill `out` from the overlay (zeros if none); drops it once finished.
    fn fill_overlay(&self, out: &mut [[f32; 2]]) -> bool {
        if !self.overlay_playing() {
            return false;
        }
        let mut slot = self.overlay.lock().unwrap();
        let Some(src) = slot.as_mut() else {
            self.overlay_active.store(false, Ordering::Release);
            return false;
        };
        src.fill(out);
        if src.finished() {
            *slot = None;
            self.overlay_active.store(false, Ordering::Release);
        }
        true
    }
}

pub struct SpeakerDsp {
    sample_rate: u32,
    live: Option<Arc<LiveDsp>>,
    seen_generation: u64,
    source: ChannelSource,
    /// Routing being crossfaded away from during the current packet.
    prev_source: Option<ChannelSource>,
    /// Current linear trim gain, and the one we ramp to over the next packet.
    gain: f32,
    target_gain: f32,
    ring: Vec<[f32; 2]>,
    write: usize,
    delay_frames: usize,
    /// (old delay, frames of crossfade left)
    delay_xfade: Option<(usize, usize)>,
    /// Newest delay requested while a crossfade was running (applied after it).
    pending_delay: Option<usize>,
    last_global: Option<f32>,
    overlay: Vec<[f32; 2]>,
    eq: EqChain,
    /// Routed frames of the current packet (the EQ runs on these in f64).
    routed: Vec<[f64; 2]>,
    probe: Vec<f32>,
    /// Timeline frame of the next packet when the caller does not say (tests).
    next_frame: u64,
}

impl SpeakerDsp {
    pub fn new(settings: &DspSettings, sample_rate: u32) -> Self {
        let gain = db_to_lin(settings.gain_db);
        Self {
            sample_rate,
            live: None,
            seen_generation: 0,
            source: settings.source,
            prev_source: None,
            gain,
            target_gain: gain,
            ring: vec![[0.0f32; 2]; RING],
            write: 0,
            delay_frames: delay_frames_for(settings.delay_ms, sample_rate),
            delay_xfade: None,
            pending_delay: None,
            last_global: None,
            overlay: Vec::new(),
            eq: EqChain::new(&settings.eq, sample_rate),
            routed: Vec::new(),
            probe: Vec::new(),
            next_frame: 0,
        }
    }

    /// A DSP that follows `live` (starting from its current settings, no ramp).
    pub fn with_live(live: Arc<LiveDsp>, sample_rate: u32) -> Self {
        let generation = live.generation();
        let mut dsp = Self::new(&live.get(), sample_rate);
        dsp.seen_generation = generation;
        dsp.live = Some(live);
        dsp
    }

    /// The configured delay (the target, if a change is still being crossfaded in).
    pub fn delay_frames(&self) -> usize {
        self.pending_delay.unwrap_or(self.delay_frames)
    }

    /// Apply new settings from the next packet on (click-free transitions).
    pub fn update(&mut self, s: &DspSettings) {
        if s.source != self.source {
            self.prev_source = Some(self.source);
            self.source = s.source;
        }
        self.target_gain = db_to_lin(s.gain_db);
        // Queued; process_to_be16 starts a crossfade to it once none is running,
        // so a dragged slider gives a chain of complete crossfades (no jumps).
        let d = delay_frames_for(s.delay_ms, self.sample_rate);
        self.pending_delay = (d != self.delay_frames || self.delay_xfade.is_some()).then_some(d);
        self.eq.update(&s.eq);
    }

    fn route(source: ChannelSource, f: [f32; 2]) -> [f32; 2] {
        match source {
            ChannelSource::Left => [f[0], f[0]],
            ChannelSource::Right => [f[1], f[1]],
            ChannelSource::Mono => {
                let m = 0.5 * (f[0] + f[1]);
                [m, m]
            }
            ChannelSource::Stereo => f,
        }
    }

    fn tap(&self, delay: usize) -> [f32; 2] {
        // `write` already points past the newest frame
        self.ring[(self.write.wrapping_sub(1 + delay)) & RING_MASK]
    }

    fn pick_up_live_changes(&mut self) {
        let Some(live) = &self.live else { return };
        let generation = live.generation();
        if generation != self.seen_generation {
            self.seen_generation = generation;
            let s = live.get();
            self.update(&s);
        }
    }

    /// Process one packet of stereo float frames into big-endian s16 bytes (the packet
    /// following the previous one on the timeline).
    #[cfg(test)]
    pub fn process_to_be16(&mut self, input: &[[f32; 2]], global_gain: f32, out: &mut Vec<u8>) {
        self.process_at(self.next_frame, input, global_gain, out);
    }

    /// Process the packet whose first frame is timeline frame `frame0`.
    pub fn process_at(&mut self, frame0: u64, input: &[[f32; 2]], global_gain: f32, out: &mut Vec<u8>) {
        out.clear();
        self.next_frame = frame0 + input.len() as u64;
        self.pick_up_live_changes();
        let (overlay, probe) = match self.live.clone() {
            Some(live) => {
                self.overlay.resize(input.len(), [0.0; 2]);
                self.probe.resize(input.len(), 0.0);
                (live.fill_overlay(&mut self.overlay), live.fill_probes(frame0, &mut self.probe))
            }
            None => (false, false),
        };
        let n = input.len().max(1) as f32;
        // delay line + routing
        self.routed.clear();
        for (i, &frame) in input.iter().enumerate() {
            let t = (i + 1) as f32 / n;
            // delay line (sample accurate)
            self.ring[self.write & RING_MASK] = frame;
            self.write = self.write.wrapping_add(1);
            if self.delay_xfade.is_none()
                && let Some(d) = self.pending_delay.take()
                && d != self.delay_frames
            {
                self.delay_xfade = Some((self.delay_frames, DELAY_XFADE_FRAMES));
                self.delay_frames = d;
            }
            let mut delayed = self.tap(self.delay_frames);
            if let Some((old, left)) = self.delay_xfade {
                let w = 1.0 - left as f32 / DELAY_XFADE_FRAMES as f32;
                let o = self.tap(old);
                delayed = [o[0] + (delayed[0] - o[0]) * w, o[1] + (delayed[1] - o[1]) * w];
                self.delay_xfade = (left > 1).then_some((old, left - 1));
            }
            let mut r = Self::route(self.source, delayed);
            if let Some(prev) = self.prev_source {
                let p = Self::route(prev, delayed);
                r = [p[0] + (r[0] - p[0]) * t, p[1] + (r[1] - p[1]) * t];
            }
            self.routed.push([r[0] as f64, r[1] as f64]);
        }
        // EQ stage
        self.eq.process(&mut self.routed);
        // gains, test chime, calibration probe
        let start = self.last_global.unwrap_or(global_gain);
        let (gain0, gain1) = (self.gain, self.target_gain);
        for (i, r) in self.routed.iter().enumerate() {
            let t = (i + 1) as f32 / n;
            let trim = gain0 + (gain1 - gain0) * t;
            let g = (start + (global_gain - start) * t) * trim;
            let extra = if overlay { self.overlay[i] } else { [0.0; 2] };
            let p = if probe { self.probe[i] } else { 0.0 };
            for (s, x) in [r[0] as f32, r[1] as f32].into_iter().zip(extra) {
                out.extend_from_slice(&f32_to_i16(s * g + x * trim + p).to_be_bytes());
            }
        }
        self.gain = gain1;
        self.prev_source = None;
        self.last_global = Some(global_gain);
    }
}

pub fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    fn run(settings: DspSettings, input: &[[f32; 2]], gain: f32) -> Vec<[i16; 2]> {
        let mut dsp = SpeakerDsp::new(&settings, 44100);
        let mut out = Vec::new();
        dsp.process_to_be16(input, gain, &mut out);
        decode(&out)
    }

    fn decode(out: &[u8]) -> Vec<[i16; 2]> {
        out.chunks(4)
            .map(|c| [i16::from_be_bytes([c[0], c[1]]), i16::from_be_bytes([c[2], c[3]])])
            .collect()
    }

    #[test]
    fn pcm_byte_order_matches_pyatv_wire_format() {
        // pyatv `_to_audio_samples` byte-swaps native (LE) s16 -> big-endian on the wire.
        let v = vectors();
        let native = unhex(&v["pcm"]["native_le"]);
        let wire = unhex(&v["pcm"]["wire"]);
        let samples: Vec<i16> = native.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        let ours: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
        assert_eq!(ours, wire);
        // and the DSP output path produces the same layout
        let mut dsp = SpeakerDsp::new(&DspSettings::default(), 44100);
        let mut out = Vec::new();
        dsp.process_to_be16(&[[0x1234 as f32 / 32767.0, -1.0]], 1.0, &mut out);
        assert_eq!(out, [0x12, 0x34, 0x80, 0x01]);
    }

    #[test]
    fn routing() {
        let input = [[0.5, -0.25]];
        let l = run(DspSettings { source: ChannelSource::Left, ..Default::default() }, &input, 1.0);
        let r = run(DspSettings { source: ChannelSource::Right, ..Default::default() }, &input, 1.0);
        let m = run(DspSettings { source: ChannelSource::Mono, ..Default::default() }, &input, 1.0);
        let s = run(DspSettings { source: ChannelSource::Stereo, ..Default::default() }, &input, 1.0);
        assert_eq!(l[0], [16384, 16384]);
        assert_eq!(r[0], [-8192, -8192]);
        assert_eq!(m[0], [4096, 4096]);
        assert_eq!(s[0], [16384, -8192]);
    }

    #[test]
    fn delay_is_sample_accurate_across_packets() {
        // 1 ms at 44.1 kHz = 44.1 -> 44 frames
        let settings = DspSettings { delay_ms: 1.0, ..Default::default() };
        let mut dsp = SpeakerDsp::new(&settings, 44100);
        assert_eq!(dsp.delay_frames(), 44);
        let mut input = vec![[0.0f32; 2]; 352];
        input[10] = [0.5, 0.5];
        let mut out = Vec::new();
        dsp.process_to_be16(&input, 1.0, &mut out);
        let idx = out.chunks(4).position(|c| c != [0, 0, 0, 0]).unwrap();
        assert_eq!(idx, 54);
        // an impulse near the end of a packet comes out in the next packet
        let mut input2 = vec![[0.0f32; 2]; 352];
        input2[340] = [0.5, 0.5];
        dsp.process_to_be16(&input2, 1.0, &mut out);
        dsp.process_to_be16(&vec![[0.0; 2]; 352], 1.0, &mut out);
        assert_eq!(out.chunks(4).position(|c| c != [0, 0, 0, 0]), Some(340 + 44 - 352));
        // max delay clamps to 500 ms; 0.1 ms resolution survives the frame rounding
        let big = SpeakerDsp::new(&DspSettings { delay_ms: 9999.0, ..Default::default() }, 44100);
        assert_eq!(big.delay_frames(), 22050);
        let fine = SpeakerDsp::new(&DspSettings { delay_ms: 12.3, ..Default::default() }, 44100);
        assert_eq!(fine.delay_frames(), 542); // 12.3 ms * 44.1 = 542.43
    }

    #[test]
    fn gain_and_master_volume_ramp() {
        assert!((master_volume_gain(1.0, false) - 1.0).abs() < 1e-6);
        assert!((master_volume_gain(0.5, false) - 10f32.powf(-25.0 / 20.0)).abs() < 1e-6);
        assert_eq!(master_volume_gain(0.8, true), 0.0);
        let out = run(DspSettings { gain_db: -6.0206, ..Default::default() }, &[[1.0, 1.0]], 1.0);
        assert!((out[0][0] as i32 - 16384).abs() <= 1);

        // ramp from 1.0 to 0.0 over one packet: monotonic, ends at 0, no jump at start
        let mut dsp = SpeakerDsp::new(&DspSettings::default(), 44100);
        let mut out = Vec::new();
        let ones = vec![[0.5f32, 0.5]; 352];
        dsp.process_to_be16(&ones, 1.0, &mut out);
        dsp.process_to_be16(&ones, 0.0, &mut out);
        let s: Vec<i16> = out.chunks(4).map(|c| i16::from_be_bytes([c[0], c[1]])).collect();
        assert!(s[0] > 16000);
        assert!(s.windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(*s.last().unwrap(), 0);
    }

    fn packet(dsp: &mut SpeakerDsp, input: &[[f32; 2]]) -> Vec<[i16; 2]> {
        let mut out = Vec::new();
        dsp.process_to_be16(input, 1.0, &mut out);
        decode(&out)
    }

    #[test]
    fn live_role_and_gain_changes_ramp_over_one_packet() {
        let live = LiveDsp::new(DspSettings { source: ChannelSource::Left, ..Default::default() });
        let mut dsp = SpeakerDsp::with_live(live.clone(), 44100);
        let input = vec![[0.5f32, -0.5]; 352];
        assert!(packet(&mut dsp, &input).iter().all(|f| *f == [16384, 16384]));

        // role change: crossfade left -> right across exactly one packet
        assert!(live.set(DspSettings { source: ChannelSource::Right, ..Default::default() }));
        assert!(!live.set(DspSettings { source: ChannelSource::Right, ..Default::default() }), "unchanged");
        let p = packet(&mut dsp, &input);
        assert!(p[0][0] > 15000, "starts near the old routing: {:?}", p[0]);
        assert_eq!(*p.last().unwrap(), [-16384, -16384]);
        assert!(p.windows(2).all(|w| w[1][0] <= w[0][0]), "monotonic crossfade");
        assert!(packet(&mut dsp, &input).iter().all(|f| *f == [-16384, -16384]));

        // gain change: linear ramp to the new trim, then steady
        live.set(DspSettings { source: ChannelSource::Right, gain_db: -6.0206, ..Default::default() });
        let p = packet(&mut dsp, &input);
        assert!(p[0][0] < -16000);
        assert!((p.last().unwrap()[0] as i32 + 8192).abs() <= 1);
        assert!(p.windows(2).all(|w| w[1][0] >= w[0][0]));
        assert!(packet(&mut dsp, &input).iter().all(|f| (f[0] as i32 + 8192).abs() <= 1));
    }

    #[test]
    fn live_delay_change_crossfades_then_lands_on_new_tap() {
        let live = LiveDsp::new(DspSettings::default());
        let mut dsp = SpeakerDsp::with_live(live.clone(), 44100);
        let silence = vec![[0.0f32; 2]; 352];
        // a steady signal stays steady through a delay change (no gap, no click)
        let steady = vec![[0.25f32, 0.25]; 352];
        packet(&mut dsp, &steady);
        packet(&mut dsp, &steady);
        live.set(DspSettings { delay_ms: 5.0, ..Default::default() });
        for _ in 0..4 {
            assert!(packet(&mut dsp, &steady).iter().all(|f| (f[0] as i32 - 8192).abs() <= 1));
        }
        assert_eq!(dsp.delay_frames(), 221); // 5 ms
        // after the crossfade an impulse comes out exactly 221 frames later
        for _ in 0..(RING / 352 + 1) {
            packet(&mut dsp, &silence);
        }
        let mut imp = silence.clone();
        imp[0] = [0.5, 0.5];
        let mut out = packet(&mut dsp, &imp);
        out.extend(packet(&mut dsp, &silence));
        assert_eq!(out.iter().position(|f| f[0] != 0), Some(221));
    }

    #[test]
    fn dragging_the_delay_slider_stays_continuous() {
        // a new delay every packet (faster than the crossfade): no jumps in a 100 Hz sine
        let live = LiveDsp::new(DspSettings::default());
        let mut dsp = SpeakerDsp::with_live(live.clone(), 44100);
        let mut phase = 0u64;
        let mut sine = || {
            let v: Vec<[f32; 2]> = (0..352)
                .map(|i| {
                    let s = (0.5 * (2.0 * std::f64::consts::PI * 100.0 * (phase + i) as f64 / 44100.0).sin()) as f32;
                    [s, s]
                })
                .collect();
            phase += 352;
            v
        };
        let mut prev: Option<i16> = None;
        let mut max_step = 0i32;
        for k in 0..120u32 {
            live.set(DspSettings { delay_ms: 5.0 + ((k * 37) % 45) as f32 + 0.1 * k as f32, ..Default::default() });
            for f in packet(&mut dsp, &sine()) {
                if let Some(p) = prev {
                    max_step = max_step.max((f[0] as i32 - p as i32).abs());
                }
                prev = Some(f[0]);
            }
        }
        // a 0.5-peak 100 Hz sine moves <= 233 LSB per sample; crossfades add a little
        assert!(max_step < 600, "discontinuity of {max_step} LSB");
        // and it settles on the last requested delay
        let last = delay_frames_for(5.0 + ((119 * 37) % 45) as f32 + 11.9, 44100);
        assert_eq!(dsp.delay_frames(), last);
        for _ in 0..4 {
            packet(&mut dsp, &sine());
        }
        assert_eq!((dsp.delay_frames, dsp.pending_delay, dsp.delay_xfade), (last, None, None));
    }

    #[test]
    fn overlay_plays_on_this_speaker_then_ends() {
        let live = LiveDsp::new(DspSettings { source: ChannelSource::Left, ..Default::default() });
        let mut dsp = SpeakerDsp::with_live(live.clone(), 44100);
        let silence = vec![[0.0f32; 2]; 352];
        live.play_overlay(Box::new(crate::audio::tone::ToneSource::new(0.5, 0.15)));
        assert!(live.overlay_playing());
        let mut peak = 0;
        // muted global gain does not silence the test chime
        for _ in 0..70 {
            let mut out = Vec::new();
            dsp.process_to_be16(&silence, 0.0, &mut out);
            peak = decode(&out).iter().fold(peak, |p, f| p.max(f[0].unsigned_abs()));
        }
        assert!(peak > 1000, "chime audible: {peak}");
        assert!(!live.overlay_playing(), "finished overlay is dropped");
        assert!(packet(&mut dsp, &silence).iter().all(|f| *f == [0, 0]));
    }
}
