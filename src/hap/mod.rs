//! HomeKit Accessory Protocol pairing as used by AirPlay 2 receivers.
//!
//! * `pair_setup` - one-time PIN pairing (normal HAP, method 0), result saved as
//!   [`HapCredentials`]. Ports pyatv `protocols/airplay/auth/hap.py` + `auth/hap_srp.py`.
//! * `transient_verify` - HAP transient pairing (pair-setup M1-M4 with flag 0x10 and the
//!   fixed PIN 3939); the SRP session key is the shared secret
//!   (pyatv `protocols/airplay/auth/hap_transient.py`).
//! * `pair_verify` - pair-verify with stored credentials; the X25519 secret is the
//!   shared secret.
//!
//! Either way the shared secret feeds HKDF-SHA512 to derive the control-channel,
//! event-channel and audio keys.

pub mod opack;
pub mod session;
pub mod srp;
pub mod tlv8;

use std::io::BufRead;

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

use crate::crypto::{hkdf_sha512, open_label, seal_label};
use crate::net::http::{HttpConnection, Response};
use tlv8::tag;

pub const TRANSIENT_PIN: &str = "3939";
const PAIR_USER_AGENT: &str = "AirPlay/320.20";

pub const CONTROL_SALT: &str = "Control-Salt";
pub const CONTROL_OUTPUT_INFO: &str = "Control-Write-Encryption-Key";
pub const CONTROL_INPUT_INFO: &str = "Control-Read-Encryption-Key";

/// Long-term pairing with one receiver (pyatv `HapCredentials`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HapCredentials {
    /// Receiver's long-term Ed25519 public key.
    pub ltpk: Vec<u8>,
    /// Our long-term Ed25519 secret seed (32 bytes).
    pub ltsk: Vec<u8>,
    /// Receiver's HAP identifier bytes (for the Home View: its `psi` UUID string).
    pub atv_id: Vec<u8>,
    /// Our pairing identifier bytes.
    pub client_id: Vec<u8>,
    /// Value for `X-Apple-Device-ID` (we keep sending the one used while pairing).
    pub apple_device_id: String,
    /// Value for `X-Apple-Client-Name`.
    pub client_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credentials {
    Transient,
    Hap(HapCredentials),
}

/// Identity headers sent with pairing requests. `hkp` is 3 for normal HAP, 4 for
/// transient. HKP 3 also carries the identity headers - without them the Home View
/// shows no PIN on `/pair-pin-start`.
fn pairing_headers(hkp: u8, identity: Option<(&str, &str)>) -> Vec<(String, String)> {
    let mut h = vec![
        ("User-Agent".to_string(), PAIR_USER_AGENT.to_string()),
        ("Connection".to_string(), "keep-alive".to_string()),
        ("X-Apple-HKP".to_string(), hkp.to_string()),
    ];
    if let Some((name, device_id)) = identity {
        h.push(("X-Apple-PD".to_string(), "1".to_string()));
        h.push(("X-Apple-Client-Name".to_string(), name.to_string()));
        h.push(("X-Apple-Device-ID".to_string(), device_id.to_string()));
    }
    h.push(("Content-Type".to_string(), "application/octet-stream".to_string()));
    h
}

fn post(conn: &mut HttpConnection, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Response> {
    let resp = conn.send_and_receive("POST", path, "HTTP/1.1", PAIR_USER_AGENT, None, headers, body)?;
    debug!("POST {path} -> {} {}", resp.code, resp.message);
    if !resp.is_ok() {
        bail!("POST {path} failed: {} {}", resp.code, resp.message);
    }
    Ok(resp)
}

fn post_tlv(conn: &mut HttpConnection, path: &str, headers: &[(String, String)], items: &[(u8, &[u8])]) -> Result<tlv8::Tlv> {
    let resp = post(conn, path, headers, &tlv8::encode(items))?;
    let t = tlv8::decode(&resp.body);
    debug!("{path} response TLV: {}", tlv8::describe(&t));
    if let Some(err) = t.get(&tag::ERROR) {
        let code = err.first().copied().unwrap_or(0);
        bail!("{path}: device returned HAP error {code} ({})", tlv8::error_name(code));
    }
    Ok(t)
}

fn need<'a>(t: &'a tlv8::Tlv, tg: u8, what: &str) -> Result<&'a Vec<u8>> {
    t.get(&tg).ok_or_else(|| anyhow!("device response is missing {what}"))
}

