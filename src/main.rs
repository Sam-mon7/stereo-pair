//! stereo-pair: stream the PC's audio to two AirPlay 2 speakers as a stereo pair, from
//! one shared clock and one RTP timeline.
//!
//! One exe, two modes: with no arguments (or `tray`) it is a tray app with a control
//! panel; with a subcommand it is the console tool. The embedded manifest sets
//! `consoleAllocationPolicy=detached` (build.rs), so started from Explorer / the Run
//! key it gets no console window, while started from a terminal it shares that
//! terminal's console (output, stdin for `pair`, Ctrl+C all work as before).

mod app;

mod airplay;
mod audio;
mod config;
mod controller;
mod crypto;
mod dsp;
mod engine;
mod hap;
mod net;
mod raop;
mod rtsp;
#[cfg(test)]
mod testvec;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use airplay::discovery;
use airplay::session::AIRPLAY_PORT;
use config::{Config, CredentialStore, StoredCredentials};
use dsp::{ChannelSource, DspSettings};
use engine::{EngineOptions, SpeakerPlan};

#[derive(Parser)]
#[command(name = "stereo-pair", version, about = "Stream PC audio to two AirPlay 2 speakers as one synchronised stereo pair")]
struct Cli {
    /// -v: debug (incl. RTSP exchanges), -vv: trace
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// No subcommand: tray app (opens the panel; see `tray`)
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Tray app (default with no arguments). Second launches open the running one's panel.
    Tray {
        /// Do not start streaming on launch (also: env STEREO_PAIR_NO_AUTOSTART=1)
        #[arg(long)]
        no_autostart: bool,
        /// Open the control panel right away
        #[arg(long)]
        panel: bool,
    },
    /// Ask the running tray app to quit cleanly (TEARDOWN on every speaker)
    Quit,
    /// List AirPlay receivers on the LAN (mDNS)
    Scan {
        #[arg(long, default_value_t = 4.0)]
        seconds: f64,
    },
    /// Show a receiver's GET /info (read-only)
    Info {
        ip: IpAddr,
        #[arg(long, default_value_t = AIRPLAY_PORT)]
        port: u16,
    },
    /// One-time HAP PIN pairing (PIN read from stdin), saved to credentials.toml
    Pair {
        ip: IpAddr,
        /// Name the speaker shows for this sender
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = AIRPLAY_PORT)]
        port: u16,
    },
    /// Import pyatv credentials (ltpk:ltsk:atv_id:client_id) for a speaker
    ImportPyatvCreds {
        ip: IpAddr,
        file: PathBuf,
        /// X-Apple-Device-ID that was used when pairing
        #[arg(long, default_value = config::SPIKE_APPLE_DEVICE_ID)]
        apple_device_id: String,
        /// Speaker label stored alongside
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Play a quiet chime on the given speakers (same shared-clock path as `run`)
    Tone {
        #[arg(required = true)]
        ips: Vec<IpAddr>,
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        /// Peak level 0..1 (default is quiet)
        #[arg(long, default_value_t = 0.15)]
        amplitude: f32,
        #[arg(long)]
        latency_ms: Option<u32>,
        /// First note on the left channel only (-> first IP), second note on the right
        /// only (-> second IP): checks routing. Default plays both notes everywhere.
        #[arg(long)]
        split: bool,
    },
    /// Stream the default Windows output to the configured speakers (Ctrl+C stops)
    Run {
        #[arg(long)]
        latency_ms: Option<u32>,
    },
    /// Show config/credential paths (writes the default config if missing)
    Config,
    /// Capture-only check: open WASAPI loopback + volume follower, print levels (no network, no sound)
    CaptureTest {
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
    },
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "info,stereo_pair=debug,rtsp=debug",
        _ => "debug,stereo_pair=trace,rtsp=trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into());
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(verbose > 0)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

fn ctrl_c_flag() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || {
        if s.swap(true, Ordering::SeqCst) {
            eprintln!("second Ctrl+C: exiting without TEARDOWN");
            std::process::exit(130);
        }
        eprintln!("stopping... (Ctrl+C again to force)");
    })?;
    Ok(stop)
}

