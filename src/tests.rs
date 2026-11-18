//! End-to-end loopback test: two fake AirPlay 2 receivers on 127.0.0.1 (HAP
//! pair-verify server, encrypted RTSP, event channel, UDP data/control) driven by the
//! real engine. Checks that audio decrypts, that both receivers get the same RTP
//! timeline and sync mapping, that the timing server answers, and that retransmits
//! work.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

use crate::audio::tone::ToneSource;
use crate::crypto::{counter_nonce, hkdf_sha512, open_label, seal_label};
use crate::dsp::{ChannelSource, DspSettings};
use crate::engine::{self, EngineOptions, SpeakerPlan};
use crate::hap::session::HapSession;
use crate::hap::tlv8::{self, tag};
use crate::hap::{Credentials, HapCredentials};
use crate::net::http::{header, parse_request};

const ACCESSORY_ID: &[u8] = b"A1B2C3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D";
const CLIENT_ID: &[u8] = b"C1D2E3F4-0000-1111-2222-333344445555";

#[derive(Default, Debug)]
struct Observed {
    methods: Vec<String>,
    flush_rtp_info: Option<String>,
    shk: Option<Vec<u8>>,
    timing_port: Option<u16>,
    /// (seq, rtptime, first-byte-type, decrypted pcm)
    audio: Vec<(u16, u32, u8, Vec<u8>)>,
    /// raw sync packets
    syncs: Vec<Vec<u8>>,
    event_reply_ok: bool,
    retransmit_ok: bool,
    timing_reply_ok: bool,
}

struct FakeReceiver {
    port: u16,
    observed: Arc<Mutex<Observed>>,
    accessory_pub: [u8; 32],
}

fn respond(stream: &mut TcpStream, hap: &mut Option<HapSession>, proto: &str, cseq: Option<&str>, body: &[u8], ctype: &str) {
    let mut head = format!("{proto} 200 OK\r\n");
    if let Some(c) = cseq {
        head += &format!("CSeq: {c}\r\n");
    }
    if !body.is_empty() {
        head += &format!("Content-Type: {ctype}\r\nContent-Length: {}\r\n", body.len());
    }
    head += "\r\n";
    let mut raw = head.into_bytes();
    raw.extend_from_slice(body);
    let wire = match hap {
        Some(s) => s.encrypt(&raw),
        None => raw,
    };
    stream.write_all(&wire).unwrap();
}

fn plist_bytes(v: &plist::Value) -> Vec<u8> {
    let mut b = Vec::new();
    plist::to_writer_binary(&mut b, v).unwrap();
    b
}

impl FakeReceiver {
    fn start(client_ltpk: [u8; 32]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let observed = Arc::new(Mutex::new(Observed::default()));
        let accessory = SigningKey::from_bytes(&rand_bytes());
        let accessory_pub = accessory.verifying_key().to_bytes();
        let obs = observed.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            serve(&mut stream, &accessory, client_ltpk, obs);
        });
        Self { port, observed, accessory_pub }
    }
}

