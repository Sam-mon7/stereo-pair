//! Per-speaker EQ editor and the pair-wide Spatial section of the panel. Edits change
//! the panel's `Config`; the panel pushes it to the controller, and the DSP glides /
//! crossfades to the new settings (live, click-free).

use eframe::egui::{self, RichText, Ui};

use super::plot::{self, Curve};
use crate::calib::{TIMELINE_RATE, grid};
use crate::dsp::biquad::FilterKind;
use crate::dsp::eq::{BandSource, EqBand, EqSettings, MAX_BANDS};
use crate::dsp::spatial::{BALANCE_RANGE, BASS_MONO_RANGE, SpatialSettings, WIDTH_RANGE, XTC_DISTANCE_RANGE, XTC_SPACING_RANGE};

/// Calibration data for one speaker, for the plot.
pub struct Measured<'a> {
    pub freqs: &'a [f64],
    /// Level-normalised measured response (before any EQ).
    pub normalized: &'a [f64],
    pub target: &'a [f64],
}

/// Response (dB) of the chain on `freqs` (0 dB when bypassed).
pub fn chain_db(eq: &EqSettings, freqs: &[f64]) -> Vec<f64> {
    let coeffs: Vec<_> = eq.active().map(|b| b.coeffs(TIMELINE_RATE)).collect();
    freqs.iter().map(|&f| coeffs.iter().map(|c| c.magnitude_db(f, TIMELINE_RATE)).sum()).collect()
}

pub fn eq_editor(ui: &mut Ui, index: usize, eq: &mut EqSettings, measured: Option<Measured>) {
    let active = eq.active().count();
    let title = if eq.bands.is_empty() {
        "EQ".to_string()
    } else if eq.bypass {
        format!("EQ (bypassed, {} band{})", eq.bands.len(), if eq.bands.len() == 1 { "" } else { "s" })
    } else {
        format!("EQ ({active} active band{})", if active == 1 { "" } else { "s" })
    };
    egui::CollapsingHeader::new(title).id_salt(("eq", index)).default_open(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut eq.bypass, "Bypass").on_hover_text("Turn the whole EQ off (bands are kept)");
            if ui.add_enabled(eq.bands.len() < MAX_BANDS, egui::Button::new("Add band")).clicked() {
                eq.bands.push(EqBand::default());
            }
            let cal = eq.bands.iter().filter(|b| b.source == BandSource::Calibration).count();
            if cal > 0
                && ui.small_button("Remove calibration bands").on_hover_text("Keeps the bands you added by hand").clicked()
            {
                eq.bands.retain(|b| b.source != BandSource::Calibration);
            }
        });
        if !eq.bands.is_empty() {
            let mut remove = None;
            egui::Grid::new(("eqbands", index)).num_columns(7).spacing([6.0, 3.0]).show(ui, |ui| {
                ui.weak("On");
                ui.weak("Type");
                ui.weak("Freq");
                ui.weak("Gain");
                ui.weak("Q");
                ui.weak("");
                ui.weak("");
                ui.end_row();
                for (i, b) in eq.bands.iter_mut().enumerate() {
                    ui.checkbox(&mut b.enabled, "");
                    egui::ComboBox::from_id_salt(("eqtype", index, i)).selected_text(b.kind.label()).width(92.0).show_ui(ui, |ui| {
                        for k in FilterKind::ALL {
                            ui.selectable_value(&mut b.kind, k, k.label());
                        }
                    });
                    let speed = (b.freq_hz as f64 * 0.004).max(0.1);
                    ui.add(egui::DragValue::new(&mut b.freq_hz).range(20.0..=20000.0).speed(speed).max_decimals(0).suffix(" Hz"));
                    ui.add_enabled(
                        b.kind.has_gain(),
                        egui::DragValue::new(&mut b.gain_db).range(-24.0..=12.0).speed(0.1).fixed_decimals(1).suffix(" dB"),
                    );
                    ui.add(egui::DragValue::new(&mut b.q).range(0.1..=10.0).speed(0.01).fixed_decimals(2));
                    if b.source == BandSource::Calibration {
                        ui.weak("cal").on_hover_text("Proposed by the calibration (re-running it replaces these)");
                    } else {
                        ui.label("");
                    }
                    if ui.small_button("x").on_hover_text("Remove this band").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                eq.bands.remove(i);
            }
        }
        let freqs = grid();
        let eq_db = chain_db(eq, &freqs);
        let mut curves = vec![Curve { freqs: &freqs, db: &eq_db, color: plot::EQ, dashed: false, label: "EQ" }];
        let after: Vec<f64>;
        if let Some(m) = &measured {
            after = m.normalized.iter().zip(&eq_db).map(|(a, b)| a + b).collect();
            curves.insert(0, Curve { freqs: m.freqs, db: m.target, color: plot::TARGET, dashed: true, label: "target" });
            curves.insert(0, Curve { freqs: m.freqs, db: m.normalized, color: plot::BEFORE, dashed: false, label: "measured" });
            curves.push(Curve { freqs: m.freqs, db: &after, color: plot::AFTER, dashed: false, label: "measured + this EQ" });
        }
        let range = plot::auto_range(&curves, 12.0);
        let range = (range.0.max(-30.0), range.1.min(24.0));
        plot::response_plot(ui, 130.0, range, &curves);
        if measured.is_none() {
            ui.weak("Run Calibrate... to see the measured response here.");
        }
    });
}

