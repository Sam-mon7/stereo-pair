//! AirPlay 2 event channel (pyatv `protocols/airplay/channels.py` EventChannel): a
//! HAP-encrypted TCP connection to the receiver's `eventPort`. The receiver sends
//! RTSP-style requests on it; we answer each with 200 OK. If it closes, the session
//! is considered dead.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::hap::session::HapSession;
use crate::net::http::{format_response, header, parse_request};

pub const EVENTS_SALT: &str = "Events-Salt";
pub const EVENTS_WRITE_INFO: &str = "Events-Write-Encryption-Key";
pub const EVENTS_READ_INFO: &str = "Events-Read-Encryption-Key";

pub struct EventChannel {
    stream: TcpStream,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EventChannel {
    /// Connect (with pyatv's 5 x 1 s retry) and start answering requests.
    /// `out_key`/`in_key`: pyatv passes (salt, EVENTS_READ_INFO, EVENTS_WRITE_INFO) as
    /// (salt, output_info, input_info), i.e. we *write* with the "Read" key.
    pub fn connect(label: &str, addr: SocketAddr, out_key: [u8; 32], in_key: [u8; 32], dead: Arc<AtomicBool>) -> Result<Self> {
        let mut tries = 5;
        let stream = loop {
            match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                Ok(s) => break s,
                Err(e) if tries > 1 => {
                    tries -= 1;
                    debug!("[{label}] event channel connect failed ({e}), retrying");
                    std::thread::sleep(Duration::from_secs(1));
                }
                Err(e) => return Err(e).with_context(|| format!("event channel connect to {addr}")),
            }
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut reader = stream.try_clone()?;
        let (stop2, label2) = (stop.clone(), label.to_string());
        let handle = std::thread::Builder::new().name(format!("events-{label}")).spawn(move || {
            let mut session = HapSession::new(&out_key, &in_key);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                let n = match reader.read(&mut chunk) {
                    Ok(0) => {
                        if !stop2.load(Ordering::Relaxed) {
                            warn!("[{label2}] event channel closed by speaker");
                            dead.store(true, Ordering::Relaxed);
                        }
                        break;
                    }
                    Ok(n) => n,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                    Err(e) => {
                        if !stop2.load(Ordering::Relaxed) {
                            warn!("[{label2}] event channel error: {e}");
                            dead.store(true, Ordering::Relaxed);
                        }
                        break;
                    }
                };
                match session.decrypt(&chunk[..n]) {
                    Ok(p) => buf.extend_from_slice(&p),
                    Err(e) => {
                        warn!("[{label2}] event channel decrypt failed: {e}");
                        continue;
                    }
                }
                loop {
                    let (req, used) = match parse_request(&buf) {
                        Ok(Some(x)) => x,
                        Ok(None) => break,
                        Err(e) => {
                            warn!("[{label2}] bad event message ({e}); dropping buffer");
                            buf.clear();
                            break;
                        }
                    };
                    buf.drain(..used);
                    let body = plist::Value::from_reader(std::io::Cursor::new(&req.body))
                        .map(|v| format!("{v:?}"))
                        .unwrap_or_else(|_| format!("{} bytes", req.body.len()));
                    debug!("[{label2}] event: {} {} {body}", req.method, req.path);
                    let mut headers = vec![
                        ("Content-Length".to_string(), "0".to_string()),
                        ("Audio-Latency".to_string(), "0".to_string()),
                    ];
                    if let Some(s) = header(&req.headers, "Server") {
                        headers.push(("Server".into(), s.into()));
                    }
                    if let Some(c) = header(&req.headers, "CSeq") {
                        headers.push(("CSeq".into(), c.into()));
                    }
                    let version = req.protocol.clone();
                    let resp = format_response(&version, 200, "OK", &headers, b"");
                    if let Err(e) = (&reader).write_all(&session.encrypt(&resp)) {
                        warn!("[{label2}] event reply failed: {e}");
                    }
                }
            }
        })?;
        debug!("[{label}] event channel connected to {addr}");
        Ok(Self { stream, stop, handle: Some(handle) })
    }
}

impl EventChannel {
    /// Stop treating a remote close as a failure (call before TEARDOWN).
    pub fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