fn rand_bytes() -> [u8; 32] {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn serve(stream: &mut TcpStream, accessory: &SigningKey, client_ltpk: [u8; 32], obs: Arc<Mutex<Observed>>) {
    let mut hap: Option<HapSession> = None;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let eph = StaticSecret::from(rand_bytes());
    let eph_pub = XPublic::from(&eph);
    let mut shared: Option<[u8; 32]> = None;
    let mut client_eph: Option<[u8; 32]> = None;
    let mut _event_thread = None;
    loop {
        let req = loop {
            if let Some((r, used)) = parse_request(&buf).unwrap() {
                buf.drain(..used);
                break Some(r);
            }
            let n = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break None,
                Ok(n) => n,
            };
            match &mut hap {
                Some(s) => buf.extend(s.decrypt(&chunk[..n]).unwrap()),
                None => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let Some(req) = req else { return };
        let cseq = header(&req.headers, "CSeq").map(str::to_string);
        obs.lock().unwrap().methods.push(format!("{} {}", req.method, req.path));
        let proto = req.protocol.clone();
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/info") => {
                let mut d = plist::Dictionary::new();
                d.insert("name".into(), "Fake".into());
                respond(stream, &mut hap, &proto, cseq.as_deref(), &plist_bytes(&d.into()), "application/x-apple-binary-plist");
            }
            ("POST", "/pair-verify") => {
                let t = tlv8::decode(&req.body);
                match t[&tag::SEQ_NO][0] {
                    1 => {
                        let cpub: [u8; 32] = t[&tag::PUBLIC_KEY].as_slice().try_into().unwrap();
                        let s = eph.diffie_hellman(&XPublic::from(cpub));
                        shared = Some(*s.as_bytes());
                        client_eph = Some(cpub);
                        let key = hkdf_sha512("Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info", s.as_bytes());
                        let mut info = eph_pub.as_bytes().to_vec();
                        info.extend_from_slice(ACCESSORY_ID);
                        info.extend_from_slice(&cpub);
                        let sig = accessory.sign(&info).to_bytes();
                        let inner = tlv8::encode(&[(tag::IDENTIFIER, ACCESSORY_ID), (tag::SIGNATURE, &sig)]);
                        let enc = seal_label(&key, b"PV-Msg02", &inner);
                        let body = tlv8::encode(&[(tag::SEQ_NO, &[2]), (tag::PUBLIC_KEY, eph_pub.as_bytes()), (tag::ENCRYPTED_DATA, &enc)]);
                        respond(stream, &mut hap, &proto, None, &body, "application/octet-stream");
                    }
                    3 => {
                        let s = shared.unwrap();
                        let key = hkdf_sha512("Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info", &s);
                        let inner = tlv8::decode(&open_label(&key, b"PV-Msg03", &t[&tag::ENCRYPTED_DATA]).unwrap());
                        assert_eq!(inner[&tag::IDENTIFIER], CLIENT_ID);
                        let mut info = client_eph.unwrap().to_vec();
                        info.extend_from_slice(CLIENT_ID);
                        info.extend_from_slice(eph_pub.as_bytes());
                        let sig: [u8; 64] = inner[&tag::SIGNATURE].as_slice().try_into().unwrap();
                        ed25519_dalek::VerifyingKey::from_bytes(&client_ltpk)
                            .unwrap()
                            .verify(&info, &Signature::from_bytes(&sig))
                            .expect("client signature");
                        respond(stream, &mut hap, &proto, None, &tlv8::encode(&[(tag::SEQ_NO, &[4])]), "application/octet-stream");
                        // switch to encrypted control channel (server side keys swapped)
                        hap = Some(HapSession::new(
                            &hkdf_sha512("Control-Salt", "Control-Read-Encryption-Key", &s),
                            &hkdf_sha512("Control-Salt", "Control-Write-Encryption-Key", &s),
                        ));
                    }
                    _ => panic!("unexpected pair-verify state"),
                }
            }
            ("SETUP", _) => {
                assert!(hap.is_some(), "SETUP must be encrypted");
                let v = plist::Value::from_reader(std::io::Cursor::new(&req.body)).unwrap();
                let d = v.as_dictionary().unwrap();
                let s = shared.unwrap();
                if let Some(streams) = d.get("streams") {
                    let st = streams.as_array().unwrap()[0].as_dictionary().unwrap();
                    let shk = st.get("shk").unwrap().as_data().unwrap().to_vec();
                    assert_eq!(shk, hkdf_sha512("Events-Salt", "Events-Write-Encryption-Key", &s).to_vec());
                    let client_control = st.get("controlPort").unwrap().as_unsigned_integer().unwrap() as u16;
                    let data = UdpSocket::bind("127.0.0.1:0").unwrap();
                    let control = UdpSocket::bind("127.0.0.1:0").unwrap();
                    let data_port = data.local_addr().unwrap().port();
                    let control_port = control.local_addr().unwrap().port();
                    obs.lock().unwrap().shk = Some(shk.clone());
                    spawn_udp(data, control, client_control, shk, obs.clone());
                    let mut sd = plist::Dictionary::new();
                    sd.insert("type".into(), plist::Value::Integer(0x60u64.into()));
                    sd.insert("dataPort".into(), plist::Value::Integer((data_port as u64).into()));
                    sd.insert("controlPort".into(), plist::Value::Integer((control_port as u64).into()));
                    let mut top = plist::Dictionary::new();
                    top.insert("streams".into(), plist::Value::Array(vec![sd.into()]));
                    respond(stream, &mut hap, &proto, cseq.as_deref(), &plist_bytes(&top.into()), "application/x-apple-binary-plist");
                } else {
                    let timing_port = d.get("timingPort").unwrap().as_unsigned_integer().unwrap() as u16;
                    assert_eq!(d.get("timingProtocol").unwrap().as_string(), Some("NTP"));
                    obs.lock().unwrap().timing_port = Some(timing_port);
                    // timing query
                    let o2 = obs.clone();
                    std::thread::spawn(move || {
                        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
                        u.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        let mut q = [0u8; 32];
                        q[0] = 0x80;
                        q[1] = 0xD2;
                        q[24..32].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
                        u.send_to(&q, ("127.0.0.1", timing_port)).unwrap();
                        let mut r = [0u8; 64];
                        if let Ok((n, _)) = u.recv_from(&mut r) {
                            o2.lock().unwrap().timing_reply_ok = n == 32 && r[1] == 0xD3 && r[8..16] == [1, 2, 3, 4, 5, 6, 7, 8] && r[16..20] == r[24..28];
                        }
                    });
                    let ev = TcpListener::bind("127.0.0.1:0").unwrap();
                    let event_port = ev.local_addr().unwrap().port();
                    let o3 = obs.clone();
                    _event_thread = Some(std::thread::spawn(move || {
                        let (mut es, _) = ev.accept().unwrap();
                        es.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                        let mut sess = HapSession::new(
                            &hkdf_sha512("Events-Salt", "Events-Write-Encryption-Key", &s),
                            &hkdf_sha512("Events-Salt", "Events-Read-Encryption-Key", &s),
                        );
                        let msg = b"POST /command RTSP/1.0\r\nCSeq: 42\r\nContent-Length: 0\r\n\r\n";
                        es.write_all(&sess.encrypt(msg)).unwrap();
                        let mut got = Vec::new();
                        let mut c = [0u8; 1024];
                        while let Ok(n) = es.read(&mut c) {
                            if n == 0 {
                                break;
                            }
                            got.extend(sess.decrypt(&c[..n]).unwrap());
                            let text = String::from_utf8_lossy(&got).to_string();
                            if text.contains("\r\n\r\n") {
                                o3.lock().unwrap().event_reply_ok = text.starts_with("RTSP/1.0 200 OK") && text.contains("CSeq: 42");
                                break;
                            }
                        }
                        // keep the channel open until the client goes away (read
                        // timeouts just mean the client is quiet)
                        loop {
                            match es.read(&mut c) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                                Err(_) => break,
                            }
                        }
                    }));
                    let mut top = plist::Dictionary::new();
                    top.insert("eventPort".into(), plist::Value::Integer((event_port as u64).into()));
                    top.insert("timingPort".into(), plist::Value::Integer(0u64.into()));
                    respond(stream, &mut hap, &proto, cseq.as_deref(), &plist_bytes(&top.into()), "application/x-apple-binary-plist");
                }
            }
            ("FLUSH", _) => {
                obs.lock().unwrap().flush_rtp_info = header(&req.headers, "RTP-Info").map(str::to_string);
                respond(stream, &mut hap, &proto, cseq.as_deref(), b"", "");
            }
            ("TEARDOWN", _) => {
                respond(stream, &mut hap, &proto, cseq.as_deref(), b"", "");
                return;
            }
            _ => respond(stream, &mut hap, &proto, cseq.as_deref(), b"", ""),
        }
    }
}