fn main() {
    let cli = Cli::parse();
    let tray = match &cli.cmd {
        None => Some(app::TrayOptions { no_autostart: false, open_panel: true }),
        Some(Cmd::Tray { no_autostart, panel }) => Some(app::TrayOptions { no_autostart: *no_autostart, open_panel: *panel }),
        _ => None,
    };
    if let Some(opts) = tray {
        if let Err(e) = app::run(opts) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    init_logging(cli.verbose);
    if let Err(e) = real_main(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.cmd else { return Ok(()) };
    match cmd {
        Cmd::Tray { .. } => Ok(()),
        Cmd::Quit => {
            if app::signal_quit()? {
                println!("asked the running tray app to quit");
            } else {
                println!("no tray app is running");
            }
            Ok(())
        }
        Cmd::Scan { seconds } => cmd_scan(seconds),
        Cmd::Info { ip, port } => {
            let v = discovery::get_info(ip, port)?;
            let mut s = String::new();
            discovery::format_plist(&v, 0, &mut s);
            print!("{s}");
            Ok(())
        }
        Cmd::Pair { ip, name, port } => cmd_pair(ip, name, port),
        Cmd::ImportPyatvCreds { ip, file, apple_device_id, label } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            let c = config::parse_pyatv_credentials(&text, &apple_device_id, config::DEFAULT_CLIENT_NAME)?;
            let mut store = CredentialStore::load()?;
            store.upsert(StoredCredentials::from_hap(&ip.to_string(), &label, &c));
            let path = store.save()?;
            println!(
                "imported credentials for {ip} (device id {}, client id {}) into {}",
                String::from_utf8_lossy(&c.atv_id),
                String::from_utf8_lossy(&c.client_id),
                path.display()
            );
            Ok(())
        }
        Cmd::Tone { ips, seconds, amplitude, latency_ms, split } => cmd_tone(ips, seconds, amplitude, latency_ms, split),
        Cmd::Run { latency_ms } => cmd_run(latency_ms),
        Cmd::CaptureTest { seconds } => cmd_capture_test(seconds),
        Cmd::Config => {
            let (cfg, path, created) = Config::load_or_create()?;
            println!("config:      {}{}", path.display(), if created { " (created with defaults)" } else { "" });
            println!("credentials: {}", config::credentials_path()?.display());
            println!("latency {} ms, follow Windows volume: {}", cfg.latency_ms, cfg.follow_windows_volume);
            let store = CredentialStore::load()?;
            for s in &cfg.speakers {
                let ip = s.ip.clone().unwrap_or_else(|| "(mDNS)".into());
                let auth = s.ip.as_deref().and_then(|ip| store.find(ip)).map(|c| c.kind.as_str()).unwrap_or("transient (default)");
                println!("  {:<20} {:<16} role={:?} gain={} dB delay={} ms auth={auth}", s.name, ip, s.role, s.gain_db, s.delay_ms);
            }
            Ok(())
        }
    }
}

fn cmd_scan(seconds: f64) -> Result<()> {
    eprintln!("browsing mDNS for {seconds} s...");
    let found = discovery::scan(Duration::from_secs_f64(seconds))?;
    if found.is_empty() {
        println!("no AirPlay receivers found (mDNS replies may be blocked by the firewall; try `info <ip>`)");
        return Ok(());
    }
    let store = CredentialStore::load().unwrap_or_default();
    for r in found {
        let ip = r.ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into());
        println!("{}", r.name);
        println!("  ip {ip}  airplay port {}  model {}", r.airplay_port.map(|p| p.to_string()).unwrap_or("-".into()), r.model());
        for key in ["deviceid", "pi", "psi", "srcvers", "features", "flags", "pk"] {
            if let Some(v) = r.txt.get(key) {
                println!("  {key} = {v}");
            }
        }
        let hints = r.pairing_hints();
        if !hints.is_empty() {
            println!("  advertises: {}", hints.join(", "));
        }
        let auth = match r.ip.and_then(|i| store.find(&i.to_string())) {
            Some(c) => format!("stored {} credentials", c.kind),
            None if r.status_flags() & 0x200 != 0 => "no stored credentials; one-time pairing required -> run `pair <ip>` (or import-pyatv-creds)".into(),
            None => "no stored credentials -> transient pairing (if that gets HTTP 470, run `pair <ip>`)".into(),
        };
        println!("  pairing: {auth}");
    }
    Ok(())
}

fn cmd_pair(ip: IpAddr, name: Option<String>, port: u16) -> Result<()> {
    let (cfg, _, _) = Config::load_or_create()?;
    let client_name = name.unwrap_or(cfg.client_name);
    let device_id = hap::new_apple_device_id();
    let mut conn = net::http::HttpConnection::connect(SocketAddr::new(ip, port))?;
    println!("Starting pairing with {ip} as \"{client_name}\" ({device_id}); the speaker should show a PIN.");
    let creds = hap::pair_setup(&mut conn, &client_name, &device_id, hap::read_pin_from_stdin)?;
    conn.shutdown();
    let label = cfg
        .speakers
        .iter()
        .find(|s| s.ip.as_deref() == Some(&ip.to_string()))
        .map(|s| s.name.clone())
        .unwrap_or_default();
    let mut store = CredentialStore::load()?;
    store.upsert(StoredCredentials::from_hap(&ip.to_string(), &label, &creds));
    let path = store.save()?;
    println!("paired; credentials saved to {}", path.display());
    Ok(())
}

