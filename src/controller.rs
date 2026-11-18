//! Control API around the in-process engine, used by the tray app.
//!
//! A [`Controller`] owns a supervisor thread that starts/stops the [`Engine`], keeps
//! it in step with the settings, drives its housekeeping and publishes a [`Status`]
//! snapshot. Calls never block on the network:
//!
//! * per-speaker role / gain / delay apply immediately through each speaker's
//!   [`LiveDsp`] (kept per endpoint across engine restarts), no reconnect;
//! * follow-Windows-volume flips the volume follower's output at once;
//! * enabling / disabling / adding a speaker connects or tears down only that one,
//!   joining the running timeline;
//! * changing latency (or the fixed device volume / timing port) restarts the stream;
//! * settings are persisted to config.toml after a short debounce.
//!
//! Extension point (not implemented yet): the calibration routine would be another
//! supervisor command next to [`Controller::test_tone`]: for each speaker in turn,
//! play a log sweep through its [`LiveDsp::play_overlay`] while recording the
//! measurement mic (a capture-device input, see [`crate::audio::open_loopback`]),
//! then derive per-speaker delay/gain (and later EQ) and apply them via `update`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};

use crate::airplay::discovery;
use crate::audio::tone::ToneSource;
use crate::audio::volume::VolumeFollower;
use crate::audio::{InputHandle, Source};
use crate::config::{Config, CredentialStore, SpeakerConfig};
use crate::dsp::LiveDsp;
use crate::dsp::spatial::LiveSpatial;
use crate::engine::{self, Engine, EngineOptions, RETRY_AFTER, SlotId, SlotState, SpeakerPlan};
use crate::hap::Credentials;

/// Test chime: same quiet chime as the `tone` command.
const TEST_SECONDS: f64 = 3.0;
const TEST_AMPLITUDE: f32 = 0.15;
const STATUS_EVERY: Duration = Duration::from_millis(250);

type OpenInput = Box<dyn FnMut() -> Result<(Box<dyn Source>, InputHandle)> + Send>;
type CredentialsFor = Box<dyn Fn(&SpeakerConfig, IpAddr) -> Result<Credentials> + Send>;

/// What the controller needs from the outside world (swappable for tests).
pub struct ControllerDeps {
    /// Opens the engine input (production: WASAPI loopback).
    pub open_input: OpenInput,
    /// Credentials for a speaker (production: credentials.toml, re-read each time so
    /// fresh pairings are picked up; transient if nothing stored).
    pub credentials: CredentialsFor,
    /// Where settings are persisted; None = not persisted.
    pub config_path: Option<PathBuf>,
    /// Windows master volume follower; None = unity gain and no volume display.
    pub volume: Option<VolumeFollower>,
    pub save_debounce: Duration,
}