fn spawn_udp(data: UdpSocket, control: UdpSocket, client_control: u16, shk: Vec<u8>, obs: Arc<Mutex<Observed>>) {
    let o2 = obs.clone();
    std::thread::spawn(move || {
        data.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let aead = ChaCha20Poly1305::new(Key::from_slice(&shk));
        let mut buf = [0u8; 2048];
        while let Ok(n) = data.recv(&mut buf) {
            let p = &buf[..n];
            let seq = u16::from_be_bytes([p[2], p[3]]);
            let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
            let nonce8: [u8; 8] = p[n - 8..].try_into().unwrap();
            let mut ct = p[12..n - 24].to_vec();
            let tag = Tag::clone_from_slice(&p[n - 24..n - 8]);
            let nonce = counter_nonce(u64::from_le_bytes(nonce8));
            aead.decrypt_in_place_detached(Nonce::from_slice(&nonce), &p[4..12], &mut ct, &tag)
                .expect("audio packet decrypts with shk / aad=header[4..12]");
            o2.lock().unwrap().audio.push((seq, ts, p[1], ct));
        }
    });
    std::thread::spawn(move || {
        control.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut buf = [0u8; 2048];
        let mut asked = false;
        for _ in 0..40 {
            if let Ok((n, _from)) = control.recv_from(&mut buf) {
                if buf[1] == 0xD4 {
                    obs.lock().unwrap().syncs.push(buf[..n].to_vec());
                } else if buf[1] == 0xD6 {
                    obs.lock().unwrap().retransmit_ok = n > 4 + 12;
                }
            }
            let first_seq = obs.lock().unwrap().audio.first().map(|a| a.0);
            if let (false, Some(seq)) = (asked, first_seq) {
                let mut req = [0x80, 0xD5, 0, 1, 0, 0, 0, 1];
                req[4..6].copy_from_slice(&seq.to_be_bytes());
                control.send_to(&req, ("127.0.0.1", client_control)).unwrap();
                asked = true;
            }
        }
    });
}

