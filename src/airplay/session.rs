//! One speaker's AirPlay 2 control session: RTSP connection, pair-verify, the two
//! SETUPs, event channel, RECORD/FLUSH, feedback keep-alive, volume and TEARDOWN.
//! The audio data plane is a [`StreamSink`] built from this session.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rand::Rng;
use tracing::{debug, info, warn};

use super::event::{EVENTS_READ_INFO, EVENTS_SALT, EVENTS_WRITE_INFO, EventChannel};
use super::setup::{get_u64, session_body, stream_body};
use crate::dsp::SpeakerDsp;
use crate::hap::{Credentials, verify_connection};
use crate::raop::stream::StreamSink;
use crate::rtsp::RtspSession;

pub const AIRPLAY_PORT: u16 = 7000;
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);

pub struct SpeakerSession {
    pub label: String,
    pub ip: IpAddr,
    rtsp: Arc<Mutex<RtspSession>>,
    _event: EventChannel,
    pub data_port: u16,
    pub server_control_port: u16,
    pub shk: [u8; 32],
    pub control_sock: Arc<UdpSocket>,
    pub ssrc: u32,
    /// Set by the event channel / feedback thread when the speaker goes away.
    pub dead: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    feedback: Option<JoinHandle<()>>,
    torn_down: bool,
}

impl SpeakerSession {
    /// Connect, authenticate and SETUP. Nothing is played until `record`.
    pub fn connect(label: &str, ip: IpAddr, port: u16, creds: &Credentials, timing_port: u16) -> Result<Self> {
        let addr = SocketAddr::new(ip, port);
        let mut rtsp = RtspSession::connect(addr, label)?;

        // pyatv fetches /info first (unencrypted).
        let mut advertised_pk: Option<Vec<u8>> = None;
        match rtsp.get_info() {
            Ok(r) if r.is_ok() => {
                debug!("[{label}] /info: {} bytes", r.body.len());
                if let Ok(v) = r.plist() {
                    advertised_pk = v.as_dictionary().and_then(|d| d.get("pk")).and_then(|p| p.as_data()).map(<[u8]>::to_vec);
                    if let Some(iv) = v.as_dictionary().and_then(|d| d.get("initialVolume")).and_then(|x| x.as_real()) {
                        info!("[{label}] speaker volume {iv:.1} dB");
                    }
                }
            }
            Ok(r) => debug!("[{label}] /info -> {}", r.code),
            Err(e) => debug!("[{label}] /info failed: {e}"),
        }

        let verified = verify_connection(&mut rtsp.conn, creds, advertised_pk.as_deref())
            .with_context(|| format!("[{label}] authentication"))?;

        let session_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
        let resp = rtsp.setup_plist(&session_body(&session_uuid, timing_port))?.ensure_ok("SETUP (session)")?;
        let body = resp.plist()?;
        debug!("[{label}] SETUP session response: {body:?}");
        let event_port = get_u64(&body, "eventPort").unwrap_or(0) as u16;

        let dead = Arc::new(AtomicBool::new(false));
        let (ev_out, ev_in) = verified.keys(EVENTS_SALT, EVENTS_READ_INFO, EVENTS_WRITE_INFO);
        let event = EventChannel::connect(label, SocketAddr::new(ip, event_port), ev_out, ev_in, dead.clone())?;

        // pyatv: shk = first 32 bytes of HKDF(Events-Salt, Events-Write-Encryption-Key).
        let (shk, _) = verified.keys(EVENTS_SALT, EVENTS_WRITE_INFO, EVENTS_READ_INFO);

        let local_ip = rtsp.conn.local_ip;
        let control_sock = Arc::new(UdpSocket::bind(SocketAddr::new(local_ip, 0))?);
        let local_control = control_sock.local_addr()?.port();
        let resp = rtsp
            .setup_plist(&stream_body(local_control, &shk, rtsp.session_id))?
            .ensure_ok("SETUP (stream)")?;
        let body = resp.plist()?;
        debug!("[{label}] SETUP stream response: {body:?}");
        let stream = body
            .as_dictionary()
            .and_then(|d| d.get("streams"))
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("SETUP stream response has no streams"))?;
        let data_port = get_u64(stream, "dataPort").ok_or_else(|| anyhow!("no dataPort"))? as u16;
        let server_control_port = get_u64(stream, "controlPort").ok_or_else(|| anyhow!("no controlPort"))? as u16;
        for key in ["latencyMin", "latencyMax", "audioLatency", "outputLatencyMicros"] {
            if let Some(v) = get_u64(stream, key).or_else(|| get_u64(&body, key)) {
                info!("[{label}] receiver reports {key} = {v}");
            }
        }
        info!("[{label}] set up: data port {data_port}, control port {server_control_port}, event port {event_port}");

