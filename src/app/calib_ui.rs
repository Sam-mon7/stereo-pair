//! The "Calibrate..." wizard: pick the mic input and sweep level (live meter, clipping
//! / no-signal warnings), run the measurement through the controller, then review the
//! result (delay / level differences, response plots, proposed corrections) and Apply,
//! Discard or Save it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Sense, Ui, Vec2};

use super::plot::{self, Curve};
use super::win;
use crate::audio::mic::{Mic, find_input, input_device_names};
use crate::calib::job::{CalibrationRequest, CalibrationResult, CalibrationState, DEFAULT_LEVEL_DBFS, DEFAULT_REPEATS, MicInput};
use crate::config::Config;
use crate::controller::{Controller, EngineState, SpeakerState, Status};
use crate::dsp::master_volume_gain;

const OK: Color32 = Color32::from_rgb(90, 200, 120);
const WARN: Color32 = Color32::from_rgb(240, 170, 60);
const BAD: Color32 = Color32::from_rgb(235, 80, 70);

pub struct CalibUi {
    pub open: bool,
    devices: Vec<String>,
    device: Option<String>,
    mic: Option<Arc<Mic>>,
    mic_error: Option<String>,
    level_dbfs: f32,
    repeats: usize,
    rms: [f32; 2],
    peak_hold: [(f32, Instant); 2],
    clip_at: Option<Instant>,
    message: Option<(Color32, String)>,
    saved_dir: Option<PathBuf>,
}

impl Default for CalibUi {
    fn default() -> Self {
        Self {
            open: false,
            devices: Vec::new(),
            device: None,
            mic: None,
            mic_error: None,
            level_dbfs: DEFAULT_LEVEL_DBFS as f32,
            repeats: DEFAULT_REPEATS,
            rms: [-150.0; 2],
            peak_hold: [(-150.0, Instant::now()); 2],
            clip_at: None,
            message: None,
            saved_dir: None,
        }
    }
}

/// What the wizard needs from the panel.
pub struct Env<'a> {
    pub controller: &'a Controller,
    pub cfg: &'a mut Config,
    pub status: &'a Status,
    pub last: &'a mut Option<Arc<CalibrationResult>>,
}

impl CalibUi {
    pub fn show(&mut self, hint: Option<&str>) {
        self.open = true;
        self.devices = input_device_names();
        if self.mic.is_none() {
            self.device = find_input(hint);
            self.open_mic();
        }
    }

    fn open_mic(&mut self) {
        self.mic = None;
        self.mic_error = None;
        self.rms = [-150.0; 2];
        self.peak_hold = [(-150.0, Instant::now()); 2];
        let Some(d) = self.device.clone() else {
            self.mic_error = Some("No capture device found.".into());
            return;
        };
        match Mic::open(&d) {
            Ok(m) => self.mic = Some(Arc::new(m)),
            Err(e) => self.mic_error = Some(format!("{e:#}")),
        }
    }

    fn poll_meter(&mut self) {
        let Some(m) = &self.mic else { return };
        let l = m.take_levels();
        let now = Instant::now();
        for c in 0..2 {
            self.rms[c] = l.rms_dbfs[c];
            let (p, t) = self.peak_hold[c];
            if l.peak_dbfs[c] >= p || now.duration_since(t) > Duration::from_millis(1500) {
                self.peak_hold[c] = (l.peak_dbfs[c], now);
            }
            if l.peak_dbfs[c] > -1.0 {
                self.clip_at = Some(now);
            }
        }
    }

    /// The input that carries the mic (the louder one).
    fn mic_channel(&self) -> usize {
        if self.rms[1] > self.rms[0] { 1 } else { 0 }
    }

    fn meter(&self, ui: &mut Ui) {
        let mic = self.mic_channel();
        for c in 0..2 {
            ui.horizontal(|ui| {
                let label = format!("Input {}", c + 1);
                ui.add_sized([62.0, 14.0], egui::Label::new(if c == mic { RichText::new(label).strong() } else { RichText::new(label).weak() }));
                let (rect, _) = ui.allocate_exact_size(Vec2::new(220.0, 12.0), Sense::hover());
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                let t = |db: f32| ((db + 80.0) / 80.0).clamp(0.0, 1.0);
                let peak = self.peak_hold[c].0;
                let color = if peak > -1.0 {
                    BAD
                } else if peak > -6.0 {
                    WARN
                } else {
                    OK
                };
                let mut bar = rect;
                bar.set_width(rect.width() * t(self.rms[c]));
                p.rect_filled(bar, 2.0, color.gamma_multiply(0.8));
                let x = rect.left() + rect.width() * t(peak);
                p.vline(x, rect.y_range(), egui::Stroke::new(2.0, color));
                ui.weak(format!("rms {:>4.0}  peak {:>4.0} dBFS", self.rms[c].max(-99.0), peak.max(-99.0)));
                if c == mic && self.rms[c] > -90.0 {
                    ui.small("mic");
                }
            });
        }
    }

