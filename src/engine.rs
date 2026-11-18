//! Orchestration: one timing server, one timeline, one sender thread, N speaker
//! sessions. Speakers that fail or drop are retried and re-join the running timeline.
//!
//! [`Engine`] is one running stream (one timeline). It is driven by calling
//! [`Engine::tick`] every ~100 ms: the CLI does that in [`run`], the tray app's
//! controller from its supervisor thread. Speakers can be added and removed while it
//! runs; per-speaker DSP changes need no engine call at all (see [`crate::dsp::LiveDsp`]),
//! nor do the pair-wide spatial settings ([`crate::dsp::spatial::LiveSpatial`]).
//!
//! The sender loop per packet: `source.fill` -> spatial stage (once, on the shared
//! stereo input) -> for each speaker: its own DSP (routing, EQ, gains, delay, probes)
//! -> encrypt -> send. [`Engine::set_programme_mute`] ramps the programme (not the
//! calibration probes or the test chime) to silence, used while calibrating.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tracing::{debug, error, info, warn};

use crate::airplay::session::{AIRPLAY_PORT, SpeakerSession};
use crate::audio::Source;
use crate::dsp::spatial::{LiveSpatial, SpatialStage};
use crate::dsp::{DspSettings, LiveDsp, SpeakerDsp};
use crate::hap::Credentials;
use crate::raop::packets::FRAMES_PER_PACKET;
use crate::raop::stream::{SinkStats, StreamSink};
use crate::raop::timeline::Timeline;
use crate::raop::timing::clock;
use crate::raop::timing_server::TimingServer;

/// Sync packet roughly once a second (44100 / 352 = 125.3 packets).
const SYNC_EVERY: u64 = 125;
/// If the sender falls this many packets behind (e.g. machine stalled), skip ahead.
const MAX_BEHIND: u64 = 250;
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(1);
const STATS_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct SpeakerPlan {
    pub label: String,
    pub ip: IpAddr,
    pub port: u16,
    pub creds: Credentials,
    /// Live DSP settings shared with whoever controls this speaker.
    pub live: Arc<LiveDsp>,
}

impl SpeakerPlan {
    pub fn new(label: &str, ip: IpAddr, creds: Credentials, dsp: DspSettings) -> Self {
        Self { label: label.to_string(), ip, port: AIRPLAY_PORT, creds, live: LiveDsp::new(dsp) }
    }

    /// Use an existing live-settings handle (kept across engine restarts).
    pub fn with_live(mut self, live: Arc<LiveDsp>) -> Self {
        self.live = live;
        self
    }
}

pub struct EngineOptions {
    pub latency_frames: u32,
    pub device_volume_db: Option<f32>,
    pub timing_port: u16,
    /// Keep retrying speakers that fail/drop (run). If false, fail fast (tone).
    pub reconnect: bool,
    /// Stop after this long (tone). None = until `stop`.
    pub run_for: Option<Duration>,
    /// Linear global gain as f32 bits (Windows master volume follower).
    pub gain: Arc<AtomicU32>,
    /// Pair-wide spatial settings (live).
    pub spatial: Arc<LiveSpatial>,
}

pub type SlotId = u64;
type Sinks = Arc<Mutex<Vec<(SlotId, StreamSink)>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Connect/SETUP in progress.
    Connecting,
    /// Joined the timeline and receiving audio.
    Streaming,
    /// Not connected; the next attempt is due in `retry_in` (if reconnecting).
    Waiting,
}

#[derive(Clone, Debug)]
pub struct SlotStatus {
    pub id: SlotId,
    pub state: SlotState,
    pub retry_in: Option<Duration>,
    pub last_error: Option<String>,
    pub packets: u64,
    pub retransmitted: u64,
    pub send_errors: u64,
}

struct Slot {
    id: SlotId,
    plan: SpeakerPlan,
    session: Option<SpeakerSession>,
    stats: Option<Arc<SinkStats>>,
    next_retry: Instant,
    pending: Option<JoinHandle<Result<SpeakerSession>>>,
    last_error: Option<String>,
    ever_joined: bool,
}

/// Local interface address the OS would use to reach `ip` (UDP connect sends nothing).
fn local_ip_towards(ip: IpAddr, port: u16) -> Result<IpAddr> {
    let probe = std::net::UdpSocket::bind(std::net::SocketAddr::new(
        if ip.is_ipv4() { IpAddr::from([0, 0, 0, 0]) } else { IpAddr::from([0u16; 8]) },
        0,
    ))?;
    probe.connect((ip, port))?;
    Ok(probe.local_addr()?.ip())
}

