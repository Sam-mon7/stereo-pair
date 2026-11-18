//! Per-speaker data plane: encrypted RTP audio, sync packets on the control port, and
//! retransmissions from a backlog (pyatv `stream_client.py` + `airplayv2.send_audio_packet`).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, trace, warn};

use super::packets::{AUDIO_HEADER_LEN, audio_header, parse_retransmit_request, retransmit_reply, sync_packet};
use super::timeline::Timeline;
use crate::crypto::CounterCipher;
use crate::dsp::SpeakerDsp;

/// pyatv keeps 1000 packets; a power of two makes the slot math trivial.
const BACKLOG: usize = 1024;

pub struct Backlog {
    slots: Vec<Option<(u16, Vec<u8>)>>,
}

impl Default for Backlog {
    fn default() -> Self {
        Self { slots: (0..BACKLOG).map(|_| None).collect() }
    }
}

impl Backlog {
    pub fn put(&mut self, seq: u16, packet: &[u8]) {
        let slot = &mut self.slots[seq as usize % BACKLOG];
        match slot {
            Some((s, buf)) => {
                *s = seq;
                buf.clear();
                buf.extend_from_slice(packet);
            }
            None => *slot = Some((seq, packet.to_vec())),
        }
    }

    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        match &self.slots[seq as usize % BACKLOG] {
            Some((s, buf)) if *s == seq => Some(buf),
            _ => None,
        }
    }
}

/// Build one AirPlay 2 realtime audio packet: header || ChaCha20-Poly1305(pcm, aad =
/// header[4..12]) || tag || 8-byte nonce (the counter, little-endian).
pub fn build_audio_packet(cipher: &mut CounterCipher, header: &[u8; AUDIO_HEADER_LEN], pcm: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(header);
    out.extend_from_slice(pcm);
    let (tag, counter) = cipher.seal_in_place(&header[4..12], &mut out[AUDIO_HEADER_LEN..]);
    out.extend_from_slice(&tag);
    out.extend_from_slice(&counter.to_le_bytes());
}

#[derive(Default)]
pub struct SinkStats {
    pub packets: AtomicU64,
    pub retransmitted: AtomicU64,
    pub send_errors: AtomicU64,
}

pub struct StreamSink {
    pub label: String,
    audio_sock: UdpSocket,
    control_sock: Arc<UdpSocket>,
    control_dest: SocketAddr,
    cipher: CounterCipher,
    pub seq: u16,
    ssrc: u32,
    backlog: Arc<Mutex<Backlog>>,
    first_audio: bool,
    first_sync: bool,
    pub dsp: SpeakerDsp,
    pub stats: Arc<SinkStats>,
    pcm: Vec<u8>,
    packet: Vec<u8>,
    retransmit_stop: Arc<AtomicBool>,
    retransmit_thread: Option<JoinHandle<()>>,
}

