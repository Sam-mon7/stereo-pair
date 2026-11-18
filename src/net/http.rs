//! Minimal blocking HTTP/RTSP message layer (pyatv `support/http.py`), with optional
//! HAP transport encryption once pair-verify has completed.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, trace};

use crate::hap::session::HapSession;

pub type Headers = Vec<(String, String)>;

#[derive(Debug, Clone)]
pub struct Response {
    pub protocol: String,
    pub code: u16,
    pub message: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub protocol: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

pub fn header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
    pub fn ensure_ok(self, what: &str) -> Result<Self> {
        if self.is_ok() {
            Ok(self)
        } else {
            bail!("{what} failed: {} {} {}", self.protocol, self.code, self.message)
        }
    }
    pub fn plist(&self) -> Result<plist::Value> {
        plist::Value::from_reader(std::io::Cursor::new(&self.body)).context("response body is not a plist")
    }
}

/// Format a request exactly like pyatv `_format_message`: request line, User-Agent
/// (unless given in `headers`), Content-Type (if given), Content-Length (if there is a
/// body), then the headers in order.
pub fn format_request(
    method: &str,
    uri: &str,
    protocol: &str,
    user_agent: &str,
    content_type: Option<&str>,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut msg = format!("{method} {uri} {protocol}");
    if header(&headers.to_vec(), "User-Agent").is_none() {
        msg += &format!("\r\nUser-Agent: {user_agent}");
    }
    if let Some(ct) = content_type {
        msg += &format!("\r\nContent-Type: {ct}");
    }
    if !body.is_empty() {
        msg += &format!("\r\nContent-Length: {}", body.len());
    }
    for (k, v) in headers {
        msg += &format!("\r\n{k}: {v}");
    }
    msg += "\r\n\r\n";
    let mut out = msg.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Format a response like pyatv `format_response`.
pub fn format_response(protocol: &str, code: u16, message: &str, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{protocol} {code} {message}\r\n");
    if header(&headers.to_vec(), "Server").is_none() {
        out += "Server: stereo-pair\r\n";
    }
    for (k, v) in headers {
        out += &format!("{k}: {v}\r\n");
    }
    if !body.is_empty() {
        out += &format!("Content-Length: {}\r\n", body.len());
    }
    let mut b = out.into_bytes();
    b.extend_from_slice(b"\r\n");
    b.extend_from_slice(body);
    b
}

/// Split one complete message off `buf`: (first line, headers, body, bytes consumed).
/// (first line, headers, body, bytes consumed)
type Parsed = (String, Headers, Vec<u8>, usize);

fn parse_message(buf: &[u8]) -> Result<Option<Parsed>> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..end]).context("non-UTF8 header")?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':').ok_or_else(|| anyhow!("bad header line {line:?}"))?;
        headers.push((k.trim().to_string(), v.trim_start().to_string()));
    }
    let len: usize = header(&headers, "Content-Length").map(|v| v.trim().parse()).transpose()?.unwrap_or(0);
    let body_start = end + 4;
    if buf.len() < body_start + len {
        return Ok(None);
    }
    Ok(Some((first, headers, buf[body_start..body_start + len].to_vec(), body_start + len)))
}

pub fn parse_response(buf: &[u8]) -> Result<Option<(Response, usize)>> {
    let Some((first, headers, body, used)) = parse_message(buf)? else { return Ok(None) };
    // e.g. "RTSP/1.0 200 OK"
    let mut parts = first.splitn(3, ' ');
    let protocol = parts.next().unwrap_or_default().to_string();
    let code: u16 = parts.next().ok_or_else(|| anyhow!("bad status line {first:?}"))?.parse()
        .with_context(|| format!("bad status line {first:?}"))?;
    let message = parts.next().unwrap_or_default().to_string();
    if !protocol.contains('/') {
        bail!("bad status line {first:?}");
    }
    Ok(Some((Response { protocol, code, message, headers, body }, used)))
}

