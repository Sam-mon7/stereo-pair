//! The one NTP timing server every receiver queries (pyatv `TimingServer`).

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, trace, warn};

use super::packets::timing_response;
use super::timing::{clock, ntp_parts};

pub struct TimingServer {
    pub port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TimingServer {
    /// Bind on the LAN address used to reach the speakers (not 0.0.0.0).
    pub fn start(local_ip: IpAddr, port: u16) -> Result<Self> {
        let sock = UdpSocket::bind(SocketAddr::new(local_ip, port))?;
        sock.set_read_timeout(Some(Duration::from_millis(250)))?;
        let port = sock.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new().name("ntp-timing".into()).spawn(move || {
            let mut buf = [0u8; 128];
            let mut served = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        let (sec, frac) = ntp_parts(clock().now());
                        if let Some(resp) = timing_response(&buf[..n], sec, frac) {
                            if let Err(e) = sock.send_to(&resp, from) {
                                warn!("timing reply to {from} failed: {e}");
                            }
                            served += 1;
                            if served == 1 {
                                debug!("first timing request from {from}");
                            }
                            trace!("timing reply to {from}");
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    // Windows reports ICMP port unreachable as ConnectionReset on UDP.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                    Err(e) => {
                        warn!("timing server recv error: {e}");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            debug!("timing server stopped after {served} replies");
        })?;
        info!("NTP timing server on UDP {local_ip}:{port}");
        Ok(Self { port, stop, handle: Some(handle) })
    }
}

impl Drop for TimingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