/// Shared secret from a completed verify; derives per-channel keys.
#[derive(Clone)]
pub struct Verified {
    shared: Vec<u8>,
}

impl Verified {
    pub fn keys(&self, salt: &str, output_info: &str, input_info: &str) -> ([u8; 32], [u8; 32]) {
        (hkdf_sha512(salt, output_info, &self.shared), hkdf_sha512(salt, input_info, &self.shared))
    }
}

/// Authenticate the connection and switch it to HAP encryption with the control keys
/// (pyatv `verify_connection`).
/// `advertised_pk` is the receiver's `pk` from /info or mDNS, tried as a second key for
/// the M2 signature check (diagnostic only).
pub fn verify_connection(conn: &mut HttpConnection, creds: &Credentials, advertised_pk: Option<&[u8]>) -> Result<Verified> {
    let verified = match creds {
        Credentials::Transient => transient_verify(conn)?,
        Credentials::Hap(c) => pair_verify(conn, c, advertised_pk)?,
    };
    let (out_key, in_key) = verified.keys(CONTROL_SALT, CONTROL_OUTPUT_INFO, CONTROL_INPUT_INFO);
    conn.enable_encryption(&out_key, &in_key);
    Ok(verified)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// Transient pairing: pair-setup M1-M4 with flags=0x10 and PIN 3939.
pub fn transient_verify(conn: &mut HttpConnection) -> Result<Verified> {
    let headers = pairing_headers(4, None);
    post(conn, "/pair-pin-start", &headers, b"")?;
    let m2 = post_tlv(
        conn,
        "/pair-setup",
        &headers,
        &[(tag::METHOD, &[0]), (tag::SEQ_NO, &[1]), (tag::FLAGS, &[tlv8::FLAG_TRANSIENT])],
    )
    .context("transient pair-setup M1 (a 470 here means the device needs normal pairing)")?;
    let salt = need(&m2, tag::SALT, "salt")?;
    let b_pub = need(&m2, tag::PUBLIC_KEY, "public key")?;
    let srp = srp::client_process(&random_bytes::<32>(), TRANSIENT_PIN, salt, b_pub)?;
    let m4 = post_tlv(
        conn,
        "/pair-setup",
        &headers,
        &[(tag::SEQ_NO, &[3]), (tag::PUBLIC_KEY, &srp.client_public), (tag::PROOF, &srp.client_proof)],
    )?;
    match m4.get(&tag::PROOF) {
        Some(p) if *p == srp.expected_server_proof => debug!("transient: server proof OK"),
        Some(_) => warn!("transient: server SRP proof mismatch (continuing like pyatv)"),
        None => debug!("transient: no server proof in M4"),
    }
    info!("transient pairing OK");
    Ok(Verified { shared: srp.session_key })
}

/// Pair-verify with stored long-term credentials.
pub fn pair_verify(conn: &mut HttpConnection, c: &HapCredentials, advertised_pk: Option<&[u8]>) -> Result<Verified> {
    let headers = pairing_headers(3, Some((&c.client_name, &c.apple_device_id)));
    let secret = StaticSecret::from(random_bytes::<32>());
    let our_pub = XPublic::from(&secret);

    let m2 = post_tlv(conn, "/pair-verify", &headers, &[(tag::SEQ_NO, &[1]), (tag::PUBLIC_KEY, our_pub.as_bytes())])?;
    let dev_pub_bytes: [u8; 32] = need(&m2, tag::PUBLIC_KEY, "public key")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("device X25519 key has wrong length"))?;
    let encrypted = need(&m2, tag::ENCRYPTED_DATA, "encrypted data")?;
    let shared = secret.diffie_hellman(&XPublic::from(dev_pub_bytes));
    let session_key = hkdf_sha512("Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info", shared.as_bytes());

    let inner = tlv8::decode(&open_label(&session_key, b"PV-Msg02", encrypted).context("pair-verify M2 decrypt")?);
    let identifier = need(&inner, tag::IDENTIFIER, "identifier")?;
    let signature = need(&inner, tag::SIGNATURE, "signature")?;
    if *identifier != c.atv_id {
        bail!(
            "pair-verify: device identifier {:?} does not match stored {:?}",
            String::from_utf8_lossy(identifier),
            String::from_utf8_lossy(&c.atv_id)
        );
    }
    // Device signs (its eph pub | its identifier | our eph pub) with its long-term key.
    let mut info = dev_pub_bytes.to_vec();
    info.extend_from_slice(identifier);
    info.extend_from_slice(our_pub.as_bytes());
    // The patched pyatv used in the spike skipped this check; keep it non-fatal.
    if verify_sig(&c.ltpk, &info, signature).is_ok() {
        debug!("pair-verify: device signature OK (stored ltpk)");
    } else if advertised_pk.is_some_and(|pk| verify_sig(pk, &info, signature).is_ok()) {
        info!("pair-verify: device signed with its advertised pk, not the stored ltpk");
    } else {
        warn!("pair-verify: device signature did not verify with stored ltpk or advertised pk; continuing");
    }

    let mut our_info = our_pub.as_bytes().to_vec();
    our_info.extend_from_slice(&c.client_id);
    our_info.extend_from_slice(&dev_pub_bytes);
    let seed: [u8; 32] = c.ltsk.as_slice().try_into().map_err(|_| anyhow!("ltsk must be 32 bytes"))?;
    let sig = SigningKey::from_bytes(&seed).sign(&our_info);
    let m3_inner = tlv8::encode(&[(tag::IDENTIFIER, &c.client_id), (tag::SIGNATURE, &sig.to_bytes())]);
    let m3 = seal_label(&session_key, b"PV-Msg03", &m3_inner);
    post_tlv(conn, "/pair-verify", &headers, &[(tag::SEQ_NO, &[3]), (tag::ENCRYPTED_DATA, &m3)])
        .context("pair-verify M3 rejected (stale credentials? re-run `pair`)")?;
    info!("pair-verify OK");
    Ok(Verified { shared: shared.as_bytes().to_vec() })
}