pub fn parse_request(buf: &[u8]) -> Result<Option<(Request, usize)>> {
    let Some((first, headers, body, used)) = parse_message(buf)? else { return Ok(None) };
    let mut parts = first.splitn(3, ' ');
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().ok_or_else(|| anyhow!("bad request line {first:?}"))?.to_string();
    let protocol = parts.next().ok_or_else(|| anyhow!("bad request line {first:?}"))?.to_string();
    Ok(Some((Request { method, path, protocol, headers, body }, used)))
}

/// A persistent TCP connection carrying HTTP/1.1 and RTSP/1.0 messages.
pub struct HttpConnection {
    stream: TcpStream,
    session: Option<HapSession>,
    buf: Vec<u8>,
    pub local_ip: IpAddr,
}

impl HttpConnection {
    pub fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("connect to {addr}"))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let local_ip = stream.local_addr()?.ip();
        debug!("connected to {addr} from {local_ip}");
        Ok(Self { stream, session: None, buf: Vec::new(), local_ip })
    }

    pub fn enable_encryption(&mut self, output_key: &[u8], input_key: &[u8]) {
        self.session = Some(HapSession::new(output_key, input_key));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_and_receive(
        &mut self,
        method: &str,
        uri: &str,
        protocol: &str,
        user_agent: &str,
        content_type: Option<&str>,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response> {
        let raw = format_request(method, uri, protocol, user_agent, content_type, headers, body);
        trace!("send {} bytes: {}", raw.len(), String::from_utf8_lossy(&raw[..raw.len() - body.len()]));
        let wire = match &mut self.session {
            Some(s) => s.encrypt(&raw),
            None => raw,
        };
        self.stream.write_all(&wire).with_context(|| format!("send {method} {uri}"))?;
        self.read_response().with_context(|| format!("no valid response to {method} {uri}"))
    }

    fn read_response(&mut self) -> Result<Response> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some((resp, used)) = parse_response(&self.buf)? {
                self.buf.drain(..used);
                return Ok(resp);
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                bail!("connection closed by remote");
            }
            match &mut self.session {
                Some(s) => {
                    let plain = s.decrypt(&chunk[..n])?;
                    self.buf.extend_from_slice(&plain);
                }
                None => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_matches_pyatv_format_message() {
        let raw = format_request(
            "POST",
            "/pair-verify",
            "HTTP/1.1",
            "ignored",
            None,
            &[("User-Agent".into(), "AirPlay/320.20".into()), ("X-Apple-HKP".into(), "3".into())],
            b"ab",
        );
        assert_eq!(
            raw,
            b"POST /pair-verify HTTP/1.1\r\nContent-Length: 2\r\nUser-Agent: AirPlay/320.20\r\nX-Apple-HKP: 3\r\n\r\nab"
        );
        let raw = format_request("GET", "/info", "RTSP/1.0", "AirPlay/550.10", None, &[("CSeq".into(), "0".into())], b"");
        assert_eq!(raw, b"GET /info RTSP/1.0\r\nUser-Agent: AirPlay/550.10\r\nCSeq: 0\r\n\r\n");
    }

    #[test]
    fn parses_partial_and_pipelined_messages() {
        let msg = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 3\r\n\r\nabcRTSP/1.0 470 Connection Authorization Required\r\n\r\n";
        assert!(parse_response(&msg[..20]).unwrap().is_none());
        assert!(parse_response(&msg[..45]).unwrap().is_none());
        let (r, used) = parse_response(msg).unwrap().unwrap();
        assert_eq!((r.code, r.body.as_slice(), r.header("cseq")), (200, &b"abc"[..], Some("3")));
        let (r2, _) = parse_response(&msg[used..]).unwrap().unwrap();
        assert_eq!(r2.code, 470);
        assert_eq!(r2.message, "Connection Authorization Required");
        let (q, _) = parse_request(b"POST /command RTSP/1.0\r\nCSeq: 9\r\n\r\n").unwrap().unwrap();
        assert_eq!((q.method.as_str(), q.path.as_str(), q.protocol.as_str()), ("POST", "/command", "RTSP/1.0"));
    }
}
