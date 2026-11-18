//! mDNS discovery of AirPlay receivers (`_airplay._tcp` + `_raop._tcp`) and `GET /info`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tracing::debug;

use crate::rtsp::RtspSession;

#[derive(Clone, Debug, Default)]
pub struct Receiver {
    pub name: String,
    pub ip: Option<IpAddr>,
    pub airplay_port: Option<u16>,
    pub raop_port: Option<u16>,
    pub txt: BTreeMap<String, String>,
}

impl Receiver {
    pub fn model(&self) -> &str {
        self.txt.get("model").or_else(|| self.txt.get("am")).map(String::as_str).unwrap_or("?")
    }

    pub fn features(&self) -> u64 {
        let raw = self.txt.get("features").or_else(|| self.txt.get("ft")).cloned().unwrap_or_default();
        parse_features(&raw)
    }

    pub fn status_flags(&self) -> u64 {
        let raw = self.txt.get("flags").or_else(|| self.txt.get("sf")).cloned().unwrap_or_default();
        u64::from_str_radix(raw.trim_start_matches("0x"), 16).unwrap_or(0)
    }

    /// Human summary of what the TXT records say about pairing.
    pub fn pairing_hints(&self) -> Vec<&'static str> {
        let f = self.features();
        let sf = self.status_flags();
        let mut out = Vec::new();
        if f & (1 << 48) != 0 {
            out.push("CoreUtils pairing+encryption");
        }
        if f & (1 << 43) != 0 {
            out.push("system pairing");
        }
        if f & (1 << 46) != 0 {
            out.push("HomeKit access control");
        }
        if sf & 0x8 != 0 {
            out.push("PIN required");
        }
        if sf & 0x80 != 0 {
            out.push("password required");
        }
        if sf & 0x200 != 0 {
            out.push("one-time pairing required");
        }
        if sf & 0x400 != 0 {
            out.push("set up for HomeKit access control");
        }
        out
    }
}

/// pyatv `parse_features`: "0xLOW,0xHIGH" -> HIGH<<32 | LOW.
pub fn parse_features(s: &str) -> u64 {
    let mut parts = s.split(',');
    let lo = parts.next().map(|p| u64::from_str_radix(p.trim().trim_start_matches("0x"), 16).unwrap_or(0)).unwrap_or(0);
    let hi = parts.next().map(|p| u64::from_str_radix(p.trim().trim_start_matches("0x"), 16).unwrap_or(0)).unwrap_or(0);
    (hi << 32) | lo
}

fn instance_name(fullname: &str, ty: &str) -> String {
    let inst = fullname.strip_suffix(ty).unwrap_or(fullname).trim_end_matches('.');
    let inst = inst.replace("\\.", ".").replace("\\\\", "\\");
    // RAOP names are "MACADDR@Name"
    match inst.split_once('@') {
        Some((mac, name)) if mac.len() == 12 && mac.chars().all(|c| c.is_ascii_hexdigit()) => name.to_string(),
        _ => inst,
    }
}