fn verify_sig(public: &[u8], msg: &[u8], sig: &[u8]) -> Result<()> {
    let pk: [u8; 32] = public.try_into().map_err(|_| anyhow!("public key length"))?;
    let sig: [u8; 64] = sig.try_into().map_err(|_| anyhow!("signature length"))?;
    VerifyingKey::from_bytes(&pk)?.verify(msg, &Signature::from_bytes(&sig))?;
    Ok(())
}

/// Generate an `X-Apple-Device-ID` value like `0x112233445566`.
pub fn new_apple_device_id() -> String {
    format!("0x{}", hex::encode_upper(random_bytes::<6>()))
}

/// Normal HAP pair-setup with a PIN shown on the device. `read_pin` is called after
/// `/pair-pin-start` (the device then shows the code).
pub fn pair_setup(
    conn: &mut HttpConnection,
    client_name: &str,
    apple_device_id: &str,
    read_pin: impl FnOnce() -> Result<String>,
) -> Result<HapCredentials> {
    let headers = pairing_headers(3, Some((client_name, apple_device_id)));
    post(conn, "/pair-pin-start", &headers, b"")?;
    let m2 = post_tlv(conn, "/pair-setup", &headers, &[(tag::METHOD, &[0]), (tag::SEQ_NO, &[1])])?;
    let salt = need(&m2, tag::SALT, "salt")?.clone();
    let b_pub = need(&m2, tag::PUBLIC_KEY, "public key")?.clone();

    let pin = read_pin()?;
    let pin = pin.trim();
    if pin.is_empty() {
        bail!("empty PIN");
    }

    let seed = random_bytes::<32>();
    let signing = SigningKey::from_bytes(&seed);
    let our_ltpk = signing.verifying_key().to_bytes();
    // pyatv uses the Ed25519 seed as the SRP private value; any 32 random bytes work.
    let srp = srp::client_process(&random_bytes::<32>(), pin, &salt, &b_pub)?;
    let m4 = post_tlv(
        conn,
        "/pair-setup",
        &headers,
        &[(tag::SEQ_NO, &[3]), (tag::PUBLIC_KEY, &srp.client_public), (tag::PROOF, &srp.client_proof)],
    )
    .context("pair-setup M3 rejected (wrong PIN?)")?;
    match m4.get(&tag::PROOF) {
        Some(p) if *p == srp.expected_server_proof => debug!("pair-setup: server proof OK"),
        Some(_) => bail!("pair-setup: server SRP proof mismatch"),
        None => warn!("pair-setup: M4 has no server proof"),
    }

    let k = &srp.session_key;
    let controller_x = hkdf_sha512("Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info", k);
    let enc_key = hkdf_sha512("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", k);
    let pairing_id = uuid::Uuid::new_v4().to_string().into_bytes();
    let mut device_info = controller_x.to_vec();
    device_info.extend_from_slice(&pairing_id);
    device_info.extend_from_slice(&our_ltpk);
    let sig = signing.sign(&device_info).to_bytes();
    let name = opack::string_dict(&[("name", client_name)]);
    let m5_inner = tlv8::encode(&[
        (tag::IDENTIFIER, &pairing_id),
        (tag::PUBLIC_KEY, &our_ltpk),
        (tag::SIGNATURE, &sig),
        (tag::NAME, &name),
    ]);
    let m5 = seal_label(&enc_key, b"PS-Msg05", &m5_inner);
    let m6 = post_tlv(conn, "/pair-setup", &headers, &[(tag::SEQ_NO, &[5]), (tag::ENCRYPTED_DATA, &m5)])?;
    let inner = tlv8::decode(&open_label(&enc_key, b"PS-Msg06", need(&m6, tag::ENCRYPTED_DATA, "encrypted data")?)?);
    let atv_id = need(&inner, tag::IDENTIFIER, "device identifier")?.clone();
    let atv_ltpk = need(&inner, tag::PUBLIC_KEY, "device public key")?.clone();
    let atv_sig = need(&inner, tag::SIGNATURE, "device signature")?;
    let accessory_x = hkdf_sha512("Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info", k);
    let mut acc_info = accessory_x.to_vec();
    acc_info.extend_from_slice(&atv_id);
    acc_info.extend_from_slice(&atv_ltpk);
    match verify_sig(&atv_ltpk, &acc_info, atv_sig) {
        Ok(()) => debug!("pair-setup: device signature OK"),
        Err(e) => warn!("pair-setup: device M6 signature check failed ({e}); saving anyway"),
    }
    info!("paired; device identifier {}", String::from_utf8_lossy(&atv_id));
    Ok(HapCredentials {
        ltpk: atv_ltpk,
        ltsk: seed.to_vec(),
        atv_id,
        client_id: pairing_id,
        apple_device_id: apple_device_id.to_string(),
        client_name: client_name.to_string(),
    })
}

