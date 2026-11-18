//! "Add / pair speaker": browse AirPlay receivers over mDNS, then authenticate with
//! the one picked. Transient pairing first (HomePods); if the device refuses or
//! advertises that it needs one-time pairing, normal HAP PIN pairing
//! (`/pair-pin-start` with the identity headers, PIN typed into the panel) and the
//! credentials are saved to credentials.toml (other entries are kept).

use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{info, warn};

use crate::airplay::discovery::{self, Receiver as AirPlayReceiver};
use crate::airplay::session::AIRPLAY_PORT;
use crate::config::{CredentialStore, StoredCredentials};
use crate::hap;
use crate::net::http::HttpConnection;

const PIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Scanning,
    /// Scan finished; pick from `found`.
    Found,
    /// Working; text says on what.
    Busy(String),
    /// The speaker shows a PIN; waiting for the user.
    NeedPin,
    Done(Paired),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Paired {
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    /// "transient" or "hap"
    pub kind: &'static str,
}

#[derive(Default)]
struct State {
    phase: Option<Phase>,
    found: Vec<AirPlayReceiver>,
}

type Repaint = Arc<dyn Fn() + Send + Sync>;

pub struct PairFlow {
    state: Arc<Mutex<State>>,
    pin_tx: Option<Sender<Option<String>>>,
    repaint: Repaint,
}

impl PairFlow {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self { state: Arc::new(Mutex::new(State::default())), pin_tx: None, repaint: Arc::new(repaint) }
    }

    pub fn phase(&self) -> Phase {
        self.state.lock().unwrap().phase.clone().unwrap_or(Phase::Idle)
    }

    pub fn found(&self) -> Vec<AirPlayReceiver> {
        self.state.lock().unwrap().found.clone()
    }

    fn set(state: &Mutex<State>, repaint: &Repaint, phase: Phase) {
        state.lock().unwrap().phase = Some(phase);
        repaint();
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase(), Phase::Scanning | Phase::Busy(_) | Phase::NeedPin)
    }

    pub fn scan(&mut self) {
        if self.busy() {
            return;
        }
        let (state, repaint) = (self.state.clone(), self.repaint.clone());
        Self::set(&state, &repaint, Phase::Scanning);
        let spawned = std::thread::Builder::new().name("pair-scan".into()).spawn(move || {
            match discovery::scan(Duration::from_secs(4)) {
                Ok(found) => {
                    info!("pairing scan: {} AirPlay receiver(s)", found.len());
                    state.lock().unwrap().found = found;
                    Self::set(&state, &repaint, Phase::Found);
                }
                Err(e) => Self::set(&state, &repaint, Phase::Failed(format!("scan failed: {e:#}"))),
            }
        });
        if let Err(e) = spawned {
            Self::set(&self.state, &self.repaint, Phase::Failed(format!("{e}")));
        }
    }

    /// Authenticate with `r` (transient, else PIN). `client_name` is what the speaker
    /// shows for this PC.
    pub fn pair(&mut self, r: &AirPlayReceiver, client_name: &str) {
        if self.busy() {
            return;
        }
        let Some(ip) = r.ip else {
            Self::set(&self.state, &self.repaint, Phase::Failed(format!("{} has no IPv4 address", r.name)));
            return;
        };
        let (pin_tx, pin_rx) = channel();
        self.pin_tx = Some(pin_tx);
        let job = Job {
            name: r.name.clone(),
            ip,
            port: r.airplay_port.unwrap_or(AIRPLAY_PORT),
            pin_first: r.status_flags() & 0x200 != 0,
            client_name: client_name.to_string(),
        };
        let (state, repaint) = (self.state.clone(), self.repaint.clone());
        Self::set(&state, &repaint, Phase::Busy(format!("Connecting to {}...", job.name)));
        let spawned = std::thread::Builder::new().name("pair".into()).spawn(move || {
            let phase = match job.run(&state, &repaint, pin_rx) {
                Ok(p) => {
                    info!("paired with {} ({}) using {} pairing", p.name, p.ip, p.kind);
                    Phase::Done(p)
                }
                Err(e) => {
                    warn!("pairing with {} failed: {e:#}", job.name);
                    Phase::Failed(format!("{e:#}"))
                }
            };
            Self::set(&state, &repaint, phase);
        });
        if let Err(e) = spawned {
            Self::set(&self.state, &self.repaint, Phase::Failed(format!("{e}")));
        }
    }

    /// The PIN as typed (kept as a string: it may start with 0).
    pub fn submit_pin(&mut self, pin: &str) {
        if let Some(tx) = &self.pin_tx {
            let _ = tx.send(Some(pin.trim().to_string()));
        }
    }

    pub fn cancel(&mut self) {
        if let Some(tx) = self.pin_tx.take() {
            let _ = tx.send(None);
        }
    }

    /// Back to the receiver list (after Done / Failed).
    pub fn reset(&mut self) {
        self.cancel();
        let has_list = !self.state.lock().unwrap().found.is_empty();
        Self::set(&self.state, &self.repaint, if has_list { Phase::Found } else { Phase::Idle });
    }
}