fn connect(plan: &SpeakerPlan, timing_port: u16) -> Result<SpeakerSession> {
    SpeakerSession::connect(&plan.label, plan.ip, plan.port, &plan.creds, timing_port)
}

/// RECORD the session at the timeline's next packet and hand its sink to the sender.
fn join(slot: &mut Slot, session: SpeakerSession, timeline: &Timeline, sinks: &Sinks, device_volume_db: Option<f32>) -> Result<()> {
    let mut session = session;
    let dsp = SpeakerDsp::with_live(slot.plan.live.clone(), timeline.sample_rate);
    debug!("[{}] DSP {:?}, delay {} frames", slot.plan.label, slot.plan.live.get(), dsp.delay_frames());
    let sink = session.make_sink(dsp)?;
    let packet = timeline.next_packet();
    session.record(sink.seq, timeline.rtptime(packet))?;
    if let Some(db) = device_volume_db {
        session.set_volume(db)?;
    }
    session.start_feedback()?;
    slot.stats = Some(sink.stats.clone());
    sinks.lock().unwrap().push((slot.id, sink));
    slot.session = Some(session);
    slot.last_error = None;
    slot.ever_joined = true;
    Ok(())
}

pub struct Engine {
    timing: TimingServer,
    timeline: Arc<Timeline>,
    sinks: Sinks,
    slots: Vec<Slot>,
    next_id: SlotId,
    sender: Option<JoinHandle<()>>,
    sender_stop: Arc<AtomicBool>,
    programme_mute: Arc<AtomicBool>,
    reconnect: bool,
    device_volume_db: Option<f32>,
    last_check: Instant,
    last_stats: Instant,
}

impl Engine {
    /// Connect every plan in parallel, RECORD them at packet 0 of a fresh timeline and
    /// start the sender. Speakers that fail are retried by [`Engine::tick`] (when
    /// `opts.reconnect`); otherwise at least one must start.
    pub fn start(plans: Vec<SpeakerPlan>, source: Box<dyn Source>, opts: EngineOptions) -> Result<(Engine, Vec<SlotId>)> {
        if plans.is_empty() {
            bail!("no speakers to stream to");
        }
        let local_ip = local_ip_towards(plans[0].ip, plans[0].port)?;
        let timing = TimingServer::start(local_ip, opts.timing_port)?;
        let timeline = Arc::new(Timeline::new(opts.latency_frames));
        info!(
            "latency {} frames ({:.0} ms) shared by {} speaker(s)",
            opts.latency_frames,
            opts.latency_frames as f64 * 1000.0 / timeline.sample_rate as f64,
            plans.len()
        );

        // Connect everyone in parallel.
        let results: Vec<Result<SpeakerSession>> = std::thread::scope(|s| {
            let handles: Vec<_> = plans.iter().map(|p| s.spawn(|| connect(p, timing.port))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| bail!("connect thread panicked"))).collect()
        });

        let sinks: Sinks = Arc::new(Mutex::new(Vec::new()));
        let mut slots: Vec<Slot> = plans
            .into_iter()
            .enumerate()
            .map(|(i, plan)| Slot {
                id: i as SlotId,
                plan,
                session: None,
                stats: None,
                next_retry: Instant::now() + RETRY_AFTER,
                pending: None,
                last_error: None,
                ever_joined: false,
            })
            .collect();
        let mut connected = 0;
        for (idx, res) in results.into_iter().enumerate() {
            let slot = &mut slots[idx];
            match res {
                Ok(session) => match join(slot, session, &timeline, &sinks, opts.device_volume_db) {
                    Ok(()) => connected += 1,
                    Err(e) => {
                        error!("[{}] start failed: {e:#}", slot.plan.label);
                        slot.last_error = Some(format!("{e:#}"));
                    }
                },
                Err(e) => {
                    error!("[{}] connect failed: {e:#}", slot.plan.label);
                    slot.last_error = Some(format!("{e:#}"));
                }
            }
        }
        if connected == 0 && !opts.reconnect {
            bail!("no speaker could be started");
        }
        if connected < slots.len() {
            warn!("{connected}/{} speakers streaming; retrying the rest every {}s", slots.len(), RETRY_AFTER.as_secs());
        }

