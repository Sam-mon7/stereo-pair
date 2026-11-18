//! The control panel window (egui). Opened on demand from the tray; closing it
//! returns to the tray loop. Edits go straight to the controller (DSP changes are
//! live) and are persisted by it after a short debounce.
//!
//! Sections: header (start/stop, latency, volume, input, Calibrate...), one card per
//! speaker (role, gain, delay, EQ expander with response plot), Spatial, footer.
//! The calibration wizard lives in [`super::calib_ui`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use eframe::egui::{self, Color32, RichText, Ui, ViewportCommand};

use super::calib_ui::{CalibUi, Env};
use super::eq_ui::{self, Measured};
use super::pairing::Phase;
use super::{TrayApp, tray, win};
use crate::calib::job::{CalibrationResult, CalibrationState};
use crate::config::{Config, SpeakerConfig};
use crate::controller::{EngineState, Health, SpeakerState, SpeakerStatus, Status};
use crate::dsp::{ChannelSource, MAX_DELAY_MS};

const GAIN_RANGE: std::ops::RangeInclusive<f32> = -30.0..=12.0;
const LATENCY_RANGE: std::ops::RangeInclusive<u32> = 100..=2000;

pub fn show(app: &mut TrayApp) -> Result<()> {
    let icon = egui::IconData { rgba: tray::icon_rgba(Health::Streaming, 64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Stereo Pair")
            .with_inner_size([640.0, 780.0])
            .with_min_inner_size([480.0, 420.0])
            .with_icon(Arc::new(icon)),
        centered: true,
        persist_window: false,
        run_and_return: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "Stereo Pair",
        options,
        Box::new(|cc| {
            *app.ctx_slot.lock().unwrap() = Some(cc.egui_ctx.clone());
            Ok(Box::new(Panel::new(app)))
        }),
    );
    // eframe returned: the window is gone (closed, or Quit). Stop repaint requests
    // from other threads reaching its event loop.
    *app.ctx_slot.lock().unwrap() = None;
    tracing::debug!("panel closed");
    result.map_err(|e| anyhow!("{e}"))
}

struct Panel<'a> {
    app: &'a mut TrayApp,
    /// The settings being edited, and what the controller last got.
    cfg: Config,
    sent: Config,
    /// Latency while its slider is held (applied on release: it restarts the stream).
    latency_drag: Option<u32>,
    pair_open: bool,
    pin: String,
    remove_confirm: Option<String>,
    autostart_error: Option<String>,
    calib: CalibUi,
}

impl<'a> Panel<'a> {
    fn new(app: &'a mut TrayApp) -> Self {
        let cfg = app.controller.config();
        app.autostart = win::autostart_enabled();
        // Developer aid: open the add/pair dialog (mDNS scan only) on launch.
        let pair_open = std::env::var_os("STEREO_PAIR_OPEN_PAIRING").is_some_and(|v| v == "1");
        if pair_open && matches!(app.pair.phase(), Phase::Idle) {
            app.pair.scan();
        }
        Self {
            app,
            sent: cfg.clone(),
            cfg,
            latency_drag: None,
            pair_open,
            pin: String::new(),
            remove_confirm: None,
            autostart_error: None,
            calib: CalibUi::default(),
        }
    }

    fn push_changes(&mut self) {
        if self.cfg != self.sent {
            self.app.controller.update(self.cfg.clone());
            self.sent = self.cfg.clone();
        }
    }
}

fn health_color(h: Health) -> Color32 {
    let [r, g, b] = tray::color(h);
    Color32::from_rgb(r, g, b)
}

fn speaker_color(s: SpeakerState) -> Color32 {
    health_color(match s {
        SpeakerState::Streaming => Health::Streaming,
        SpeakerState::Connecting | SpeakerState::Retrying | SpeakerState::Testing => Health::Connecting,
        SpeakerState::Error => Health::Error,
        SpeakerState::Disabled | SpeakerState::Idle => Health::Off,
    })
}

