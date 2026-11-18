//! RTSP session on top of [`HttpConnection`] (pyatv `support/rtsp.py`).

use std::net::SocketAddr;

use anyhow::Result;
use rand::Rng;
use tracing::debug;

use crate::net::http::{HttpConnection, Response};

pub const USER_AGENT: &str = "AirPlay/550.10";
pub const BPLIST: &str = "application/x-apple-binary-plist";

pub struct RtspSession {
    pub conn: HttpConnection,
    cseq: u32,
    /// Random 32-bit session id: used in the request URI, as `streamConnectionID` and as
    /// the RTP SSRC (pyatv does the same).
    pub session_id: u32,
    dacp_id: String,
    active_remote: u32,
    label: String,
}

impl RtspSession {
    pub fn connect(addr: SocketAddr, label: &str) -> Result<Self> {
        let conn = HttpConnection::connect(addr)?;
        let mut rng = rand::thread_rng();
        Ok(Self {
            conn,
            cseq: 0,
            session_id: rng.r#gen(),
            dacp_id: format!("{:X}", rng.r#gen::<u64>()),
            active_remote: rng.r#gen(),
            label: label.to_string(),
        })
    }

    pub fn uri(&self) -> String {
        format!("rtsp://{}/{}", self.conn.local_ip, self.session_id)
    }

    /// Send one request with the standard RTSP headers. Non-2xx responses are
    /// returned, not turned into errors; use `Response::ensure_ok`.
    pub fn exchange(
        &mut self,
        method: &str,
        uri: Option<&str>,
        content_type: Option<&str>,
        extra_headers: &[(&str, String)],
        body: &[u8],
        protocol: &str,
    ) -> Result<Response> {
        let cseq = self.cseq;
        self.cseq += 1;
        let mut headers: Vec<(String, String)> = vec![
            ("CSeq".into(), cseq.to_string()),
            ("DACP-ID".into(), self.dacp_id.clone()),
            ("Active-Remote".into(), self.active_remote.to_string()),
            ("Client-Instance".into(), self.dacp_id.clone()),
        ];
        for (k, v) in extra_headers {
            if let Some(slot) = headers.iter_mut().find(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                slot.1 = v.clone();
            } else {
                headers.push((k.to_string(), v.clone()));
            }
        }
        let uri = uri.map(str::to_string).unwrap_or_else(|| self.uri());
        debug!(target: "rtsp", "[{}] > {method} {uri} (CSeq {cseq}, {} byte body)", self.label, body.len());
        let resp = self.conn.send_and_receive(method, &uri, protocol, USER_AGENT, content_type, &headers, body)?;
        debug!(target: "rtsp", "[{}] < {} {} ({} byte body)", self.label, resp.code, resp.message, resp.body.len());
        tracing::trace!(target: "rtsp", "[{}] < headers {:?}", self.label, resp.headers);
        Ok(resp)
    }

    pub fn request(&mut self, method: &str, extra: &[(&str, String)]) -> Result<Response> {
        self.exchange(method, None, None, extra, b"", "RTSP/1.0")
    }

    /// SETUP with a binary-plist body.
    pub fn setup_plist(&mut self, body: &plist::Value) -> Result<Response> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, body)?;
        // pyatv puts the plist Content-Type in the header dict (after Client-Instance).
        self.exchange("SETUP", None, None, &[("Content-Type", BPLIST.to_string())], &buf, "RTSP/1.0")
    }

    pub fn get_info(&mut self) -> Result<Response> {
        self.exchange("GET", Some("/info"), None, &[], b"", "RTSP/1.0")
    }

    pub fn feedback(&mut self) -> Result<Response> {
        self.exchange("POST", Some("/feedback"), None, &[], b"", "RTSP/1.0")
    }

    pub fn set_parameter(&mut self, name: &str, value: &str) -> Result<Response> {
        let body = format!("{name}: {value}");
        self.exchange("SET_PARAMETER", None, Some("text/parameters"), &[], body.as_bytes(), "RTSP/1.0")
    }
}