        let ssrc = rtsp.session_id;
        Ok(Self {
            label: label.to_string(),
            ip,
            rtsp: Arc::new(Mutex::new(rtsp)),
            _event: event,
            data_port,
            server_control_port,
            shk,
            control_sock,
            ssrc,
            dead,
            stop: Arc::new(AtomicBool::new(false)),
            feedback: None,
            torn_down: false,
        })
    }

    /// Build the data-plane sink with a random starting sequence number.
    pub fn make_sink(&self, dsp: SpeakerDsp) -> Result<StreamSink> {
        let seq: u16 = rand::thread_rng().r#gen();
        StreamSink::new(
            &self.label,
            SocketAddr::new(self.ip, self.data_port),
            self.control_sock.clone(),
            SocketAddr::new(self.ip, self.server_control_port),
            &self.shk,
            seq,
            self.ssrc,
            dsp,
        )
    }

    /// Start the 2 s `/feedback` keep-alive (best effort; 3 failures in a row mark the
    /// session dead).
    pub fn start_feedback(&mut self) -> Result<()> {
        let (rtsp, stop, dead, label) = (self.rtsp.clone(), self.stop.clone(), self.dead.clone(), self.label.clone());
        self.feedback = Some(std::thread::Builder::new().name(format!("feedback-{label}")).spawn(move || {
            let mut failures = 0;
            'outer: while !stop.load(Ordering::Relaxed) {
                let result = rtsp.lock().unwrap().feedback();
                match result {
                    Ok(r) if r.is_ok() => failures = 0,
                    Ok(r) => debug!("[{label}] feedback -> {}", r.code),
                    Err(e) => {
                        failures += 1;
                        debug!("[{label}] feedback failed: {e}");
                        if failures >= 3 {
                            warn!("[{label}] speaker stopped answering");
                            dead.store(true, Ordering::Relaxed);
                        }
                    }
                }
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    std::thread::sleep(FEEDBACK_INTERVAL / 20);
                }
            }
        })?);
        Ok(())
    }

    /// RECORD then FLUSH with the first packet's seq/rtptime (pyatv order).
    pub fn record(&self, seq: u16, rtptime: u32) -> Result<()> {
        let mut rtsp = self.rtsp.lock().unwrap();
        let rec = rtsp.request("RECORD", &[])?.ensure_ok("RECORD")?;
        if let Some(l) = rec.header("Audio-Latency") {
            info!("[{}] RECORD reports Audio-Latency {l}", self.label);
        }
        rtsp.request(
            "FLUSH",
            &[
                ("Range", "npt=0-".to_string()),
                ("Session", "0".to_string()),
                ("RTP-Info", format!("seq={seq};rtptime={rtptime}")),
            ],
        )?
        .ensure_ok("FLUSH")?;
        info!("[{}] RECORD ok (seq {seq}, rtptime {rtptime})", self.label);
        Ok(())
    }

    /// AirPlay volume in dB (-30..0, -144 = mute).
    pub fn set_volume(&self, db: f32) -> Result<()> {
        let resp = self.rtsp.lock().unwrap().set_parameter("volume", &format!("{db:.6}"))?;
        if !resp.is_ok() {
            warn!("[{}] SET_PARAMETER volume -> {}", self.label, resp.code);
        }
        Ok(())
    }

    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        self.stop.store(true, Ordering::Relaxed);
        self._event.close();
        if let Some(h) = self.feedback.take() {
            let _ = h.join();
        }
        let mut rtsp = self.rtsp.lock().unwrap();
        match rtsp.request("TEARDOWN", &[("Session", "0".to_string())]) {
            Ok(r) => info!("[{}] TEARDOWN -> {}", self.label, r.code),
            Err(e) => debug!("[{}] TEARDOWN failed: {e}", self.label),
        }
        rtsp.conn.shutdown();
    }
}

impl Drop for SpeakerSession {
    fn drop(&mut self) {
        self.teardown();
    }
}
