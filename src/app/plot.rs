//! Small frequency-response plot (log frequency 20 Hz..20 kHz, dB) for the EQ editor
//! and the calibration results.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};

pub const BEFORE: Color32 = Color32::from_rgb(150, 150, 150);
pub const AFTER: Color32 = Color32::from_rgb(90, 170, 255);
pub const EQ: Color32 = Color32::from_rgb(255, 170, 60);
pub const TARGET: Color32 = Color32::from_rgb(220, 220, 220);

pub struct Curve<'a> {
    pub freqs: &'a [f64],
    pub db: &'a [f64],
    pub color: Color32,
    pub dashed: bool,
    pub label: &'a str,
}

const F_LO: f64 = 20.0;
const F_HI: f64 = 20000.0;

fn x_of(rect: Rect, f: f64) -> f32 {
    let t = ((f / F_LO).ln() / (F_HI / F_LO).ln()).clamp(0.0, 1.0);
    rect.left() + t as f32 * rect.width()
}

fn y_of(rect: Rect, db: f64, range: (f32, f32)) -> f32 {
    let t = ((db as f32 - range.0) / (range.1 - range.0)).clamp(-0.05, 1.05);
    rect.bottom() - t * rect.height()
}

fn fmt_hz(f: f64) -> String {
    if f >= 1000.0 { format!("{:.1}k", f / 1000.0).replace(".0k", "k") } else { format!("{f:.0}") }
}

/// Draw the plot (`height` px tall, full available width) and a legend line.
pub fn response_plot(ui: &mut Ui, height: f32, range: (f32, f32), curves: &[Curve]) {
    let width = ui.available_width().max(200.0);
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter_at(outer);
    let v = ui.visuals();
    painter.rect_filled(outer, 3.0, v.extreme_bg_color);
    let rect = Rect::from_min_max(outer.min + Vec2::new(30.0, 4.0), outer.max - Vec2::new(4.0, 14.0));
    let grid = Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6));
    let text = v.weak_text_color();
    let font = FontId::proportional(9.5);
    for f in [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0] {
        let x = x_of(rect, f);
        painter.vline(x, rect.y_range(), grid);
        if [100.0, 1000.0, 10000.0].contains(&f) {
            painter.text(Pos2::new(x, rect.bottom() + 1.0), Align2::CENTER_TOP, fmt_hz(f), font.clone(), text);
        }
    }
    let step = if range.1 - range.0 > 30.0 { 10.0 } else { 5.0 };
    let mut d = (range.0 / step).ceil() * step;
    while d <= range.1 {
        let y = y_of(rect, d as f64, range);
        let s = if d == 0.0 { Stroke::new(1.0, grid.color.gamma_multiply(1.8)) } else { grid };
        painter.hline(rect.x_range(), y, s);
        painter.text(Pos2::new(rect.left() - 3.0, y), Align2::RIGHT_CENTER, format!("{d:+.0}"), font.clone(), text);
        d += step;
    }
    let clip = painter.with_clip_rect(rect);
    for c in curves {
        let pts: Vec<Pos2> = c
            .freqs
            .iter()
            .zip(c.db)
            .filter(|(f, d)| **f >= F_LO && **f <= F_HI && d.is_finite())
            .map(|(f, d)| Pos2::new(x_of(rect, *f), y_of(rect, *d, range)))
            .collect();
        if pts.len() < 2 {
            continue;
        }
        let stroke = Stroke::new(1.5, c.color);
        if c.dashed {
            clip.extend(Shape::dashed_line(&pts, stroke, 5.0, 4.0));
        } else {
            clip.add(Shape::line(pts, stroke));
        }
    }
    painter.rect_stroke(outer, 3.0, Stroke::new(1.0, grid.color), StrokeKind::Inside);
    if let Some(p) = resp.hover_pos()
        && rect.contains(p)
    {
        let f = F_LO * (F_HI / F_LO).powf(((p.x - rect.left()) / rect.width()) as f64);
        let db = range.0 + (rect.bottom() - p.y) / rect.height() * (range.1 - range.0);
        resp.on_hover_text(format!("{} Hz, {db:+.1} dB", fmt_hz(f)));
    }
    ui.horizontal_wrapped(|ui| {
        for c in curves.iter().filter(|c| !c.label.is_empty()) {
            let (r, _) = ui.allocate_exact_size(Vec2::new(16.0, 10.0), Sense::hover());
            let y = r.center().y;
            if c.dashed {
                ui.painter().extend(Shape::dashed_line(&[Pos2::new(r.left(), y), Pos2::new(r.right(), y)], Stroke::new(1.5, c.color), 4.0, 3.0));
            } else {
                ui.painter().line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], Stroke::new(2.0, c.color));
            }
            ui.small(c.label);
        }
    });
}

/// Plot y-range covering the curves (at least +-`min_span`, rounded to 5 dB).
pub fn auto_range(curves: &[Curve], min_span: f32) -> (f32, f32) {
    let mut lo = -min_span;
    let mut hi = min_span;
    for c in curves {
        for (f, d) in c.freqs.iter().zip(c.db) {
            if (F_LO..=F_HI).contains(f) && d.is_finite() {
                lo = lo.min(*d as f32);
                hi = hi.max(*d as f32);
            }
        }
    }
    ((lo / 5.0).floor() * 5.0, (hi / 5.0).ceil() * 5.0)
}

pub fn small_label(ui: &mut Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
}
