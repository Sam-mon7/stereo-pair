//! Tray mode: notification-area icon + on-demand egui panel around the in-process
//! engine ([`crate::controller::Controller`]).
//!
//! Threads: the main thread owns the tray icon (its hidden window needs this thread's
//! message loop) and runs either our own message pump or, while the panel is open,
//! eframe's event loop (`run_and_return`, so closing the panel returns here and the
//! app stays in the tray). The engine, volume follower, capture and pairing run on
//! their own threads.

mod panel;
mod pairing;
mod tray;
mod win;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use eframe::egui;
use tracing::{error, info, warn};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::config::{self, Config};
use crate::controller::{Controller, ControllerDeps};
use pairing::PairFlow;
use tray::{Tray, TrayAction};

pub struct TrayOptions {
    /// Do not start streaming on launch, whatever `auto_start` in config.toml says.
    pub no_autostart: bool,
    /// Open the panel right away.
    pub open_panel: bool,
}

enum AppEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    ShowPanel,
    Quit,
}

type CtxSlot = Arc<Mutex<Option<egui::Context>>>;

fn repaint(slot: &CtxSlot) {
    if let Some(ctx) = slot.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}

pub struct TrayApp {
    pub controller: Controller,
    tray: Tray,
    rx: Receiver<AppEvent>,
    pub ctx_slot: CtxSlot,
    pub quit: bool,
    /// Open (or, if open, raise) the panel.
    pub show_panel: bool,
    pub pair: PairFlow,
    pub autostart: bool,
    pub log_dir: PathBuf,
}

impl TrayApp {
    /// Handle tray / menu / other-instance events and refresh the tray icon.
    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                AppEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. })
                | AppEvent::Tray(TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }) => self.show_panel = true,
                AppEvent::Tray(_) => {}
                AppEvent::Menu(m) => match self.tray.action(&m) {
                    Some(TrayAction::Toggle) => self.toggle_streaming(),
                    Some(TrayAction::OpenPanel) => self.show_panel = true,
                    Some(TrayAction::Autostart) => {
                        let want = !self.autostart;
                        self.set_autostart(want);
                    }
                    Some(TrayAction::Quit) => self.quit = true,
                    None => {}
                },
                AppEvent::ShowPanel => self.show_panel = true,
                AppEvent::Quit => self.quit = true,
            }
        }
        let st = self.controller.status();
        self.tray.update(&st);
    }

    pub fn toggle_streaming(&mut self) {
        if self.controller.status().wanted {
            info!("stop requested");
            self.controller.stop();
        } else {
            info!("start requested");
            self.controller.start();
        }
    }

    /// Write/remove the HKCU Run entry; returns the error text on failure.
    pub fn set_autostart(&mut self, on: bool) -> Option<String> {
        let err = match win::set_autostart(on) {
            Ok(()) => {
                info!("start with Windows: {}", if on { "on" } else { "off" });
                None
            }
            Err(e) => {
                warn!("cannot change start with Windows: {e:#}");
                Some(format!("{e:#}"))
            }
        };
        self.autostart = win::autostart_enabled();
        self.tray.set_autostart_checked(self.autostart);
        err
    }
}

const LOG_FILTER: &str = "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,eframe=warn,egui_wgpu=warn,egui_winit=warn,winit=warn";

pub fn log_dir() -> Result<PathBuf> {
    Ok(config::app_dir()?.join("logs"))
}

/// Daily-rolling log files in %APPDATA%\stereo-pair\logs (14 kept), mirrored to
/// stderr when started from a terminal.
fn init_logging(also_stderr: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("stereo-pair")
        .filename_suffix("log")
        .max_log_files(14)
        .build(&dir)
        .context("log file")?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| LOG_FILTER.into());
    let file = tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false).with_target(false);
    let stderr = also_stderr.then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false));
    tracing_subscriber::registry().with(filter).with(file).with(stderr).init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!("panic: {info}");
        default_hook(info);
    }));
    Ok(guard)
}