/// Browse for `timeout` and return receivers keyed by name.
pub fn scan(timeout: Duration) -> Result<Vec<Receiver>> {
    let mdns = ServiceDaemon::new()?;
    const AIRPLAY: &str = "_airplay._tcp.local.";
    const RAOP: &str = "_raop._tcp.local.";
    let rx_a = mdns.browse(AIRPLAY)?;
    let rx_r = mdns.browse(RAOP)?;
    let mut found: BTreeMap<String, Receiver> = BTreeMap::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut any = false;
        for (rx, ty) in [(&rx_a, AIRPLAY), (&rx_r, RAOP)] {
            while let Ok(ev) = rx.try_recv() {
                any = true;
                if let ServiceEvent::ServiceResolved(info) = ev {
                    let name = instance_name(&info.fullname, ty);
                    debug!("resolved {} ({ty}) port {}", info.fullname, info.port);
                    let r = found.entry(name.clone()).or_insert_with(|| Receiver { name, ..Default::default() });
                    if r.ip.is_none() {
                        r.ip = info.get_addresses_v4().into_iter().next().map(IpAddr::V4);
                    }
                    if ty == AIRPLAY {
                        r.airplay_port = Some(info.port);
                    } else {
                        r.raop_port = Some(info.port);
                    }
                    for p in info.get_properties().iter() {
                        // AirPlay TXT wins over RAOP TXT for shared keys
                        if ty == AIRPLAY || !r.txt.contains_key(p.key()) {
                            r.txt.insert(p.key().to_string(), p.val_str().to_string());
                        }
                    }
                }
            }
        }
        if !any {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = mdns.shutdown();
    Ok(found.into_values().collect())
}

/// Resolve a receiver's IPv4 address by its AirPlay name.
pub fn resolve_name(name: &str, timeout: Duration) -> Result<Option<IpAddr>> {
    Ok(scan(timeout)?.into_iter().find(|r| r.name.eq_ignore_ascii_case(name)).and_then(|r| r.ip))
}

/// `GET /info` over RTSP (what pyatv does before pairing). Read-only.
pub fn get_info(ip: IpAddr, port: u16) -> Result<plist::Value> {
    let mut rtsp = RtspSession::connect(SocketAddr::new(ip, port), &ip.to_string())?;
    let resp = rtsp.get_info()?.ensure_ok("GET /info")?;
    let v = resp.plist()?;
    rtsp.conn.shutdown();
    Ok(v)
}

fn is_scalar(v: &plist::Value) -> bool {
    matches!(v, plist::Value::Integer(_) | plist::Value::String(_) | plist::Value::Boolean(_) | plist::Value::Real(_))
}

fn inline(a: &[plist::Value]) -> String {
    a.iter()
        .map(|v| {
            let mut s = String::new();
            format_plist(v, 0, &mut s);
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Indented, human-readable plist dump (data shown as hex, long data truncated).
pub fn format_plist(v: &plist::Value, indent: usize, out: &mut String) {
    use plist::Value as V;
    let pad = "  ".repeat(indent);
    match v {
        V::Dictionary(d) => {
            for (k, val) in d {
                match val {
                    V::Array(a) if a.iter().all(is_scalar) => {
                        out.push_str(&format!("{pad}{k}: [{}]
", inline(a)));
                    }
                    V::Dictionary(_) | V::Array(_) => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        format_plist(val, indent + 1, out);
                    }
                    _ => {
                        out.push_str(&format!("{pad}{k}: "));
                        format_plist(val, 0, out);
                    }
                }
            }
        }
        V::Array(a) => {
            for (i, val) in a.iter().enumerate() {
                out.push_str(&format!("{pad}- [{i}]\n"));
                format_plist(val, indent + 1, out);
            }
        }
        V::Data(b) => {
            let shown = &b[..b.len().min(48)];
            out.push_str(&format!("<{} bytes> {}{}\n", b.len(), hex::encode(shown), if b.len() > 48 { "..." } else { "" }));
        }
        V::String(s) => out.push_str(&format!("{s}\n")),
        V::Integer(i) => {
            let shown = i.as_unsigned().map(|u| {
                if u > 0xFFFF { format!("{u} ({u:#x})") } else { u.to_string() }
            });
            out.push_str(&format!("{}\n", shown.unwrap_or_else(|| format!("{i:?}"))));
        }
        V::Real(r) => out.push_str(&format!("{r}\n")),
        V::Boolean(b) => out.push_str(&format!("{b}\n")),
        other => out.push_str(&format!("{other:?}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_and_names() {
        assert_eq!(parse_features("0x4A7FCA00,0xBC354BD0"), 0xBC354BD0_4A7FCA00);
        assert_eq!(parse_features("0x445F8A00"), 0x445F8A00);
        assert_eq!(instance_name("Living Room._airplay._tcp.local.", "_airplay._tcp.local."), "Living Room");
        assert_eq!(instance_name("AABBCCDDEEFF@Living Room (2)._raop._tcp.local.", "_raop._tcp.local."), "Living Room (2)");
    }
}