pub fn spatial_editor(ui: &mut Ui, s: &mut SpatialSettings) {
    let on = [s.width_enabled, s.balance_enabled, s.bass_mono_enabled, s.xtc_enabled].iter().filter(|x| **x).count();
    let title = if on == 0 { "Spatial".to_string() } else { format!("Spatial ({on} on)") };
    egui::CollapsingHeader::new(RichText::new(title).strong()).id_salt("spatial").default_open(false).show(ui, |ui| {
        egui::Grid::new("spatialgrid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.checkbox(&mut s.width_enabled, "Width");
            ui.add_enabled(s.width_enabled, egui::Slider::new(&mut s.width_pct, WIDTH_RANGE).suffix(" %").step_by(1.0))
                .on_hover_text("Mid/side: 0 % = mono, 100 % = unchanged, 200 % = wider");
            ui.end_row();

            ui.checkbox(&mut s.balance_enabled, "Balance");
            ui.horizontal(|ui| {
                ui.add_enabled(s.balance_enabled, egui::Slider::new(&mut s.balance, BALANCE_RANGE).step_by(1.0).show_value(false));
                let b = s.balance.round();
                ui.weak(if b < 0.0 {
                    format!("left {:.0}", -b)
                } else if b > 0.0 {
                    format!("right {b:.0}")
                } else {
                    "centre".into()
                });
            });
            ui.end_row();

            ui.checkbox(&mut s.bass_mono_enabled, "Bass mono");
            ui.add_enabled(
                s.bass_mono_enabled,
                egui::Slider::new(&mut s.bass_mono_hz, BASS_MONO_RANGE).suffix(" Hz").step_by(5.0).text("below"),
            )
            .on_hover_text("Both speakers play the same bass below this frequency (Linkwitz-Riley crossover)");
            ui.end_row();

            ui.checkbox(&mut s.xtc_enabled, "3D (experimental)")
                .on_hover_text("Crosstalk cancellation: widens the soundstage beyond the speakers for a listener sitting centred");
            ui.add_enabled(s.xtc_enabled, egui::Slider::new(&mut s.xtc_strength_pct, 0.0..=100.0).suffix(" %").step_by(1.0).text("strength"));
            ui.end_row();

            ui.label("");
            ui.add_enabled_ui(s.xtc_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("speakers");
                    ui.add(egui::DragValue::new(&mut s.xtc_spacing_m).range(XTC_SPACING_RANGE).speed(0.01).fixed_decimals(2).suffix(" m"));
                    ui.label("apart, you sit");
                    ui.add(egui::DragValue::new(&mut s.xtc_distance_m).range(XTC_DISTANCE_RANGE).speed(0.01).fixed_decimals(2).suffix(" m"));
                    ui.label("away");
                });
                let (angle, us) = s.xtc_geometry();
                ui.weak(format!("{angle:.0} degrees between the speakers; far-ear delay {us:.0} us"));
            });
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.small_button("Reset").clicked() {
                *s = SpatialSettings::default();
            }
            ui.weak("Applies to the pair before each speaker's role. 5.1/7.1 sources reach this app already downmixed to stereo by Windows.");
        });
    });
}