/// Constant +0.25 left / -0.25 right: routing and gain are readable straight off the PCM.
struct ConstSource;

impl crate::audio::Source for ConstSource {
    fn fill(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.25, -0.25]);
    }
}

fn last_samples(o: &Mutex<Observed>) -> Vec<i16> {
    let o = o.lock().unwrap();
    let pcm = &o.audio.last().expect("audio received").3;
    pcm.chunks(2).map(|c| i16::from_be_bytes([c[0], c[1]])).collect()
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn controller_drives_fake_receivers_with_live_changes() {
    use crate::config::{Config, SpeakerConfig};
    use crate::controller::{Controller, ControllerDeps, EngineState, SpeakerState};

    let client_seed = rand_bytes();
    let client_ltpk = SigningKey::from_bytes(&client_seed).verifying_key().to_bytes();
    let rx: Vec<FakeReceiver> = (0..2).map(|_| FakeReceiver::start(client_ltpk)).collect();
    let keys: std::collections::HashMap<u16, [u8; 32]> = rx.iter().map(|r| (r.port, r.accessory_pub)).collect();
    let mut cfg = Config { latency_ms: 300, auto_start: false, ..Config::default() };
    cfg.speakers = rx
        .iter()
        .zip([("fakeL", ChannelSource::Left), ("fakeR", ChannelSource::Right)])
        .map(|(r, (name, role))| SpeakerConfig { port: Some(r.port), ..SpeakerConfig::new(name, "127.0.0.1", role) })
        .collect();
    let deps = ControllerDeps {
        open_input: Box::new(|| {
            Ok((Box::new(ConstSource) as Box<dyn crate::audio::Source>, crate::audio::InputHandle::new(|| "const".into(), ())))
        }),
        credentials: Box::new(move |s, _ip| {
            Ok(Credentials::Hap(HapCredentials {
                ltpk: keys[&s.port.unwrap()].to_vec(),
                ltsk: client_seed.to_vec(),
                atv_id: ACCESSORY_ID.to_vec(),
                client_id: CLIENT_ID.to_vec(),
                apple_device_id: "0x0123456789AB".into(),
                client_name: "Stereo Pair".into(),
            }))
        }),
        config_path: None,
        volume: None,
        save_debounce: Duration::from_millis(100),
    };
    let mut ctrl = Controller::new(cfg.clone(), deps).unwrap();
    ctrl.start();
    wait_until("both speakers streaming", || {
        let st = ctrl.status();
        st.engine == EngineState::Running
            && st.speakers.iter().all(|s| s.state == SpeakerState::Streaming && s.packets > 40)
            && rx.iter().all(|r| r.observed.lock().unwrap().audio.len() > 40)
    });
    assert_eq!(ctrl.status().input.as_deref(), Some("const"));
    assert!(last_samples(&rx[0].observed).iter().all(|&s| s == 8192), "left speaker plays the left channel");
    assert!(last_samples(&rx[1].observed).iter().all(|&s| s == -8192), "right speaker plays the right channel");

    // live: speaker 0 -> right channel at -6 dB, without reconnecting
    cfg.speakers[0].role = ChannelSource::Right;
    cfg.speakers[0].gain_db = -6.0206;
    ctrl.update(cfg.clone());
    wait_until("live DSP change", || last_samples(&rx[0].observed).iter().all(|&s| (s as i32 + 4096).abs() <= 1));
    let records = |r: &FakeReceiver| r.observed.lock().unwrap().methods.iter().filter(|m| m.starts_with("RECORD")).count();
    assert_eq!(records(&rx[0]), 1, "no reconnect for a DSP change");

    // test chime is mixed into speaker 0's stream only
    ctrl.test_tone(&cfg.speakers[0].key());
    wait_until("chime audible on speaker 0", || last_samples(&rx[0].observed).iter().any(|&s| (s as i32 + 4096).abs() > 200));
    assert!(last_samples(&rx[1].observed).iter().all(|&s| s == -8192), "speaker 1 does not get the chime");

    // disabling speaker 1 tears down just that one; speaker 0 keeps streaming
    cfg.speakers[1].enabled = false;
    ctrl.update(cfg.clone());
    wait_until("speaker 1 torn down", || {
        rx[1].observed.lock().unwrap().methods.iter().any(|m| m.starts_with("TEARDOWN"))
            && ctrl.status().speakers[1].state == SpeakerState::Disabled
    });
    let n = rx[0].observed.lock().unwrap().audio.len();
    wait_until("speaker 0 still streaming", || rx[0].observed.lock().unwrap().audio.len() > n + 10);

    ctrl.stop();
    wait_until("stopped with TEARDOWN", || {
        ctrl.status().engine == EngineState::Stopped
            && rx[0].observed.lock().unwrap().methods.iter().any(|m| m.starts_with("TEARDOWN"))
    });
    assert!(ctrl.shutdown(Duration::from_secs(5)));
}

#[test]
fn two_fake_receivers_share_one_timeline() {
    let client_seed = rand_bytes();
    let client_ltpk = SigningKey::from_bytes(&client_seed).verifying_key().to_bytes();
    let rx: Vec<FakeReceiver> = (0..2).map(|_| FakeReceiver::start(client_ltpk)).collect();
    let plans: Vec<SpeakerPlan> = rx
        .iter()
        .zip([ChannelSource::Left, ChannelSource::Right])
        .enumerate()
        .map(|(i, (r, role))| {
            let creds = Credentials::Hap(HapCredentials {
                ltpk: r.accessory_pub.to_vec(),
                ltsk: client_seed.to_vec(),
                atv_id: ACCESSORY_ID.to_vec(),
                client_id: CLIENT_ID.to_vec(),
                apple_device_id: "0x0123456789AB".into(),
                client_name: "Stereo Pair".into(),
            });
            let mut p = SpeakerPlan::new(&format!("fake{i}"), IpAddr::V4(Ipv4Addr::LOCALHOST), creds, DspSettings {
                source: role,
                ..Default::default()
            });
            p.port = r.port;
            p
        })
        .collect();
    let opts = EngineOptions {
        latency_frames: 13230,
        device_volume_db: Some(-20.0),
        timing_port: 0,
        reconnect: false,
        run_for: Some(Duration::from_millis(1500)),
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        spatial: Default::default(),
    };
    // stereo source: left = tone, right = silence -> only the "left" speaker hears it
    let source = Box::new(ToneSource::new(0.5, 0.5).split(true));
    engine::run(plans, source, opts, Arc::new(AtomicBool::new(false))).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let a = rx[0].observed.lock().unwrap();
    let b = rx[1].observed.lock().unwrap();
    for o in [&*a, &*b] {
        let m: Vec<&str> = o.methods.iter().map(String::as_str).collect();
        assert!(m.starts_with(&["GET /info", "POST /pair-verify", "POST /pair-verify"]), "{m:?}");
        for needed in ["RECORD", "FLUSH", "SET_PARAMETER", "TEARDOWN"] {
            assert!(m.iter().any(|x| x.starts_with(needed)), "missing {needed}: {m:?}");
        }
        assert!(o.event_reply_ok, "event channel reply");
        assert!(o.timing_reply_ok, "timing server reply");
        assert!(o.retransmit_ok, "retransmit");
        assert!(o.audio.len() > 100, "got {} packets", o.audio.len());
        assert_eq!(o.audio[0].2, 0xE0, "first packet has marker");
        assert_eq!(o.audio[0].1, 13230, "first rtptime = latency");
        assert_eq!(o.audio[0].3.len(), 352 * 4);
        assert!(o.flush_rtp_info.as_deref().unwrap().ends_with("rtptime=13230"));
        assert_eq!(o.syncs[0][0], 0x90);
        // packets are contiguous on the timeline
        for w in o.audio.windows(2) {
            assert_eq!(w[1].0, w[0].0.wrapping_add(1));
            assert_eq!(w[1].1, w[0].1.wrapping_add(352));
        }
    }
    // same timeline: identical rtptimes and identical sync packets (except seq/ssrc which
    // sync packets do not carry)
    let ts_a: Vec<u32> = a.audio.iter().map(|x| x.1).collect();
    let ts_b: Vec<u32> = b.audio.iter().map(|x| x.1).collect();
    let common = ts_a.len().min(ts_b.len());
    assert_eq!(ts_a[..common], ts_b[..common]);
    assert_eq!(a.syncs[0], b.syncs[0], "sync packets must be identical across speakers");
    // routing: left speaker got the tone in the first half, right speaker silence then
    let loud = |pcm: &Vec<u8>| pcm.chunks(2).map(|c| i16::from_be_bytes([c[0], c[1]]).unsigned_abs()).max().unwrap();
    let a_peak = a.audio[..20].iter().map(|x| loud(&x.3)).max().unwrap();
    let b_peak = b.audio[..20].iter().map(|x| loud(&x.3)).max().unwrap();
    assert!(a_peak > 1000, "left speaker should hear first note ({a_peak})");
    assert_eq!(b_peak, 0, "right speaker silent during first note");
    assert_ne!(a.shk, b.shk, "each speaker has its own key");
}

fn hap_creds(port_keys: &std::collections::HashMap<u16, [u8; 32]>, port: u16, client_seed: &[u8; 32]) -> Credentials {
    Credentials::Hap(HapCredentials {
        ltpk: port_keys[&port].to_vec(),
        ltsk: client_seed.to_vec(),
        atv_id: ACCESSORY_ID.to_vec(),
        client_id: CLIENT_ID.to_vec(),
        apple_device_id: "0x0123456789AB".into(),
        client_name: "Stereo Pair".into(),
    })
}

/// Timeline frame -> stereo sample, from everything a receiver decrypted.
fn received_frames(o: &Mutex<Observed>, latency: u32) -> std::collections::BTreeMap<u64, [i16; 2]> {
    let o = o.lock().unwrap();
    let mut m = std::collections::BTreeMap::new();
    for (_, rtp, _, pcm) in &o.audio {
        let f0 = rtp.wrapping_sub(latency) as u64;
        for (i, c) in pcm.chunks(4).enumerate() {
            m.insert(f0 + i as u64, [i16::from_be_bytes([c[0], c[1]]), i16::from_be_bytes([c[2], c[3]])]);
        }
    }
    m
}

#[test]
fn probes_land_on_their_exact_timeline_frames() {
    use crate::dsp::f32_to_i16;
    use crate::engine::Engine;
    let client_seed = rand_bytes();
    let client_ltpk = SigningKey::from_bytes(&client_seed).verifying_key().to_bytes();
    let rx: Vec<FakeReceiver> = (0..2).map(|_| FakeReceiver::start(client_ltpk)).collect();
    let keys: std::collections::HashMap<u16, [u8; 32]> = rx.iter().map(|r| (r.port, r.accessory_pub)).collect();
    let plans: Vec<SpeakerPlan> = rx
        .iter()
        .zip([ChannelSource::Left, ChannelSource::Right])
        .enumerate()
        .map(|(i, (r, role))| {
            let creds = hap_creds(&keys, r.port, &client_seed);
            // probes bypass the speaker's delay, trim and EQ
            let dsp = DspSettings { source: role, delay_ms: 7.0 * i as f32, gain_db: -6.0, ..Default::default() };
            let mut p = SpeakerPlan::new(&format!("fake{i}"), IpAddr::V4(Ipv4Addr::LOCALHOST), creds, dsp);
            p.port = r.port;
            p
        })
        .collect();
    let lives: Vec<_> = plans.iter().map(|p| p.live.clone()).collect();
    let latency = 13230;
    let opts = EngineOptions {
        latency_frames: latency,
        device_volume_db: None,
        timing_port: 0,
        reconnect: false,
        run_for: None,
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        spatial: Default::default(),
    };
    let (mut engine, _) = Engine::start(plans, Box::new(ConstSource), opts).unwrap();
    engine.set_programme_mute(true);
    // a distinctive probe: 1000 samples of a ramp, scheduled ~0.4 s ahead, with odd
    // offsets so it straddles packet boundaries
    let probe: Arc<Vec<f32>> = Arc::new((0..1000).map(|k| (k as f32 - 500.0) / 1000.0).collect());
    let at = engine.timeline().next_packet() * 352 + 17_777;
    lives[0].schedule_probe(at, probe.clone());
    lives[1].schedule_probe(at + 4_321, probe.clone());
    wait_until("probes played", || engine.timeline().next_packet() * 352 > at + 4_321 + 2000 && !lives[1].probes_pending());
    std::thread::sleep(Duration::from_millis(100));
    engine.shutdown();
    for (r, start) in rx.iter().zip([at, at + 4_321]) {
        let got = received_frames(&r.observed, latency);
        for f in start - 400..start + 1400 {
            let want = if (start..start + 1000).contains(&f) { f32_to_i16(probe[(f - start) as usize]) } else { 0 };
            assert_eq!(got.get(&f).copied(), Some([want, want]), "frame {f} (probe at {start})");
        }
    }
}

/// Test mic: on stop, "records" the room: both receivers' decrypted output summed
/// with known integer delays and gains, plus noise, from an arbitrary start.
struct RoomMic {
    rx: Vec<Arc<Mutex<Observed>>>,
    latency: u32,
    delays: [usize; 2],
    gains: [f32; 2],
}

impl crate::calib::job::MicInput for RoomMic {
    fn start_recording(&self) {}
    fn stop_recording(&self) -> anyhow::Result<crate::calib::Recording> {
        let frames: Vec<_> = self.rx.iter().map(|o| received_frames(o, self.latency)).collect();
        let first = frames.iter().map(|m| *m.keys().next().unwrap()).max().unwrap();
        let last = frames.iter().map(|m| *m.keys().next_back().unwrap()).min().unwrap();
        let len = (last - first) as usize + 1234;
        let mut mic = vec![0.0f32; len];
        for ((m, d), g) in frames.iter().zip(self.delays).zip(self.gains) {
            for (f, s) in m.range(first..=last) {
                let i = (f - first) as usize + d + 1234; // arbitrary recording start
                if i < len {
                    mic[i] += g * s[0] as f32 / 32767.0;
                }
            }
        }
        let mut seed = 7u32;
        for v in mic.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v += ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 1e-4;
        }
        Ok(crate::calib::Recording { rate: 44100.0, channels: vec![vec![0.0; len], mic] })
    }
    fn error(&self) -> Option<String> {
        None
    }
}