        timeline.start_at(clock().now());
        let sender_stop = Arc::new(AtomicBool::new(false));
        let programme_mute = Arc::new(AtomicBool::new(false));
        let sender = spawn_sender(
            source,
            sinks.clone(),
            timeline.clone(),
            Gains { global: opts.gain.clone(), mute: programme_mute.clone(), spatial: opts.spatial.clone() },
            sender_stop.clone(),
        )?;
        info!("streaming");
        let ids = slots.iter().map(|s| s.id).collect();
        let next_id = slots.len() as SlotId;
        Ok((
            Engine {
                timing,
                timeline,
                sinks,
                slots,
                next_id,
                sender: Some(sender),
                sender_stop,
                programme_mute,
                reconnect: opts.reconnect,
                device_volume_db: opts.device_volume_db,
                last_check: Instant::now(),
                last_stats: Instant::now(),
            },
            ids,
        ))
    }

    fn spawn_connect(&mut self, idx: usize) {
        let port = self.timing.port;
        let slot = &mut self.slots[idx];
        let plan = slot.plan.clone();
        info!("[{}] {}", plan.label, if slot.ever_joined { "reconnecting" } else { "connecting" });
        match std::thread::Builder::new().name(format!("connect-{}", plan.label)).spawn(move || connect(&plan, port)) {
            Ok(h) => slot.pending = Some(h),
            Err(e) => {
                slot.last_error = Some(format!("cannot spawn connect thread: {e}"));
                slot.next_retry = Instant::now() + RETRY_AFTER;
            }
        }
    }

    /// Housekeeping: finish background connects (joining the running timeline), drop
    /// dead speakers, schedule retries, log stats. Call every ~100 ms.
    pub fn tick(&mut self) {
        for idx in 0..self.slots.len() {
            if !self.slots[idx].pending.as_ref().is_some_and(|h| h.is_finished()) {
                continue;
            }
            let handle = self.slots[idx].pending.take().unwrap();
            let slot = &mut self.slots[idx];
            let res = handle.join().unwrap_or_else(|_| Err(anyhow!("connect thread panicked")));
            let res = res.and_then(|s| join(slot, s, &self.timeline, &self.sinks, self.device_volume_db));
            match res {
                Ok(()) => info!("[{}] joined the shared timeline at packet {}", slot.plan.label, self.timeline.next_packet()),
                Err(e) => {
                    warn!("[{}] connect failed: {e:#}", slot.plan.label);
                    slot.last_error = Some(format!("{e:#}"));
                    slot.next_retry = Instant::now() + RETRY_AFTER;
                }
            }
        }

        if self.last_check.elapsed() >= CHECK_EVERY {
            self.last_check = Instant::now();
            for idx in 0..self.slots.len() {
                let slot = &mut self.slots[idx];
                if slot.session.as_ref().is_some_and(|s| s.dead.load(Ordering::Relaxed)) {
                    warn!("[{}] lost; dropping it from the stream", slot.plan.label);
                    let id = slot.id;
                    self.sinks.lock().unwrap().retain(|(i, _)| *i != id);
                    if let Some(mut s) = slot.session.take() {
                        s.teardown();
                    }
                    slot.last_error = Some("connection lost".into());
                    slot.next_retry = Instant::now() + RETRY_AFTER;
                }
                if slot.session.is_none() && slot.pending.is_none() && self.reconnect && Instant::now() >= slot.next_retry {
                    self.spawn_connect(idx);
                }
            }
        }

        if self.last_stats.elapsed() >= STATS_EVERY {
            self.last_stats = Instant::now();
            for (_, s) in self.sinks.lock().unwrap().iter() {
                info!(
                    "[{}] {} packets sent, {} retransmitted, {} send errors",
                    s.label,
                    s.stats.packets.load(Ordering::Relaxed),
                    s.stats.retransmitted.load(Ordering::Relaxed),
                    s.stats.send_errors.load(Ordering::Relaxed)
                );
            }
        }
    }

    /// Add a speaker to the running stream; it connects in the background and joins
    /// the timeline at the next packet once set up.
    pub fn add_speaker(&mut self, plan: SpeakerPlan) -> SlotId {
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot {
            id,
            plan,
            session: None,
            stats: None,
            next_retry: Instant::now(),
            pending: None,
            last_error: None,
            ever_joined: false,
        });
        let idx = self.slots.len() - 1;
        self.spawn_connect(idx);
        id
    }

    /// Remove a speaker (TEARDOWN if connected). A connect still in flight is left to
    /// finish on its own; its session tears down when dropped.
    pub fn remove_speaker(&mut self, id: SlotId) {
        let Some(idx) = self.slots.iter().position(|s| s.id == id) else { return };
        let mut slot = self.slots.remove(idx);
        self.sinks.lock().unwrap().retain(|(i, _)| *i != id);
        if let Some(mut s) = slot.session.take() {
            s.teardown();
        }
        info!("[{}] removed from the stream", slot.plan.label);
    }

    /// The live DSP handle of a speaker (to play the test chime into it, etc.).
    pub fn live(&self, id: SlotId) -> Option<Arc<LiveDsp>> {
        self.slots.iter().find(|s| s.id == id).map(|s| s.plan.live.clone())
    }

    pub fn latency_frames(&self) -> u32 {
        self.timeline.latency
    }

    /// The shared timeline (to schedule calibration probes on it).
    pub fn timeline(&self) -> Arc<Timeline> {
        self.timeline.clone()
    }

    /// Ramp the programme audio to silence (true) or back (false) on every speaker,
    /// from the next packet; probes and the test chime are not affected.
    pub fn set_programme_mute(&self, mute: bool) {
        self.programme_mute.store(mute, Ordering::Relaxed);
    }

    pub fn status(&self) -> Vec<SlotStatus> {
        let now = Instant::now();
        self.slots
            .iter()
            .map(|s| {
                let state = if s.session.is_some() {
                    SlotState::Streaming
                } else if s.pending.is_some() {
                    SlotState::Connecting
                } else {
                    SlotState::Waiting
                };
                let stat = |f: fn(&SinkStats) -> u64| s.stats.as_deref().map(f).unwrap_or(0);
                SlotStatus {
                    id: s.id,
                    state,
                    retry_in: (state == SlotState::Waiting && self.reconnect).then(|| s.next_retry.saturating_duration_since(now)),
                    last_error: s.last_error.clone(),
                    packets: stat(|x| x.packets.load(Ordering::Relaxed)),
                    retransmitted: stat(|x| x.retransmitted.load(Ordering::Relaxed)),
                    send_errors: stat(|x| x.send_errors.load(Ordering::Relaxed)),
                }
            })
            .collect()
    }

    /// Stop the sender and TEARDOWN every speaker. Idempotent (also run on drop).
    pub fn shutdown(&mut self) {
        let Some(sender) = self.sender.take() else { return };
        info!("stopping");
        self.sender_stop.store(true, Ordering::Relaxed);
        let _ = sender.join();
        self.sinks.lock().unwrap().clear();
        for slot in self.slots.iter_mut() {
            if let Some(mut s) = slot.session.take() {
                s.teardown();
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Blocking run used by the CLI (`run`, `tone`) and tests.
pub fn run(plans: Vec<SpeakerPlan>, source: Box<dyn Source>, opts: EngineOptions, stop: Arc<AtomicBool>) -> Result<()> {
    let run_for = opts.run_for;
    let (mut engine, _) = Engine::start(plans, source, opts)?;
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if run_for.is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        engine.tick();
    }
    engine.shutdown();
    Ok(())
}

struct Gains {
    global: Arc<AtomicU32>,
    mute: Arc<AtomicBool>,
    spatial: Arc<LiveSpatial>,
}

fn spawn_sender(mut source: Box<dyn Source>, sinks: Sinks, timeline: Arc<Timeline>, gains: Gains, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new().name("rtp-sender".into()).spawn(move || {
        let mut spatial = SpatialStage::with_live(gains.spatial, timeline.sample_rate);
        // 1 ms timer resolution so sleeps between 8 ms packets are accurate.
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        let clk = clock();
        let mut frames = vec![[0.0f32; 2]; FRAMES_PER_PACKET];
        let mut n: u64 = 0;
        let mut finished_logged = false;
        while !stop.load(Ordering::Relaxed) {
            let due = timeline.packets_due(clk.now());
            if due > n + MAX_BEHIND {
                warn!("sender {} packets behind; skipping ahead", due - n);
                n = due - 1;
            }
            while n < due {
                source.fill(&mut frames);
                // pair stage: once, on the shared stereo input
                spatial.process(&mut frames);
                // each speaker ramps its global gain per packet, so muting is click-free
                let g = if gains.mute.load(Ordering::Relaxed) { 0.0 } else { f32::from_bits(gains.global.load(Ordering::Relaxed)) };
                {
                    let mut sinks = sinks.lock().unwrap();
                    for (_, sink) in sinks.iter_mut() {
                        if n.is_multiple_of(SYNC_EVERY) || sink.needs_sync() {
                            sink.send_sync(&timeline, n);
                        }
                        sink.send_audio(&timeline, n, &frames, g);
                    }
                }
                n += 1;
                timeline.set_next_packet(n);
            }
            if source.finished() && !finished_logged {
                debug!("source finished at packet {n}; sending silence");
                finished_logged = true;
            }
            let next = clk.instant_of(timeline.send_ntp(n));
            let now = Instant::now();
            if next > now {
                std::thread::sleep((next - now).min(Duration::from_millis(20)));
            }
        }
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
        debug!("sender stopped at packet {n}");
    })?)
}