impl StreamSink {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        label: &str,
        data_dest: SocketAddr,
        control_sock: Arc<UdpSocket>,
        control_dest: SocketAddr,
        shk: &[u8; 32],
        seq: u16,
        ssrc: u32,
        dsp: SpeakerDsp,
    ) -> Result<Self> {
        let audio_sock = UdpSocket::bind(SocketAddr::new(control_sock.local_addr()?.ip(), 0))?;
        audio_sock.connect(data_dest)?;
        let backlog = Arc::new(Mutex::new(Backlog::default()));
        let stats = Arc::new(SinkStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_retransmitter(label, control_sock.clone(), backlog.clone(), stats.clone(), stop.clone())?;
        debug!("[{label}] audio -> {data_dest}, control -> {control_dest}, seq {seq}, ssrc {ssrc:#010x}");
        Ok(Self {
            label: label.to_string(),
            audio_sock,
            control_sock,
            control_dest,
            cipher: CounterCipher::new(shk),
            seq,
            ssrc,
            backlog,
            first_audio: true,
            first_sync: true,
            dsp,
            stats,
            pcm: Vec::with_capacity(352 * 4),
            packet: Vec::with_capacity(1500),
            retransmit_stop: stop,
            retransmit_thread: Some(handle),
        })
    }

    /// True until the first sync packet went out (late joiners get one immediately).
    pub fn needs_sync(&self) -> bool {
        self.first_sync
    }

    pub fn send_sync(&mut self, timeline: &Timeline, packet: u64) {
        let (nl, sec, frac, now) = timeline.sync_fields(packet);
        let p = sync_packet(self.first_sync, nl, sec, frac, now);
        self.first_sync = false;
        trace!("[{}] sync rtp {now} @ {sec}.{frac:08x}", self.label);
        if let Err(e) = self.control_sock.send_to(&p, self.control_dest) {
            debug!("[{}] sync send failed: {e}", self.label);
        }
    }

    /// Run this speaker's DSP over the shared input frames and send packet `packet`.
    pub fn send_audio(&mut self, timeline: &Timeline, packet: u64, input: &[[f32; 2]], global_gain: f32) {
        self.dsp.process_to_be16(input, global_gain, &mut self.pcm);
        let header = audio_header(self.first_audio, self.seq, timeline.rtptime(packet), self.ssrc);
        self.first_audio = false;
        build_audio_packet(&mut self.cipher, &header, &self.pcm, &mut self.packet);
        if let Err(e) = self.audio_sock.send(&self.packet) {
            let n = self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
            if n < 5 || n.is_multiple_of(1000) {
                warn!("[{}] audio send failed: {e}", self.label);
            }
        }
        if let Ok(mut b) = self.backlog.lock() {
            b.put(self.seq, &self.packet);
        }
        self.stats.packets.fetch_add(1, Ordering::Relaxed);
        self.seq = self.seq.wrapping_add(1);
    }
}

impl Drop for StreamSink {
    fn drop(&mut self) {
        self.retransmit_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.retransmit_thread.take() {
            let _ = h.join();
        }
    }
}

fn spawn_retransmitter(
    label: &str,
    sock: Arc<UdpSocket>,
    backlog: Arc<Mutex<Backlog>>,
    stats: Arc<SinkStats>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let label = label.to_string();
    Ok(std::thread::Builder::new().name(format!("retransmit-{label}")).spawn(move || {
        let mut buf = [0u8; 2048];
        while !stop.load(Ordering::Relaxed) {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let Some((first, count)) = parse_retransmit_request(&buf[..n]) else {
                        debug!("[{label}] unhandled control data from {from}: {}", hex::encode(&buf[..n.min(32)]));
                        continue;
                    };
                    debug!("[{label}] retransmit request seq {first} x{count} from {from}");
                    let replies: Vec<Vec<u8>> = {
                        let b = backlog.lock().unwrap();
                        (0..count)
                            .filter_map(|i| {
                                let s = first.wrapping_add(i);
                                let p = b.get(s).map(retransmit_reply);
                                if p.is_none() {
                                    debug!("[{label}] packet {s} not in backlog");
                                }
                                p
                            })
                            .collect()
                    };
                    for r in replies {
                        let _ = sock.send_to(&r, from);
                        stats.retransmitted.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => {
                    debug!("[{label}] control recv error: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn audio_packets_match_pyatv_send_audio_packet() {
        let v = vectors();
        let ap = &v["audio_packets"];
        let shk = unhex(&ap["shk"]);
        let mut cipher = CounterCipher::new(&shk);
        let mut out = Vec::new();
        for (i, p) in ap["packets"].as_array().unwrap().iter().enumerate() {
            let header: [u8; 12] = unhex(&p["header"]).try_into().unwrap();
            assert_eq!(header, audio_header(i == 0, 1000 + i as u16, 66150 + 352 * i as u32, 0xDEADBEEF));
            build_audio_packet(&mut cipher, &header, &unhex(&p["audio"]), &mut out);
            assert_eq!(out, unhex(&p["packet"]), "packet {i}");
        }
    }

    #[test]
    fn backlog_wraps_and_checks_seq() {
        let mut b = Backlog::default();
        b.put(65535, b"a");
        b.put(0, b"b");
        assert_eq!(b.get(65535), Some(&b"a"[..]));
        assert_eq!(b.get(0), Some(&b"b"[..]));
        b.put(1024, b"c"); // same slot as 0
        assert_eq!(b.get(0), None);
        assert_eq!(b.get(1024), Some(&b"c"[..]));
    }
}