/// Read a PIN line from stdin (kept as a string: it can start with 0).
pub fn read_pin_from_stdin() -> Result<String> {
    eprint!("Enter the PIN shown on the speaker: ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    #[test]
    fn ed25519_and_x25519_match_python_cryptography() {
        let v = vectors();
        let e = &v["ed25519"];
        let seed: [u8; 32] = unhex(&e["seed"]).try_into().unwrap();
        let sk = SigningKey::from_bytes(&seed);
        assert_eq!(sk.verifying_key().to_bytes().to_vec(), unhex(&e["public"]));
        assert_eq!(sk.sign(&unhex(&e["msg"])).to_bytes().to_vec(), unhex(&e["sig"]));
        verify_sig(&unhex(&e["public"]), &unhex(&e["msg"]), &unhex(&e["sig"])).unwrap();

        let x = &v["x25519"];
        let a: [u8; 32] = unhex(&x["a_priv"]).try_into().unwrap();
        let b: [u8; 32] = unhex(&x["b_pub"]).try_into().unwrap();
        let shared = StaticSecret::from(a).diffie_hellman(&XPublic::from(b));
        assert_eq!(shared.as_bytes().to_vec(), unhex(&x["shared"]));
    }

    #[test]
    fn pairing_header_order() {
        let h = pairing_headers(3, Some(("Stereo Pair", "0x112233445566")));
        let names: Vec<_> = h.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            ["User-Agent", "Connection", "X-Apple-HKP", "X-Apple-PD", "X-Apple-Client-Name", "X-Apple-Device-ID", "Content-Type"]
        );
        assert_eq!(pairing_headers(4, None)[2].1, "4");
    }
}
