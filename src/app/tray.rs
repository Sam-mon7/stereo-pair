//! The notification-area icon: state-coloured icon (off / connecting / streaming /
//! error), tooltip, and the context menu.

use anyhow::{Result, anyhow};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::controller::{Health, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Toggle,
    OpenPanel,
    Autostart,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    toggle: MenuItem,
    open: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
    shown: Option<(Health, String, bool)>,
}

impl Tray {
    pub fn new(autostart_on: bool) -> Result<Self> {
        let menu = Menu::new();
        let toggle = MenuItem::new("Start streaming", true, None);
        let open = MenuItem::new("Open panel", true, None);
        let autostart = CheckMenuItem::new("Start with Windows", true, autostart_on, None);
        let quit = MenuItem::new("Quit", true, None);
        menu.append_items(&[&toggle, &open, &PredefinedMenuItem::separator(), &autostart, &PredefinedMenuItem::separator(), &quit])?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Stereo Pair")
            .with_icon(tray_icon(Health::Off)?)
            .build()?;
        Ok(Self { icon, toggle, open, autostart, quit, shown: None })
    }

    pub fn action(&self, ev: &MenuEvent) -> Option<TrayAction> {
        let id = ev.id();
        if id == self.toggle.id() {
            Some(TrayAction::Toggle)
        } else if id == self.open.id() {
            Some(TrayAction::OpenPanel)
        } else if id == self.autostart.id() {
            Some(TrayAction::Autostart)
        } else if id == self.quit.id() {
            Some(TrayAction::Quit)
        } else {
            None
        }
    }

    pub fn set_autostart_checked(&self, on: bool) {
        self.autostart.set_checked(on);
    }

    /// Bring icon, tooltip and the Start/Stop item in line with `st` (no-op if same).
    pub fn update(&mut self, st: &Status) {
        let health = st.health();
        let tip = format!("Stereo Pair: {}", st.summary());
        let key = (health, tip.clone(), st.wanted);
        if self.shown.as_ref() == Some(&key) {
            return;
        }
        if self.shown.as_ref().is_none_or(|s| s.0 != health)
            && let Ok(i) = tray_icon(health)
        {
            let _ = self.icon.set_icon(Some(i));
        }
        // the shell truncates tooltips at 127 characters
        let tip_short: String = tip.chars().take(120).collect();
        let _ = self.icon.set_tooltip(Some(tip_short));
        self.toggle.set_text(if st.wanted { "Stop streaming" } else { "Start streaming" });
        self.shown = Some(key);
    }

    pub fn hide(&self) {
        let _ = self.icon.set_visible(false);
    }
}

pub fn color(h: Health) -> [u8; 3] {
    match h {
        Health::Off => [0x9a, 0xa0, 0xa6],
        Health::Connecting => [0xf5, 0xa6, 0x23],
        Health::Streaming => [0x30, 0xc1, 0x5a],
        Health::Error => [0xff, 0x45, 0x3a],
    }
}

fn tray_icon(h: Health) -> Result<Icon> {
    Icon::from_rgba(icon_rgba(h, 32), 32, 32).map_err(|e| anyhow!("tray icon: {e}"))
}

/// Signed distance from `p` to a rounded rectangle centred at `c`.
fn rounded_rect_sd(p: (f32, f32), c: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let qx = (p.0 - c.0).abs() - (half.0 - r);
    let qy = (p.1 - c.1).abs() - (half.1 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn circle_sd(p: (f32, f32), c: (f32, f32), r: f32) -> f32 {
    ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - r
}

fn cover(sd: f32) -> f32 {
    (0.5 - sd).clamp(0.0, 1.0)
}

/// Two little speaker cabinets (the pair) in the state colour, each with a tweeter
/// and a woofer, outlined so they read on light and dark taskbars.
pub fn icon_rgba(h: Health, size: u32) -> Vec<u8> {
    let s = size as f32 / 32.0;
    let fill = color(h).map(|c| c as f32 / 255.0);
    let cone = fill.map(|c| c * 0.38);
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let mut rgba = [0.0f32; 4];
            for cx in [8.5f32, 23.5] {
                let body = rounded_rect_sd(p, (cx, 16.0), (6.8, 12.5), 2.6);
                let outline = cover(body - 1.1) - cover(body);
                let woofer = cover(circle_sd(p, (cx, 20.0), 3.7));
                let tweeter = cover(circle_sd(p, (cx, 9.6), 1.7));
                let b = cover(body);
                let inner = (woofer + tweeter).min(1.0);
                // composite: outline (dark, translucent), body, cones
                let layers = [([0.0, 0.0, 0.0], outline * 0.6), (fill, b), (cone, b * inner)];
                for (c, a) in layers {
                    if a <= 0.0 {
                        continue;
                    }
                    for i in 0..3 {
                        rgba[i] = c[i] * a + rgba[i] * (1.0 - a);
                    }
                    rgba[3] = a + rgba[3] * (1.0 - a);
                }
            }
            let o = ((y * size + x) * 4) as usize;
            // straight (non-premultiplied) alpha
            let a = rgba[3];
            for i in 0..3 {
                out[o + i] = if a > 0.0 { ((rgba[i] / a).clamp(0.0, 1.0) * 255.0).round() as u8 } else { 0 };
            }
            out[o + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_differ_by_state_and_have_transparent_corners() {
        let a = icon_rgba(Health::Off, 32);
        let b = icon_rgba(Health::Streaming, 32);
        assert_eq!(a.len(), 32 * 32 * 4);
        assert_ne!(a, b);
        assert_eq!(a[3], 0, "corner is transparent");
        // centre of the left cabinet's body is opaque and in the state colour
        let o = ((14 * 32 + 8) * 4) as usize;
        assert_eq!(b[o + 3], 255);
        assert_eq!(&b[o..o + 3], &color(Health::Streaming));
    }
}