    fn mic_warnings(&self, ui: &mut Ui) {
        if self.clip_at.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            ui.colored_label(BAD, "Clipping! Turn the input gain on the Focusrite down.");
        } else if self.rms[0].max(self.rms[1]) < -80.0 && self.mic.is_some() {
            ui.colored_label(WARN, "No signal on either input: check the mic cable, 48V phantom power and the gain knob.");
        }
    }

    pub fn window(&mut self, ctx: &egui::Context, env: Env) {
        if !self.open {
            return;
        }
        self.poll_meter();
        ctx.request_repaint_after(Duration::from_millis(100));
        let state = env.controller.calibration();
        let mut open = true;
        egui::Window::new("Calibrate")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .default_height(560.0)
            .vscroll(true)
            .show(ctx, |ui| match &state {
                CalibrationState::Idle | CalibrationState::Failed(_) => self.setup(ui, &state, env),
                CalibrationState::Measuring { progress, detail, .. } => {
                    ui.add(egui::ProgressBar::new(*progress).show_percentage());
                    ui.label(detail);
                    ui.add_space(6.0);
                    self.meter(ui);
                    self.mic_warnings(ui);
                    ui.add_space(6.0);
                    ui.weak("Keep the room quiet and leave the mic where it is.");
                    if ui.button("Cancel").clicked() {
                        env.controller.cancel_calibration();
                    }
                }
                CalibrationState::Analyzing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Analysing the recording...");
                    });
                }
                CalibrationState::Done(r) => self.results(ui, r.clone(), env),
            });
        if !open {
            self.open = false;
            if !state.busy() {
                // free the capture device (a running measurement keeps its own handle)
                self.mic = None;
            }
        }
    }

    fn setup(&mut self, ui: &mut Ui, state: &CalibrationState, env: Env) {
        if let Some((c, m)) = &self.message {
            ui.colored_label(*c, m);
            if let Some(dir) = self.saved_dir.clone()
                && ui.small_button("Open folder").clicked()
            {
                win::open_folder(&dir);
            }
            ui.add_space(4.0);
        }
        if let CalibrationState::Failed(e) = state {
            ui.colored_label(BAD, format!("Last attempt: {e}"));
            ui.add_space(4.0);
        }
        ui.label("Place the mic at your listening position, between the speakers, pointing up, at ear height.");
        ui.add_space(6.0);
        egui::Grid::new("calsetup").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Mic input");
            ui.horizontal(|ui| {
                let mut chosen = self.device.clone();
                egui::ComboBox::from_id_salt("micdev")
                    .selected_text(chosen.clone().unwrap_or_else(|| "(none)".into()))
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for d in &self.devices {
                            ui.selectable_value(&mut chosen, Some(d.clone()), d);
                        }
                    });
                if chosen != self.device {
                    self.device = chosen;
                    env.cfg.calibration_mic = self.device.clone();
                    self.open_mic();
                }
                if ui.small_button("Refresh").clicked() {
                    self.devices = input_device_names();
                    self.open_mic();
                }
            });
            ui.end_row();
            ui.label("");
            match (&self.mic, &self.mic_error) {
                (_, Some(e)) => ui.colored_label(BAD, e),
                (Some(m), None) => ui.weak(format!("{}; the louder input is used", m.format())),
                _ => ui.weak(""),
            };
            ui.end_row();
        });
        self.meter(ui);
        self.mic_warnings(ui);
        ui.weak("Tip: clap once; the mic's input should jump well above the other one.");
        ui.add_space(8.0);

        // levels
        let st = env.status;
        let music_db = match (env.cfg.follow_windows_volume, st.windows_volume) {
            (true, Some((v, muted))) => 20.0 * master_volume_gain(v, muted).max(1e-6).log10(),
            _ => 0.0,
        };
        egui::Grid::new("callevel").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Sweep level");
            ui.add(egui::Slider::new(&mut self.level_dbfs, -40.0..=-6.0).suffix(" dBFS").step_by(1.0));
            ui.end_row();
            ui.label("");
            let diff = self.level_dbfs - music_db;
            if diff > 3.0 {
                ui.colored_label(
                    WARN,
                    format!(
                        "About {diff:.0} dB louder than the loudest music at your current volume ({music_db:.0} dB). The sweeps ignore the Windows volume; consider a lower level."
                    ),
                );
            } else {
                ui.weak(format!(
                    "About {:.0} dB below the loudest music at your current volume. The sweeps ignore the Windows volume.",
                    -diff
                ));
            }
            ui.end_row();
            ui.label("Sweeps");
            ui.add(egui::Slider::new(&mut self.repeats, 1..=4).suffix(" per speaker"));
            ui.end_row();
        });

        // readiness
        let enabled: Vec<_> = st.speakers.iter().filter(|s| s.enabled).collect();
        let not_streaming: Vec<&str> = enabled.iter().filter(|s| s.state != SpeakerState::Streaming).map(|s| s.name.as_str()).collect();
        let ready_msg = if st.engine != EngineState::Running {
            Some("Start streaming first: the sweeps play through the running stream.".to_string())
        } else if enabled.is_empty() {
            Some("No speaker is enabled.".to_string())
        } else if !not_streaming.is_empty() {
            Some(format!("Waiting for {} to connect.", not_streaming.join(", ")))
        } else if self.mic.is_none() {
            Some("Choose a working mic input.".to_string())
        } else {
            None
        };
        let latency = st.latency_ms.unwrap_or(env.cfg.latency_ms) as f64 / 1000.0;
        let names: Vec<&str> = enabled.iter().map(|s| s.name.as_str()).collect();
        if let Some(mic) = self.mic.clone() {
            let probe = CalibrationRequest::new(mic.clone() as Arc<dyn MicInput>, self.level_dbfs as f64, self.repeats);
            let secs = probe.duration(names.len().max(1), latency);
            ui.add_space(6.0);
            plot::small_label(
                ui,
                format!(
                    "What happens: the music mutes, then you hear {} rising tones of {:.0} s each, one speaker at a time ({}), about {:.0} s in all. Stay quiet and still; the music comes back by itself.",
                    names.len() * self.repeats,
                    probe.sweep_seconds,
                    names.join(", then "),
                    secs
                ),
            );
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let start = ui.add_enabled(ready_msg.is_none(), egui::Button::new(RichText::new("Start measurement").strong()));
            if start.clicked()
                && let Some(mic) = self.mic.clone()
            {
                self.message = None;
                self.saved_dir = None;
                env.controller.calibrate(CalibrationRequest::new(mic as Arc<dyn MicInput>, self.level_dbfs as f64, self.repeats));
            }
            if let Some(m) = &ready_msg {
                ui.colored_label(WARN, m);
            }
        });
    }

    fn results(&mut self, ui: &mut Ui, r: Arc<CalibrationResult>, env: Env) {
        let a = &r.analysis;
        let p = &r.proposal;
        if a.ok() {
            ui.colored_label(OK, "Measurement looks consistent.");
        } else {
            ui.colored_label(BAD, "The measurement is not reliable enough to apply:");
        }
        for i in &a.issues {
            ui.colored_label(if i.fatal { BAD } else { WARN }, format!("- {}", i.text));
        }
        ui.add_space(4.0);
        if a.speakers.len() >= 2 {
            let s0 = &a.speakers[0];
            for s in &a.speakers[1..] {
                let d = s.arrival_ms;
                let l = s.level_db - s0.level_db;
                ui.label(format!(
                    "{} arrives {:.2} ms {} than {} and is {:.1} dB {} at the mic.",
                    s.name,
                    d.abs(),
                    if d >= 0.0 { "later" } else { "earlier" },
                    s0.name,
                    l.abs(),
                    if l <= 0.0 { "quieter" } else { "louder" }
                ));
            }
        }
        ui.weak(format!(
            "Mic on input {} ({:.1} kHz), its clock {:+.0} ppm vs. the stream{}.",
            a.mic_channel + 1,
            a.mic_rate / 1000.0,
            a.clock_ppm,
            a.noise_dbfs.map(|n| format!(", room noise {n:.0} dBFS")).unwrap_or_default()
        ));
        ui.add_space(6.0);
        egui::Grid::new("calres").num_columns(a.speakers.len() + 1).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
            ui.label("");
            for s in &a.speakers {
                ui.strong(&s.name);
            }
            ui.end_row();
            ui.label("Arrival");
            for s in &a.speakers {
                ui.label(format!("{:+.2} ms (+-{:.2})", s.arrival_ms, s.arrival_spread_ms / 2.0));
            }
            ui.end_row();
            ui.label("Level at mic");
            let ref_level = a.speakers[0].level_db;
            for s in &a.speakers {
                ui.label(format!("{:+.1} dB (+-{:.1})", s.level_db - ref_level, s.level_spread_db / 2.0));
            }
            ui.end_row();
            ui.label("Signal / noise");
            for s in &a.speakers {
                ui.label(format!("{:.0} dB{}", s.pnr_db, s.snr_db.map(|x| format!(" (raw {x:.0} dB)")).unwrap_or_default()))
                    .on_hover_text("Impulse-response peak to noise; in brackets the sweep vs. the silence before it");
            }
            ui.end_row();
            ui.label("Bass roll-off");
            for s in &a.speakers {
                ui.label(format!("{:.0} Hz", s.lf_rolloff_hz));
            }
            ui.end_row();
            ui.strong("Proposed");
            ui.end_row();
            let current = |key: &str| env.cfg.speakers.iter().find(|s| s.key() == key);
            ui.label("Delay");
            for (k, sp) in r.keys.iter().zip(&p.speakers) {
                let cur = current(k).map(|s| s.delay_ms).unwrap_or(0.0);
                ui.label(format!("{cur:.1} -> {:.2} ms", sp.delay_ms));
            }
            ui.end_row();
            ui.label("Trim");
            for (k, sp) in r.keys.iter().zip(&p.speakers) {
                let cur = current(k).map(|s| s.gain_db).unwrap_or(0.0);
                ui.label(format!("{cur:.1} -> {:.1} dB", sp.gain_db));
            }
            ui.end_row();
            ui.label("EQ");
            for sp in &p.speakers {
                let text = if sp.bands.is_empty() { "no bands".to_string() } else { format!("{} bands", sp.bands.len()) };
                let detail = sp
                    .bands
                    .iter()
                    .map(|b| format!("{:.0} Hz  {:+.1} dB  Q {:.2}", b.freq_hz, b.gain_db, b.q))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.label(text).on_hover_text(if detail.is_empty() { "Nothing worth correcting".into() } else { detail });
            }
            ui.end_row();
        });
        ui.add_space(6.0);
        for (s, sp) in a.speakers.iter().zip(&p.speakers) {
            ui.label(format!(
                "{}: deviation from target {:.1} dB -> {:.1} dB rms",
                s.name, sp.error_rms_before, sp.error_rms_after
            ));
            let curves = [
                Curve { freqs: &a.grid_hz, db: &sp.normalized_db, color: plot::BEFORE, dashed: false, label: "measured" },
                Curve { freqs: &a.grid_hz, db: &p.target_db, color: plot::TARGET, dashed: true, label: "target" },
                Curve { freqs: &a.grid_hz, db: &sp.corrected_db, color: plot::AFTER, dashed: false, label: "with proposed EQ" },
                Curve { freqs: &a.grid_hz, db: &sp.eq_db, color: plot::EQ, dashed: false, label: "EQ" },
            ];
            let range = plot::auto_range(&curves, 12.0);
            plot::response_plot(ui, 120.0, (range.0.max(-36.0), range.1.min(24.0)), &curves);
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            let apply = ui
                .add_enabled(a.ok(), egui::Button::new(RichText::new("Apply").strong()))
                .on_hover_text("Set these delays and trims, and replace the calibration EQ bands (your own bands stay)");
            if apply.clicked() {
                let n = r.apply_to(env.cfg);
                *env.last = Some(r.clone());
                env.controller.dismiss_calibration();
                self.message = Some((
                    OK,
                    format!("Applied to {n} speaker(s). Listen, and use each speaker's EQ Bypass to compare."),
                ));
            }
            if ui.button("Discard").clicked() {
                env.controller.dismiss_calibration();
                self.message = None;
            }
            if ui.button("Save measurement").clicked() {
                let res = crate::calib::report::calibration_dir()
                    .and_then(|d| crate::calib::report::save(&d, &r.schedule, &r.analysis, &r.proposal));
                match res {
                    Ok(dir) => {
                        self.message = Some((OK, format!("Saved to {}", dir.display())));
                        self.saved_dir = Some(dir);
                    }
                    Err(e) => self.message = Some((BAD, format!("Could not save: {e:#}"))),
                }
            }
        });
        if let Some((c, m)) = &self.message {
            ui.colored_label(*c, m);
            if let Some(dir) = self.saved_dir.clone()
                && ui.small_button("Open folder").clicked()
            {
                win::open_folder(&dir);
            }
        }
    }
}