fn dot(ui: &mut Ui, c: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, c);
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl eframe::App for Panel<'_> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Runs even while the window is minimised, so tray clicks keep working.
        self.app.poll();
        if self.app.quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if std::mem::take(&mut self.app.show_panel) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        ui.style_mut().spacing.slider_width = 240.0;
        let st = self.app.controller.status();
        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(6.0);
            self.footer(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.header(ui, &st);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                self.speakers(ui, &st);
                ui.add_space(2.0);
                eq_ui::spatial_editor(ui, &mut self.cfg.spatial);
            });
        });
        let ctx = ui.ctx().clone();
        self.pair_window(&ctx);
        let env = Env { controller: &self.app.controller, cfg: &mut self.cfg, status: &st, last: &mut self.app.last_calibration };
        self.calib.window(&ctx, env);
        self.push_changes();
    }
}

impl Panel<'_> {
    fn header(&mut self, ui: &mut Ui, st: &Status) {
        ui.horizontal(|ui| {
            dot(ui, health_color(st.health()));
            ui.heading("Stereo Pair");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = if st.wanted { "Stop" } else { "Start" };
                if ui.add(egui::Button::new(RichText::new(text).strong()).min_size(egui::vec2(72.0, 26.0))).clicked() {
                    self.app.toggle_streaming();
                }
                let busy = self.app.controller.calibration().busy();
                let label = if busy { "Calibrating..." } else { "Calibrate..." };
                if ui
                    .add(egui::Button::new(label).min_size(egui::vec2(96.0, 26.0)))
                    .on_hover_text("Measure the speakers with a mic at your seat and match their timing, level and tone")
                    .clicked()
                {
                    let hint = self.cfg.calibration_mic.clone();
                    self.calib.show(hint.as_deref());
                }
            });
        });
        let summary = st.summary();
        match &st.engine {
            EngineState::Error(_) => ui.colored_label(health_color(Health::Error), summary),
            _ => ui.label(summary),
        };
        ui.add_space(6.0);
        egui::Grid::new("global").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Latency");
            ui.horizontal(|ui| {
                let mut lat = self.latency_drag.unwrap_or(self.cfg.latency_ms);
                let r = ui.add(egui::Slider::new(&mut lat, LATENCY_RANGE).suffix(" ms").step_by(10.0));
                if r.changed() {
                    self.latency_drag = Some(lat);
                }
                if let Some(l) = self.latency_drag
                    && !r.dragged()
                    && !r.has_focus()
                {
                    self.cfg.latency_ms = l;
                    self.latency_drag = None;
                }
                let restarting = st.latency_ms.is_some_and(|l| l.abs_diff(self.cfg.latency_ms) > 1);
                ui.weak(if restarting { "restarting..." } else { "change restarts the stream" });
            });
            ui.end_row();

            ui.label("Windows volume");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cfg.follow_windows_volume, "Follow");
                let text = match st.windows_volume {
                    Some((_, true)) => "muted".to_string(),
                    Some((v, false)) => format!("{:.0} %", v * 100.0),
                    None => "unknown".to_string(),
                };
                ui.weak(text);
            });
            ui.end_row();

            ui.label("Audio source");
            ui.weak(st.input.clone().unwrap_or_else(|| "not capturing (stream stopped)".into()));
            ui.end_row();
        });
    }

    fn speakers(&mut self, ui: &mut Ui, st: &Status) {
        ui.strong("Speakers");
        ui.add_space(4.0);
        if self.cfg.speakers.is_empty() {
            ui.weak("No speakers yet. Use \"Add / pair speaker\" below.");
        }
        let mut remove: Option<usize> = None;
        let speakers_len = self.cfg.speakers.len();
        // measured responses for the EQ plots: a result on screen, else the last applied
        let cal: Option<Arc<CalibrationResult>> = match self.app.controller.calibration() {
            CalibrationState::Done(r) => Some(r),
            _ => self.app.last_calibration.clone(),
        };
        for i in 0..speakers_len {
            let key = self.cfg.speakers[i].key();
            let status = st.speakers.iter().find(|s| s.key == key).cloned();
            let controller = &self.app.controller;
            let confirm = &mut self.remove_confirm;
            let s = &mut self.cfg.speakers[i];
            let measured = cal.as_ref().and_then(|r| {
                let j = r.keys.iter().position(|k| *k == key)?;
                Some(Measured { freqs: &r.analysis.grid_hz, normalized: &r.proposal.speakers[j].normalized_db, target: &r.proposal.target_db })
            });
            egui::Frame::group(ui.style()).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                speaker_card(ui, s, status.as_ref(), controller, confirm, &mut remove, i);
                eq_ui::eq_editor(ui, i, &mut s.eq, measured);
            });
            ui.add_space(6.0);
        }
        if let Some(i) = remove {
            let s = self.cfg.speakers.remove(i);
            tracing::info!("removed speaker {} ({}) from config", s.name, s.key());
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Add / pair speaker...").clicked() {
                self.pair_open = true;
                if matches!(self.app.pair.phase(), Phase::Idle) {
                    self.app.pair.scan();
                }
            }
            if ui.button("Open log folder").clicked() {
                let _ = std::fs::create_dir_all(&self.app.log_dir);
                win::open_folder(&self.app.log_dir);
            }
            let mut autostart = self.app.autostart;
            if ui.checkbox(&mut autostart, "Start with Windows").changed() {
                self.autostart_error = self.app.set_autostart(autostart);
            }
            ui.checkbox(&mut self.cfg.auto_start, "Stream on launch");
        });
        if let Some(e) = &self.autostart_error {
            ui.colored_label(health_color(Health::Error), e);
        }
    }

    fn pair_window(&mut self, ctx: &egui::Context) {
        // A finished pairing adds the speaker (once).
        if let Phase::Done(p) = self.app.pair.phase() {
            let ip = p.ip.to_string();
            if !self.cfg.speakers.iter().any(|s| s.ip.as_deref() == Some(ip.as_str())) {
                let has = |r| self.cfg.speakers.iter().any(|s| s.role == r);
                let role = if !has(ChannelSource::Left) {
                    ChannelSource::Left
                } else if !has(ChannelSource::Right) {
                    ChannelSource::Right
                } else {
                    ChannelSource::Mono
                };
                let mut s = SpeakerConfig::new(&p.name, &ip, role);
                if p.port != crate::airplay::session::AIRPLAY_PORT {
                    s.port = Some(p.port);
                }
                tracing::info!("adding speaker {} ({ip}) as {:?}", p.name, role);
                self.cfg.speakers.push(s);
            }
        }
        if !self.pair_open {
            return;
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new("Add / pair speaker")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let pair = &mut self.app.pair;
                match pair.phase() {
                    Phase::Idle => {
                        ui.label("Search the network for AirPlay speakers.");
                        if ui.button("Scan").clicked() {
                            pair.scan();
                        }
                    }
                    Phase::Scanning => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Searching for AirPlay receivers (4 s)...");
                        });
                    }
                    Phase::Found => {
                        let found = pair.found();
                        if found.is_empty() {
                            ui.label("No AirPlay receivers answered. (mDNS replies may be blocked by the firewall.)");
                        }
                        egui::Grid::new("found").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                            for r in &found {
                                let ip = r.ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into());
                                ui.vertical(|ui| {
                                    let nowrap = |t: RichText| egui::Label::new(t).wrap_mode(egui::TextWrapMode::Extend);
                                    ui.add(nowrap(RichText::new(&r.name).strong()));
                                    ui.add(nowrap(RichText::new(r.model()).weak()));
                                });
                                ui.monospace(&ip);
                                let configured = self.cfg.speakers.iter().any(|s| s.ip.as_deref() == Some(ip.as_str()));
                                if configured {
                                    ui.weak("added");
                                } else if ui.add_enabled(r.ip.is_some(), egui::Button::new("Add")).clicked() {
                                    self.pin.clear();
                                    pair.pair(r, &self.cfg.client_name);
                                }
                                ui.end_row();
                            }
                        });
                        ui.add_space(4.0);
                        if ui.button("Scan again").clicked() {
                            pair.scan();
                        }
                    }
                    Phase::Busy(text) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text);
                        });
                    }
                    Phase::NeedPin => {
                        ui.label("Enter the PIN shown on the speaker:");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.pin).desired_width(120.0).char_limit(8).font(egui::TextStyle::Monospace),
                        );
                        self.pin.retain(|c| c.is_ascii_digit());
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if (ui.add_enabled(!self.pin.is_empty(), egui::Button::new("Pair")).clicked() || enter) && !self.pin.is_empty() {
                                pair.submit_pin(&self.pin);
                            }
                            if ui.button("Cancel").clicked() {
                                pair.cancel();
                            }
                        });
                    }
                    Phase::Done(p) => {
                        ui.label(format!(
                            "Paired with {} ({}) using {} pairing. It has been added to the speaker list.",
                            p.name,
                            p.ip,
                            if p.kind == "hap" { "PIN" } else { "transient" }
                        ));
                        if ui.button("Done").clicked() {
                            pair.reset();
                            close = true;
                        }
                    }
                    Phase::Failed(e) => {
                        ui.colored_label(health_color(Health::Error), e);
                        if ui.button("Back").clicked() {
                            pair.reset();
                        }
                    }
                }
            });
        if !open || close {
            if matches!(self.app.pair.phase(), Phase::NeedPin) {
                self.app.pair.cancel();
            }
            if matches!(self.app.pair.phase(), Phase::Done(_) | Phase::Failed(_)) {
                self.app.pair.reset();
            }
            self.pair_open = false;
        }
    }
}