impl ControllerDeps {
    pub fn production(config_path: PathBuf, follow_volume: bool) -> Self {
        let volume = match VolumeFollower::start_with(Arc::new(AtomicBool::new(follow_volume))) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("cannot follow the Windows volume: {e:#}");
                None
            }
        };
        Self {
            open_input: Box::new(crate::audio::open_loopback),
            credentials: Box::new(|_s, ip| CredentialStore::load()?.credentials_for(&ip.to_string())),
            config_path: Some(config_path),
            volume,
            save_debounce: Duration::from_millis(800),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum EngineState {
    #[default]
    Stopped,
    /// Connecting the speakers for a new stream.
    Starting,
    Running,
    /// Could not start; retried every few seconds while streaming is wanted.
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerState {
    Disabled,
    /// Enabled, stream stopped.
    Idle,
    Connecting,
    Streaming,
    /// Dropped or failed; reconnect pending.
    Retrying,
    /// Could not be prepared (bad ip, not found via mDNS, credentials).
    Error,
    /// Standalone test chime session in progress.
    Testing,
}

impl SpeakerState {
    pub fn label(self) -> &'static str {
        match self {
            SpeakerState::Disabled => "disabled",
            SpeakerState::Idle => "idle",
            SpeakerState::Connecting => "connecting",
            SpeakerState::Streaming => "streaming",
            SpeakerState::Retrying => "reconnecting",
            SpeakerState::Error => "error",
            SpeakerState::Testing => "testing",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerStatus {
    pub key: String,
    pub name: String,
    pub ip: String,
    pub enabled: bool,
    pub state: SpeakerState,
    pub detail: Option<String>,
    pub packets: u64,
    pub retransmitted: u64,
    pub send_errors: u64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub engine: EngineState,
    /// True while the user wants streaming (Start pressed / auto-start).
    pub wanted: bool,
    /// Latency of the running stream (ms).
    pub latency_ms: Option<u32>,
    pub speakers: Vec<SpeakerStatus>,
    /// Engine input, e.g. `loopback: "Speakers (...)" (2 ch, 44100 Hz, F32)`.
    pub input: Option<String>,
    /// Windows master volume (scalar, muted).
    pub windows_volume: Option<(f32, bool)>,
    pub follow_windows_volume: bool,
}

/// Coarse state for the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Off,
    Connecting,
    Streaming,
    Error,
}

impl Status {
    pub fn health(&self) -> Health {
        let enabled: Vec<&SpeakerStatus> = self.speakers.iter().filter(|s| s.enabled).collect();
        let streaming = enabled.iter().filter(|s| s.state == SpeakerState::Streaming).count();
        match &self.engine {
            EngineState::Stopped => Health::Off,
            EngineState::Starting => Health::Connecting,
            EngineState::Error(_) => Health::Error,
            EngineState::Running if !enabled.is_empty() && streaming == enabled.len() => Health::Streaming,
            EngineState::Running
                if streaming == 0 && enabled.iter().any(|s| matches!(s.state, SpeakerState::Retrying | SpeakerState::Error)) =>
            {
                Health::Error
            }
            EngineState::Running => Health::Connecting,
        }
    }

    pub fn summary(&self) -> String {
        let enabled = self.speakers.iter().filter(|s| s.enabled).count();
        let streaming = self.speakers.iter().filter(|s| s.enabled && s.state == SpeakerState::Streaming).count();
        match &self.engine {
            EngineState::Stopped => "Stopped".into(),
            EngineState::Starting => "Connecting...".into(),
            EngineState::Error(e) => format!("Error: {e}"),
            EngineState::Running => format!("Streaming to {streaming}/{enabled} speakers"),
        }
    }
}

enum Cmd {
    Start,
    Stop,
    Update(Box<Config>),
    Test(String),
    Shutdown,
}

type Waker = Box<dyn Fn() + Send>;

struct Shared {
    status: Mutex<Status>,
    config: Mutex<Config>,
    live: Mutex<HashMap<String, Arc<LiveDsp>>>,
    /// Pair-wide spatial settings, kept across engine restarts.
    spatial: Arc<LiveSpatial>,
    waker: Mutex<Option<Waker>>,
}

impl Shared {
    /// Get-or-create the live DSP for a speaker and bring it to its config.
    fn live_for(&self, s: &SpeakerConfig) -> Arc<LiveDsp> {
        let mut map = self.live.lock().unwrap();
        let live = map.entry(s.key()).or_insert_with(|| LiveDsp::new(s.dsp())).clone();
        live.set(s.dsp());
        live
    }
}

pub struct Controller {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn new(cfg: Config, deps: ControllerDeps) -> Result<Self> {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            config: Mutex::new(cfg.clone()),
            live: Mutex::new(HashMap::new()),
            spatial: LiveSpatial::new(cfg.spatial.clone()),
            waker: Mutex::new(None),
        });
        for s in &cfg.speakers {
            shared.live_for(s);
        }
        if let Some(v) = &deps.volume {
            v.set_follow(cfg.follow_windows_volume);
        }
        let (tx, rx) = channel();
        let gain = deps.volume.as_ref().map(|v| v.handle()).unwrap_or_else(|| Arc::new(AtomicU32::new(1.0f32.to_bits())));
        let sup = Supervisor {
            shared: shared.clone(),
            rx,
            saved: cfg.clone(),
            cfg,
            deps,
            gain,
            want: false,
            running: None,
            state: EngineState::Stopped,
            next_start: Instant::now(),
            speaker_errors: HashMap::new(),
            test: None,
            save_due: None,
        };
        sup.publish();
        let thread = std::thread::Builder::new().name("controller".into()).spawn(move || sup.run())?;
        Ok(Self { tx, shared, thread: Some(thread) })
    }

    /// Called (from the supervisor thread) whenever the status snapshot changes.
    pub fn set_waker(&self, f: impl Fn() + Send + 'static) {
        *self.shared.waker.lock().unwrap() = Some(Box::new(f));
    }

    pub fn start(&self) {
        let _ = self.tx.send(Cmd::Start);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Apply new settings: DSP changes take effect at the next packet; structural
    /// changes (enable, add, latency) are handled by the supervisor; persisted after
    /// a debounce.
    pub fn update(&self, cfg: Config) {
        for s in &cfg.speakers {
            self.shared.live_for(s);
        }
        self.shared.spatial.set(cfg.spatial.clone());
        *self.shared.config.lock().unwrap() = cfg.clone();
        let _ = self.tx.send(Cmd::Update(Box::new(cfg)));
    }

    /// Play the quiet test chime on one speaker (by [`SpeakerConfig::key`]): mixed into
    /// its stream if it is streaming, else through a short standalone session.
    pub fn test_tone(&self, key: &str) {
        let _ = self.tx.send(Cmd::Test(key.to_string()));
    }

    pub fn config(&self) -> Config {
        self.shared.config.lock().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    #[cfg_attr(not(test), allow(dead_code))] // inspection hook (tests, future calibration)
    pub fn live_dsp(&self, key: &str) -> Option<Arc<LiveDsp>> {
        self.shared.live.lock().unwrap().get(key).cloned()
    }

    /// Stop streaming (TEARDOWN on every speaker), flush settings and end the
    /// supervisor. Returns false if it did not finish within `timeout`.
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        let Some(thread) = self.thread.take() else { return true };
        let _ = self.tx.send(Cmd::Shutdown);
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                warn!("controller did not stop within {timeout:?}");
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = thread.join();
        true
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.shutdown(Duration::from_secs(15));
    }
}

struct Running {
    engine: Engine,
    ids: HashMap<String, SlotId>,
    input: InputHandle,
    /// (latency_ms, device_volume_db, timing_port) the stream was started with.
    signature: (u32, Option<f32>, u16),
}

struct TestRun {
    key: String,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

struct Supervisor {
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    cfg: Config,
    /// Last config written to disk.
    saved: Config,
    deps: ControllerDeps,
    gain: Arc<AtomicU32>,
    want: bool,
    running: Option<Running>,
    state: EngineState,
    next_start: Instant,
    /// Per-speaker problems outside the engine (resolve / credentials / test).
    speaker_errors: HashMap<String, String>,
    test: Option<TestRun>,
    save_due: Option<Instant>,
}

fn signature(cfg: &Config) -> (u32, Option<f32>, u16) {
    (cfg.latency_ms, cfg.device_volume_db, cfg.timing_port)
}

impl Supervisor {
    fn run(mut self) {
        let mut last_status = Instant::now();
        loop {
            let handled = match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    self.handle(cmd);
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
            };
            if let Some(r) = self.running.as_mut() {
                r.engine.tick();
            }
            if self.want && self.running.is_none() && Instant::now() >= self.next_start {
                self.try_start();
            }
            self.reap_test();
            if self.save_due.is_some_and(|t| Instant::now() >= t) {
                self.flush_save();
            }
            if handled || last_status.elapsed() >= STATUS_EVERY {
                last_status = Instant::now();
                self.publish();
            }
        }
        info!("controller shutting down");
        if let Some(t) = self.test.take() {
            t.stop.store(true, Ordering::Relaxed);
            let _ = t.handle.join();
        }
        self.want = false;
        self.stop_engine();
        self.flush_save();
        self.publish();
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start => {
                self.want = true;
                if self.running.is_none() {
                    self.try_start();
                }
            }
            Cmd::Stop => {
                self.want = false;
                self.stop_engine();
            }
            Cmd::Update(cfg) => self.apply_update(*cfg),
            Cmd::Test(key) => self.test_tone(&key),
            Cmd::Shutdown => {}
        }
    }

    fn plan_for(&self, s: &SpeakerConfig) -> Result<SpeakerPlan> {
        let ip: IpAddr = match &s.ip {
            Some(ip) => ip.parse().with_context(|| format!("bad ip {ip:?}"))?,
            None => discovery::resolve_name(&s.name, Duration::from_secs(4))?
                .ok_or_else(|| anyhow!("\"{}\" not found via mDNS", s.name))?,
        };
        let creds = (self.deps.credentials)(s, ip).context("credentials")?;
        let mut plan = SpeakerPlan::new(&s.name, ip, creds, s.dsp()).with_live(self.shared.live_for(s));
        if let Some(port) = s.port {
            plan.port = port;
        }
        Ok(plan)
    }

    fn try_start(&mut self) {
        if let Some(t) = self.test.take() {
            debug!("waiting for the test chime to finish before starting");
            let _ = t.handle.join();
        }
        self.next_start = Instant::now() + RETRY_AFTER;
        let enabled: Vec<SpeakerConfig> = self.cfg.speakers.iter().filter(|s| s.enabled).cloned().collect();
        if enabled.is_empty() {
            self.state = EngineState::Error("no speakers enabled".into());
            return;
        }
        let mut plans = Vec::new();
        let mut keys = Vec::new();
        for s in &enabled {
            match self.plan_for(s) {
                Ok(p) => {
                    self.speaker_errors.remove(&s.key());
                    plans.push(p);
                    keys.push(s.key());
                }
                Err(e) => {
                    warn!("[{}] skipped: {e:#}", s.name);
                    self.speaker_errors.insert(s.key(), format!("{e:#}"));
                }
            }
        }
        if plans.is_empty() {
            self.state = EngineState::Error("no enabled speaker could be prepared".into());
            return;
        }
        self.state = EngineState::Starting;
        self.publish();
        let (source, input) = match (self.deps.open_input)() {
            Ok(x) => x,
            Err(e) => {
                warn!("cannot open the audio input: {e:#}");
                self.state = EngineState::Error(format!("audio input: {e:#}"));
                return;
            }
        };
        let opts = EngineOptions {
            latency_frames: self.cfg.latency_frames(),
            device_volume_db: self.cfg.device_volume_db,
            timing_port: self.cfg.timing_port,
            reconnect: true,
            run_for: None,
            gain: self.gain.clone(),
            spatial: self.shared.spatial.clone(),
        };
        match Engine::start(plans, source, opts) {
            Ok((engine, ids)) => {
                self.running = Some(Running {
                    engine,
                    ids: keys.into_iter().zip(ids).collect(),
                    input,
                    signature: signature(&self.cfg),
                });
                self.state = EngineState::Running;
            }
            Err(e) => {
                warn!("cannot start streaming: {e:#}");
                self.state = EngineState::Error(format!("{e:#}"));
            }
        }
    }

    fn stop_engine(&mut self) {
        if let Some(mut r) = self.running.take() {
            r.engine.shutdown();
            drop(r.input);
        }
        self.state = EngineState::Stopped;
    }

    fn apply_update(&mut self, cfg: Config) {
        for s in &cfg.speakers {
            self.shared.live_for(s);
        }
        self.shared.spatial.set(cfg.spatial.clone());
        if let Some(v) = &self.deps.volume {
            v.set_follow(cfg.follow_windows_volume);
        }
        if cfg != self.cfg {
            self.save_due = Some(Instant::now() + self.deps.save_debounce);
        }
        self.cfg = cfg;
        let Some(running) = self.running.as_mut() else {
            if self.want {
                self.next_start = Instant::now(); // config changed: retry a failed start now
            }
            return;
        };
        if running.signature != signature(&self.cfg) {
            info!("latency {} ms requested; restarting the stream", self.cfg.latency_ms);
            self.stop_engine();
            self.try_start();
            return;
        }
        let desired: HashMap<String, &SpeakerConfig> =
            self.cfg.speakers.iter().filter(|s| s.enabled).map(|s| (s.key(), s)).collect();
        let gone: Vec<String> = running.ids.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for key in gone {
            if let Some(id) = running.ids.remove(&key) {
                running.engine.remove_speaker(id);
            }
        }
        let new: Vec<SpeakerConfig> =
            desired.iter().filter(|(k, _)| !running.ids.contains_key(*k)).map(|(_, s)| (*s).clone()).collect();
        for s in new {
            match self.plan_for(&s) {
                Ok(plan) => {
                    self.speaker_errors.remove(&s.key());
                    let running = self.running.as_mut().unwrap();
                    let id = running.engine.add_speaker(plan);
                    running.ids.insert(s.key(), id);
                }
                Err(e) => {
                    warn!("[{}] cannot add: {e:#}", s.name);
                    self.speaker_errors.insert(s.key(), format!("{e:#}"));
                }
            }
        }
    }

    fn test_tone(&mut self, key: &str) {
        let Some(s) = self.cfg.speakers.iter().find(|s| s.key() == key).cloned() else { return };
        if let Some(r) = &self.running
            && let Some(&id) = r.ids.get(key)
        {
            let streaming = r.engine.status().iter().any(|st| st.id == id && st.state == SlotState::Streaming);
            match (streaming, r.engine.live(id)) {
                (true, Some(live)) => {
                    info!("[{}] test chime (mixed into the stream)", s.name);
                    live.play_overlay(Box::new(ToneSource::new(TEST_SECONDS, TEST_AMPLITUDE)));
                }
                _ => {
                    self.speaker_errors.insert(key.into(), "test: speaker is not connected yet".into());
                }
            }
            return;
        }
        if self.test.is_some() {
            return;
        }
        let plan = match self.plan_for(&s) {
            Ok(p) => SpeakerPlan { live: LiveDsp::new(s.dsp()), ..p },
            Err(e) => {
                self.speaker_errors.insert(key.into(), format!("test: {e:#}"));
                return;
            }
        };
        self.speaker_errors.remove(key);
        info!("[{}] test chime (standalone session)", s.name);
        let latency = self.cfg.latency_frames();
        let opts = EngineOptions {
            latency_frames: latency,
            device_volume_db: self.cfg.device_volume_db,
            timing_port: 0,
            reconnect: false,
            run_for: Some(Duration::from_secs_f64(TEST_SECONDS + latency as f64 / 44100.0 + 0.75)),
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            spatial: Default::default(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let spawned = std::thread::Builder::new().name("test-chime".into()).spawn(move || {
            engine::run(vec![plan], Box::new(ToneSource::new(TEST_SECONDS, TEST_AMPLITUDE)), opts, stop2)
        });
        match spawned {
            Ok(handle) => self.test = Some(TestRun { key: key.into(), stop, handle }),
            Err(e) => {
                self.speaker_errors.insert(key.into(), format!("test: {e}"));
            }
        }
    }

    fn reap_test(&mut self) {
        if !self.test.as_ref().is_some_and(|t| t.handle.is_finished()) {
            return;
        }
        let t = self.test.take().unwrap();
        match t.handle.join() {
            Ok(Ok(())) => debug!("test chime done"),
            Ok(Err(e)) => {
                warn!("test chime failed: {e:#}");
                self.speaker_errors.insert(t.key, format!("test failed: {e:#}"));
            }
            Err(_) => {
                self.speaker_errors.insert(t.key, "test thread panicked".into());
            }
        }
    }

    fn flush_save(&mut self) {
        self.save_due = None;
        let Some(path) = &self.deps.config_path else { return };
        if self.cfg == self.saved {
            return;
        }
        match self.cfg.save_to(path) {
            Ok(()) => {
                debug!("settings saved to {}", path.display());
                self.saved = self.cfg.clone();
            }
            Err(e) => warn!("cannot save settings: {e:#}"),
        }
    }

    fn status(&self) -> Status {
        let slots = self.running.as_ref().map(|r| r.engine.status()).unwrap_or_default();
        let speakers = self
            .cfg
            .speakers
            .iter()
            .map(|s| {
                let key = s.key();
                let mut st = SpeakerStatus {
                    key: key.clone(),
                    name: s.name.clone(),
                    ip: s.ip.clone().unwrap_or_else(|| "(mDNS)".into()),
                    enabled: s.enabled,
                    state: SpeakerState::Idle,
                    detail: self.speaker_errors.get(&key).cloned(),
                    packets: 0,
                    retransmitted: 0,
                    send_errors: 0,
                };
                let slot = self
                    .running
                    .as_ref()
                    .and_then(|r| r.ids.get(&key))
                    .and_then(|id| slots.iter().find(|x| x.id == *id));
                st.state = if self.test.as_ref().is_some_and(|t| t.key == key) {
                    SpeakerState::Testing
                } else if let Some(slot) = slot {
                    st.packets = slot.packets;
                    st.retransmitted = slot.retransmitted;
                    st.send_errors = slot.send_errors;
                    match slot.state {
                        SlotState::Streaming => {
                            if self.shared.live.lock().unwrap().get(&key).is_some_and(|l| l.overlay_playing()) {
                                st.detail = Some("playing the test chime".into());
                            }
                            SpeakerState::Streaming
                        }
                        SlotState::Connecting => SpeakerState::Connecting,
                        SlotState::Waiting => {
                            let why = slot.last_error.clone().unwrap_or_else(|| "not connected".into());
                            st.detail = Some(match slot.retry_in {
                                Some(d) => format!("{why} (retry in {} s)", d.as_secs() + 1),
                                None => why,
                            });
                            SpeakerState::Retrying
                        }
                    }
                } else if !s.enabled {
                    SpeakerState::Disabled
                } else if st.detail.is_some() && self.running.is_some() {
                    SpeakerState::Error
                } else if self.state == EngineState::Starting {
                    SpeakerState::Connecting
                } else if st.detail.is_some() && self.want {
                    SpeakerState::Error
                } else {
                    SpeakerState::Idle
                };
                st
            })
            .collect();
        Status {
            engine: self.state.clone(),
            wanted: self.want,
            latency_ms: self.running.as_ref().map(|r| r.engine.latency_frames() * 1000 / 44100),
            speakers,
            input: self.running.as_ref().map(|r| r.input.describe()),
            windows_volume: self.deps.volume.as_ref().and_then(|v| v.windows_volume()),
            follow_windows_volume: self.cfg.follow_windows_volume,
        }
    }

    fn publish(&self) {
        let st = self.status();
        let changed = {
            let mut cur = self.shared.status.lock().unwrap();
            let changed = *cur != st;
            *cur = st;
            changed
        };
        if changed && let Some(w) = self.shared.waker.lock().unwrap().as_ref() {
            w();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::ChannelSource;

    fn deps(path: Option<PathBuf>) -> ControllerDeps {
        ControllerDeps {
            open_input: Box::new(|| {
                Ok((Box::new(ToneSource::new(1.0, 0.1)) as Box<dyn Source>, InputHandle::new(|| "test tone".into(), ())))
            }),
            credentials: Box::new(|_, _| Ok(Credentials::Transient)),
            config_path: path,
            volume: None,
            save_debounce: Duration::from_millis(300),
        }
    }

    fn wait_for(ctrl: &Controller, what: &str, f: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = ctrl.status();
            if f(&st) {
                return st;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {st:#?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn live_dsp_changes_apply_immediately_and_settings_persist_debounced() {
        let dir = crate::testvec::temp_dir("ctrl");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        for s in cfg.speakers.iter_mut() {
            s.enabled = false; // nothing here may touch the network
        }
        let mut ctrl = Controller::new(cfg.clone(), deps(Some(path.clone()))).unwrap();
        let key = cfg.speakers[1].key();
        assert_eq!(ctrl.live_dsp(&key).unwrap().get().source, ChannelSource::Right);

        // DSP edits are visible to the sender path at once, before the supervisor runs
        let mut edited = cfg.clone();
        edited.speakers[1].gain_db = -4.5;
        edited.speakers[1].delay_ms = 33.3;
        edited.speakers[1].role = ChannelSource::Mono;
        ctrl.update(edited.clone());
        let live = ctrl.live_dsp(&key).unwrap().get();
        assert_eq!((live.source, live.gain_db, live.delay_ms), (ChannelSource::Mono, -4.5, 33.3));
        assert_eq!(ctrl.config(), edited);

        // a burst of edits is written once, after the debounce, with the last values
        edited.latency_ms = 400;
        ctrl.update(edited.clone());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!path.exists(), "not written before the debounce");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(Instant::now() < deadline, "settings never saved");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(Config::load_from(&path).unwrap(), edited);

        // a last edit right before shutdown is flushed by shutdown
        edited.auto_start = false;
        ctrl.update(edited.clone());
        assert!(ctrl.shutdown(Duration::from_secs(5)));
        assert_eq!(Config::load_from(&path).unwrap(), edited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn start_without_enabled_speakers_reports_error_and_stop_clears_it() {
        let mut cfg = Config::default();
        for s in cfg.speakers.iter_mut() {
            s.enabled = false;
        }
        let ctrl = Controller::new(cfg, deps(None)).unwrap();
        let st = ctrl.status();
        assert_eq!(st.engine, EngineState::Stopped);
        assert_eq!(st.health(), Health::Off);
        assert!(st.speakers.iter().all(|s| s.state == SpeakerState::Disabled));
        ctrl.start();
        let st = wait_for(&ctrl, "error", |s| matches!(s.engine, EngineState::Error(_)));
        assert!(st.wanted);
        assert_eq!(st.health(), Health::Error);
        assert_eq!(st.summary(), "Error: no speakers enabled");
        ctrl.stop();
        let st = wait_for(&ctrl, "stopped", |s| s.engine == EngineState::Stopped);
        assert!(!st.wanted);
    }

    #[test]
    fn health_summarises_speaker_states() {
        let sp = |state, enabled| SpeakerStatus {
            key: String::new(),
            name: String::new(),
            ip: String::new(),
            enabled,
            state,
            detail: None,
            packets: 0,
            retransmitted: 0,
            send_errors: 0,
        };
        let mut st = Status { engine: EngineState::Running, ..Default::default() };
        st.speakers = vec![sp(SpeakerState::Streaming, true), sp(SpeakerState::Disabled, false)];
        assert_eq!(st.health(), Health::Streaming);
        assert_eq!(st.summary(), "Streaming to 1/1 speakers");
        st.speakers.push(sp(SpeakerState::Connecting, true));
        assert_eq!(st.health(), Health::Connecting);
        st.speakers = vec![sp(SpeakerState::Retrying, true)];
        assert_eq!(st.health(), Health::Error);
    }
}