fn cmd_tone(ips: Vec<IpAddr>, seconds: f64, amplitude: f32, latency_ms: Option<u32>, split: bool) -> Result<()> {
    let (mut cfg, _, _) = Config::load_or_create()?;
    if let Some(l) = latency_ms {
        cfg.latency_ms = l;
    }
    let store = CredentialStore::load()?;
    let mut plans = Vec::new();
    for (i, ip) in ips.iter().enumerate() {
        let source = match (split, i) {
            (true, 0) => ChannelSource::Left,
            (true, 1) => ChannelSource::Right,
            _ => ChannelSource::Mono,
        };
        plans.push(SpeakerPlan::new(
            &ip.to_string(),
            *ip,
            store.credentials_for(&ip.to_string())?,
            DspSettings { source, ..Default::default() },
        ));
    }
    let latency = cfg.latency_frames();
    let source: Box<dyn audio::Source> =
        Box::new(audio::tone::ToneSource::new(seconds, amplitude.clamp(0.0, 1.0)).split(split));
    let opts = EngineOptions {
        latency_frames: latency,
        device_volume_db: cfg.device_volume_db,
        timing_port: cfg.timing_port,
        reconnect: false,
        run_for: Some(Duration::from_secs_f64(seconds + latency as f64 / 44100.0 + 0.75)),
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
    };
    let stop = ctrl_c_flag()?;
    engine::run(plans, source, opts, stop)
}

fn cmd_capture_test(seconds: f64) -> Result<()> {
    use audio::Source;
    let capture = audio::capture::Capture::start()?;
    let follower = audio::volume::VolumeFollower::start()?;
    let gain = follower.handle();
    let mut source = audio::LoopbackSource::new(capture.shared.clone());
    let mut frames = vec![[0.0f32; 2]; raop::packets::FRAMES_PER_PACKET];
    let start = std::time::Instant::now();
    let mut next = start;
    let (mut peak, mut packets, mut nonzero) = (0.0f32, 0u64, 0u64);
    let mut last_print = start;
    while start.elapsed().as_secs_f64() < seconds {
        source.fill(&mut frames);
        packets += 1;
        let p = frames.iter().fold(0.0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
        if p > 0.0 {
            nonzero += 1;
        }
        peak = peak.max(p);
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = std::time::Instant::now();
            let g = f32::from_bits(gain.load(Ordering::Relaxed));
            println!(
                "{:>5.1}s  packets {packets:>5}  with audio {nonzero:>5}  peak {:>6.1} dBFS  Windows-volume gain {:>6.1} dB",
                start.elapsed().as_secs_f64(),
                20.0 * peak.max(1e-9).log10(),
                20.0 * g.max(1e-9).log10()
            );
            peak = 0.0;
        }
        next += Duration::from_secs_f64(352.0 / 44100.0);
        if let Some(d) = next.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(d);
        }
    }
    let dev = capture.shared.lock().unwrap().device.clone();
    println!("device \"{dev}\": {packets} packets pulled at 44.1 kHz, {nonzero} contained audio");
    Ok(())
}

fn cmd_run(latency_ms: Option<u32>) -> Result<()> {
    let (mut cfg, path, created) = Config::load_or_create()?;
    if created {
        warn!("wrote default config to {} - check speaker names/roles", path.display());
    }
    if let Some(l) = latency_ms {
        cfg.latency_ms = l;
    }
    let store = CredentialStore::load()?;
    let mut plans = Vec::new();
    for s in cfg.speakers.iter().filter(|s| s.enabled) {
        let ip: IpAddr = match &s.ip {
            Some(ip) => ip.parse().with_context(|| format!("bad ip {ip:?} for {}", s.name))?,
            None => discovery::resolve_name(&s.name, Duration::from_secs(4))?
                .ok_or_else(|| anyhow!("speaker \"{}\" not found via mDNS; set ip in {}", s.name, path.display()))?,
        };
        let creds = store.credentials_for(&ip.to_string())?;
        info!("{} ({ip}): role {:?}, gain {} dB, delay {} ms", s.name, s.role, s.gain_db, s.delay_ms);
        let mut plan = SpeakerPlan::new(&s.name, ip, creds, s.dsp());
        if let Some(port) = s.port {
            plan.port = port;
        }
        plans.push(plan);
    }

    let capture = audio::capture::Capture::start()?;
    let source = Box::new(audio::LoopbackSource::new(capture.shared.clone()));
    let follower = if cfg.follow_windows_volume { Some(audio::volume::VolumeFollower::start()?) } else { None };
    let gain = follower.as_ref().map(|f| f.handle()).unwrap_or_else(|| Arc::new(AtomicU32::new(1.0f32.to_bits())));
    let opts = EngineOptions {
        latency_frames: cfg.latency_frames(),
        device_volume_db: cfg.device_volume_db,
        timing_port: cfg.timing_port,
        reconnect: true,
        run_for: None,
        gain,
    };
    let stop = ctrl_c_flag()?;
    let res = engine::run(plans, source, opts, stop);
    drop(follower);
    drop(capture);
    res
}