pub fn run(opts: TrayOptions) -> Result<()> {
    let console = win::console_kind();
    if console == win::ConsoleKind::Owned {
        // Only if Windows ignored the manifest's consoleAllocationPolicy.
        win::free_console();
    }
    let Some(_instance) = win::acquire_single_instance()? else {
        // Already running: ask it to show its panel.
        let shown = win::signal(win::SHOW_PANEL_EVENT)?;
        if console == win::ConsoleKind::Inherited {
            eprintln!("stereo-pair is already running in the tray{}", if shown { "; opened its panel" } else { "" });
        }
        return Ok(());
    };
    let _log = init_logging(console == win::ConsoleKind::Inherited)?;
    info!("stereo-pair {} tray app starting (pid {})", env!("CARGO_PKG_VERSION"), std::process::id());
    let res = run_tray(opts);
    if let Err(e) = &res {
        error!("fatal: {e:#}");
        win::error_box("Stereo Pair", &format!("Stereo Pair stopped:\n\n{e:#}"));
    }
    info!("tray app exited");
    res
}

fn run_tray(opts: TrayOptions) -> Result<()> {
    let path = config::config_path()?;
    let (cfg, created) = Config::load_or_create_at(&path)
        .with_context(|| format!("{} could not be read; fix or delete it", path.display()))?;
    if created {
        warn!("wrote default config to {}", path.display());
    }
    info!("config {} ({} speakers, latency {} ms)", path.display(), cfg.speakers.len(), cfg.latency_ms);

    let ctx_slot: CtxSlot = Arc::new(Mutex::new(None));
    let (tx, rx) = channel::<AppEvent>();
    let main_thread = win::current_thread_id();
    let forward = {
        let (tx, slot) = (tx.clone(), ctx_slot.clone());
        move |ev: AppEvent| {
            let _ = tx.send(ev);
            repaint(&slot);
            win::wake_thread(main_thread);
        }
    };
    let f = forward.clone();
    TrayIconEvent::set_event_handler(Some(move |e| f(AppEvent::Tray(e))));
    let f = forward.clone();
    MenuEvent::set_event_handler(Some(move |e| f(AppEvent::Menu(e))));
    let f = forward.clone();
    win::listen(win::SHOW_PANEL_EVENT, move || f(AppEvent::ShowPanel))?;
    let f = forward.clone();
    win::listen(win::QUIT_EVENT, move || f(AppEvent::Quit))?;
    let f = forward.clone();
    // Ctrl+C in the terminal we were started from: clean quit.
    if let Err(e) = ctrlc::set_handler(move || f(AppEvent::Quit)) {
        warn!("no Ctrl+C handler: {e}");
    }
    drop(tx);

    let controller = Controller::new(cfg.clone(), ControllerDeps::production(path, cfg.follow_windows_volume))?;
    let slot = ctx_slot.clone();
    controller.set_waker(move || repaint(&slot));
    let autostart = win::autostart_enabled();
    let tray = Tray::new(autostart)?;
    let slot = ctx_slot.clone();
    let pair = PairFlow::new(move || repaint(&slot));

    let no_autostart = opts.no_autostart || std::env::var_os("STEREO_PAIR_NO_AUTOSTART").is_some_and(|v| v != "0" && !v.is_empty());
    if cfg.auto_start && !no_autostart {
        info!("auto-start: starting the stream");
        controller.start();
    } else {
        info!("auto-start {}", if no_autostart { "suppressed (--no-autostart)" } else { "off in config" });
    }

    let mut app = TrayApp {
        controller,
        tray,
        rx,
        ctx_slot,
        quit: false,
        show_panel: opts.open_panel,
        pair,
        autostart,
        log_dir: log_dir()?,
    };
    loop {
        app.poll();
        if app.quit {
            break;
        }
        if std::mem::take(&mut app.show_panel) {
            if let Err(e) = panel::show(&mut app) {
                error!("panel failed: {e:#}");
            }
            continue;
        }
        if !win::pump_messages(Duration::from_millis(250)) {
            break;
        }
    }
    info!("quitting");
    app.tray.hide();
    app.pair.cancel();
    if !app.controller.shutdown(Duration::from_secs(12)) {
        warn!("engine did not stop in time; exiting anyway");
    }
    Ok(())
}

/// `stereo-pair quit`: ask a running tray app to quit cleanly.
pub fn signal_quit() -> Result<bool> {
    win::signal(win::QUIT_EVENT)
}