fn speaker_card(
    ui: &mut Ui,
    s: &mut SpeakerConfig,
    status: Option<&SpeakerStatus>,
    controller: &crate::controller::Controller,
    confirm: &mut Option<String>,
    remove: &mut Option<usize>,
    index: usize,
) {
    let state = status.map(|x| x.state).unwrap_or(SpeakerState::Idle);
    let key = s.key();
    ui.horizontal(|ui| {
        ui.checkbox(&mut s.enabled, "").on_hover_text("Enabled");
        dot(ui, speaker_color(state));
        ui.strong(&s.name);
        ui.weak(s.ip.clone().unwrap_or_else(|| "(mDNS)".into()));
        ui.colored_label(speaker_color(state), state.label());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if confirm.as_deref() == Some(key.as_str()) {
                if ui.button("Keep").clicked() {
                    *confirm = None;
                }
                if ui.button(RichText::new("Remove").color(health_color(Health::Error))).clicked() {
                    *remove = Some(index);
                    *confirm = None;
                }
            } else {
                if ui.small_button("Remove").on_hover_text("Remove from the list (pairing keys are kept)").clicked() {
                    *confirm = Some(key.clone());
                }
                let can_test = !matches!(state, SpeakerState::Testing | SpeakerState::Connecting);
                if ui
                    .add_enabled(can_test, egui::Button::new("Test"))
                    .on_hover_text("Play a quiet chime on this speaker only")
                    .clicked()
                {
                    controller.test_tone(&key);
                }
            }
        });
    });
    ui.add_space(2.0);
    egui::Grid::new(("speaker", index)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        ui.label("Role");
        egui::ComboBox::from_id_salt(("role", index))
            .selected_text(s.role.label())
            .width(140.0)
            .show_ui(ui, |ui| {
                for r in ChannelSource::ALL {
                    ui.selectable_value(&mut s.role, r, r.label());
                }
            });
        ui.end_row();

        ui.label("Gain");
        ui.add(egui::Slider::new(&mut s.gain_db, GAIN_RANGE).suffix(" dB").step_by(0.5).fixed_decimals(1));
        ui.end_row();

        ui.label("Delay");
        ui.add(
            egui::Slider::new(&mut s.delay_ms, 0.0..=MAX_DELAY_MS)
                .suffix(" ms")
                .step_by(0.1)
                .fixed_decimals(1)
                .drag_value_speed(0.1),
        );
        ui.end_row();
    });
    if let Some(st) = status {
        ui.horizontal_wrapped(|ui| {
            ui.weak(format!(
                "packets {}   resends {}   send errors {}",
                thousands(st.packets),
                thousands(st.retransmitted),
                thousands(st.send_errors)
            ));
        });
        if let Some(d) = &st.detail {
            let c = if st.state == SpeakerState::Streaming { ui.visuals().weak_text_color() } else { speaker_color(st.state) };
            ui.colored_label(c, d);
        }
    }
}