#[test]
fn controller_calibration_end_to_end_through_fake_receivers() {
    use crate::calib::job::{CalibrationRequest, CalibrationState};
    use crate::config::{Config, SpeakerConfig};
    use crate::controller::{Controller, ControllerDeps, SpeakerState};

    let client_seed = rand_bytes();
    let client_ltpk = SigningKey::from_bytes(&client_seed).verifying_key().to_bytes();
    let rx: Vec<FakeReceiver> = (0..2).map(|_| FakeReceiver::start(client_ltpk)).collect();
    let keys: std::collections::HashMap<u16, [u8; 32]> = rx.iter().map(|r| (r.port, r.accessory_pub)).collect();
    let mut cfg = Config { latency_ms: 300, auto_start: false, ..Config::default() };
    cfg.speakers = rx
        .iter()
        .zip([("fakeL", ChannelSource::Left), ("fakeR", ChannelSource::Right)])
        .map(|(r, (name, role))| SpeakerConfig { port: Some(r.port), ..SpeakerConfig::new(name, "127.0.0.1", role) })
        .collect();
    let deps = ControllerDeps {
        open_input: Box::new(|| {
            Ok((Box::new(ConstSource) as Box<dyn crate::audio::Source>, crate::audio::InputHandle::new(|| "const".into(), ())))
        }),
        credentials: Box::new(move |s, _ip| Ok(hap_creds(&keys, s.port.unwrap(), &client_seed))),
        config_path: None,
        volume: None,
        save_debounce: Duration::from_millis(100),
    };
    let mut ctrl = Controller::new(cfg.clone(), deps).unwrap();
    let mic = Arc::new(RoomMic {
        rx: rx.iter().map(|r| r.observed.clone()).collect(),
        latency: 13230,
        // right speaker reaches the mic 163 samples (3.696 ms) later, 2.5 dB quieter
        delays: [60, 223],
        gains: [1.0, 10f32.powf(-2.5 / 20.0)],
    });
    let request = |mic: Arc<RoomMic>| {
        let mut r = CalibrationRequest::new(mic, -12.0, 2);
        r.sweep_seconds = 0.8;
        r.gap_seconds = 0.4;
        r.lead_seconds = 1.0;
        r
    };

    // not streaming: refused
    ctrl.calibrate(request(mic.clone()));
    wait_until("refused", || matches!(ctrl.calibration(), CalibrationState::Failed(e) if e.contains("Start streaming")));

    ctrl.start();
    wait_until("both speakers streaming", || ctrl.status().speakers.iter().all(|s| s.state == SpeakerState::Streaming && s.packets > 20));
    ctrl.calibrate(request(mic.clone()));
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    let mut first_frame = 0;
    let result = loop {
        match ctrl.calibration() {
            CalibrationState::Done(r) => break r,
            CalibrationState::Failed(e) => panic!("calibration failed: {e}"),
            CalibrationState::Measuring { first_frame: f, .. } if f > 0 => first_frame = f,
            _ => {}
        }
        assert!(std::time::Instant::now() < deadline, "calibration timed out: {:?}", ctrl.calibration());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(result.first_frame, first_frame);
    let a = &result.analysis;
    assert_eq!(a.mic_channel, 1);
    let d = a.speakers[1].arrival_ms;
    assert!((d - 163.0 / 44.1).abs() < 0.1, "delay {d} ms, issues {:?}", a.issues);
    let l = a.speakers[1].level_db - a.speakers[0].level_db;
    assert!((l + 2.5).abs() < 0.3, "level {l} dB");
    assert!(a.clock_ppm.abs() < 5.0, "same clock: {} ppm", a.clock_ppm);
    assert!(a.ok(), "{:?}", a.issues);
    let p = &result.proposal;
    assert!((p.speakers[0].delay_ms - 3.70).abs() < 0.1 && p.speakers[1].delay_ms == 0.0);
    assert!((p.speakers[0].gain_db + 2.5).abs() < 0.3 && p.speakers[1].gain_db == 0.0);
    assert!(p.speakers.iter().all(|s| s.bands.is_empty()), "digital path is flat: {:?}", p.speakers.iter().map(|s| &s.bands).collect::<Vec<_>>());

    // the programme was muted before the first sweep, and the sweeps landed exactly
    // where scheduled: speaker 0 at first_frame, speaker 1 one period later
    let got0 = received_frames(&rx[0].observed, 13230);
    let got1 = received_frames(&rx[1].observed, 13230);
    let sweep: Vec<i16> = result.schedule.sweep.render(44100.0).iter().map(|&v| crate::dsp::f32_to_i16(v as f32)).collect();
    let period = result.schedule.period_frames;
    for f in first_frame - 20_000..first_frame {
        assert_eq!(got0[&f], [0, 0], "muted before the sweep (frame {f})");
    }
    for (k, s) in sweep.iter().enumerate() {
        let k = k as u64;
        assert_eq!(got0[&(first_frame + k)], [*s, *s], "speaker 0 sweep sample {k}");
        assert_eq!(got1[&(first_frame + k)], [0, 0], "speaker 1 silent during speaker 0's sweep");
        assert_eq!(got1[&(first_frame + period + k)], [*s, *s], "speaker 1 sweep sample {k}");
    }

    // the programme comes back after the measurement
    wait_until("programme unmuted", || last_samples(&rx[0].observed).iter().all(|&s| s == 8192));
    ctrl.dismiss_calibration();
    assert!(matches!(ctrl.calibration(), CalibrationState::Idle));
    ctrl.stop();
    assert!(ctrl.shutdown(Duration::from_secs(5)));
}