struct Job {
    name: String,
    ip: IpAddr,
    port: u16,
    /// TXT status flags say "one-time pairing required": skip the transient attempt.
    pin_first: bool,
    client_name: String,
}

impl Job {
    fn run(&self, state: &Mutex<State>, repaint: &Repaint, pin_rx: Receiver<Option<String>>) -> Result<Paired> {
        let addr = SocketAddr::new(self.ip, self.port);
        if !self.pin_first {
            PairFlow::set(state, repaint, Phase::Busy(format!("Trying transient pairing with {}...", self.name)));
            match try_transient(addr) {
                Ok(()) => return Ok(self.paired("transient")),
                Err(e) => info!("{}: transient pairing failed ({e:#}); trying PIN pairing", self.name),
            }
        }
        PairFlow::set(state, repaint, Phase::Busy(format!("Asking {} to show a PIN...", self.name)));
        let device_id = hap::new_apple_device_id();
        let mut conn = HttpConnection::connect(addr)?;
        let creds = hap::pair_setup(&mut conn, &self.client_name, &device_id, || {
            PairFlow::set(state, repaint, Phase::NeedPin);
            match pin_rx.recv_timeout(PIN_TIMEOUT) {
                Ok(Some(pin)) if !pin.is_empty() => {
                    PairFlow::set(state, repaint, Phase::Busy("Checking the PIN...".into()));
                    Ok(pin)
                }
                Ok(Some(_)) => bail!("empty PIN"),
                Ok(None) | Err(RecvTimeoutError::Disconnected) => bail!("pairing cancelled"),
                Err(RecvTimeoutError::Timeout) => bail!("no PIN entered within {} minutes", PIN_TIMEOUT.as_secs() / 60),
            }
        });
        conn.shutdown();
        let creds = creds?;
        // Re-read the store right before writing so concurrent edits are kept; a
        // store that does not parse is never overwritten.
        let mut store = CredentialStore::load().context("credentials.toml is unreadable; not overwriting it")?;
        store.upsert(StoredCredentials::from_hap(&self.ip.to_string(), &self.name, &creds));
        let path = store.save()?;
        info!("credentials for {} saved to {}", self.ip, path.display());
        Ok(self.paired("hap"))
    }

    fn paired(&self, kind: &'static str) -> Paired {
        Paired { name: self.name.clone(), ip: self.ip, port: self.port, kind }
    }
}

fn try_transient(addr: SocketAddr) -> Result<()> {
    let mut conn = HttpConnection::connect(addr)?;
    let r = hap::transient_verify(&mut conn).map(|_| ());
    conn.shutdown();
    r.map_err(|e| anyhow!("{e:#}"))
}
