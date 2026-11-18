# /// script
# requires-python = ">=3.13,<3.14"
# dependencies = ["pyatv", "srptools<1.0"]
# ///
"""Generate reference test vectors from pyatv / srptools for the Rust unit tests.

Run from the repo root:
    uv run --python 3.13.15 --with pyatv --with "srptools<1.0" python tools/gen_vectors.py

Writes tests/vectors.json. Everything is deterministic (fixed keys/nonces), no network.
"""

import array
import binascii
import hashlib
import json
import os
import plistlib
import sys

from srptools import SRPClientSession, SRPContext, SRPServerSession, constants
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives import serialization

from pyatv.auth.hap_srp import hkdf_expand
from pyatv.auth.hap_tlv8 import write_tlv, read_tlv
from pyatv.auth.hap_session import HAPSession
from pyatv.support.chacha20 import Chacha20Cipher8byteNonce, Chacha20Cipher
from pyatv.support import opack
from pyatv.protocols.raop import timing
from pyatv.protocols.raop.packets import AudioPacketHeader, SyncPacket, TimingPacket
from pyatv.protocols.raop.protocols.airplayv2 import AirPlayV2
from pyatv.protocols.raop.protocols import StreamContext
import pyatv.protocols.raop.audio_source as audio_source

h = lambda b: binascii.hexlify(b).decode()
out = {}

# --- SRP (HAP style, 3072-bit, SHA-512) ---------------------------------------
client_priv = bytes(range(1, 33))  # pyatv uses the Ed25519 seed as SRP private 'a'
salt = bytes.fromhex("00a1b2c3d4e5f60718293a4b5c6d7e8f")  # leading zero on purpose
pin = "0427"
ctx = SRPContext("Pair-Setup", pin, prime=constants.PRIME_3072,
                 generator=constants.PRIME_3072_GEN, hash_func=hashlib.sha512)
x = ctx.get_common_password_hash(salt)
verifier = ctx.get_common_password_verifier(x)
srv_ctx = SRPContext("Pair-Setup", pin, prime=constants.PRIME_3072,
                     generator=constants.PRIME_3072_GEN, hash_func=hashlib.sha512)
server = SRPServerSession(srv_ctx, hex(verifier)[2:], private="%x" % 0x1234567890ABCDEF1234567890ABCDEF)
B = binascii.unhexlify(server.public)
client = SRPClientSession(ctx, binascii.hexlify(client_priv).decode())
client.process(server.public, binascii.hexlify(salt).decode())
server.process(client.public, binascii.hexlify(salt).decode())
assert server.key == client.key
out["srp"] = {
    "pin": pin,
    "salt": h(salt),
    "client_private": h(client_priv),
    "server_public": h(B),
    "client_public": h(binascii.unhexlify(client.public)),
    "session_key": h(binascii.unhexlify(client.key)),
    "client_proof": h(binascii.unhexlify(client.key_proof)),
    "server_proof": h(binascii.unhexlify(server.key_proof_hash)),
}

# --- HKDF ---------------------------------------------------------------------
shared = bytes(range(100, 132))
out["hkdf"] = [
    {"salt": s, "info": i, "ikm": h(shared), "okm": h(hkdf_expand(s, i, shared))}
    for s, i in [
        ("Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info"),
        ("Control-Salt", "Control-Write-Encryption-Key"),
        ("Events-Salt", "Events-Write-Encryption-Key"),
        ("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info"),
    ]
]

# --- TLV8 ---------------------------------------------------------------------
tlv_in = {6: b"\x01", 3: bytes((i * 7) & 0xFF for i in range(300)), 0x13: b"\x10"}
tlv = write_tlv(tlv_in)
assert read_tlv(tlv) == tlv_in
out["tlv"] = {"encoded": h(tlv), "pubkey": h(tlv_in[3])}

# --- OPACK name -----------------------------------------------------------------
out["opack_name"] = h(opack.pack({"name": "Stereo Pair"}))

# --- ChaCha20-Poly1305 ------------------------------------------------------------
key = bytes(range(32))
c = Chacha20Cipher8byteNonce(key, key)
out["chacha"] = {
    "key": h(key),
    "pv_msg02": h(c.encrypt(b"hello pair verify", nonce=b"PV-Msg02")),
    "counter0_aad": h(c.encrypt(b"frame0", aad=b"\x06\x00")),
    "counter1_aad": h(c.encrypt(b"frame1", aad=b"\x06\x00")),
}

# HAP session framing (1024 byte blocks, little-endian length AAD)
sess = HAPSession()
sess.enable(key, bytes(reversed(key)))
plain = bytes((i * 13) & 0xFF for i in range(1500))
out["hap_session"] = {"out_key": h(key), "plain": h(plain), "encrypted": h(sess.encrypt(plain))}

# --- AirPlay 2 audio packet (header + ChaCha with aad=header[4:12] + 8 byte nonce) ---
class _T:
    def sendto(self, data):
        self.data = data
ctx2 = StreamContext()
ap = AirPlayV2(ctx2, None)
shk = bytes(range(50, 82))
ap._cipher = Chacha20Cipher8byteNonce(shk, shk)
import asyncio
pkts = []
for i in range(2):
    hdr = AudioPacketHeader.encode(0x80, 0xE0 if i == 0 else 0x60, 1000 + i, 66150 + 352 * i, 0xDEADBEEF)
    audio = bytes((j + i) & 0xFF for j in range(352 * 4))
    t = _T()
    asyncio.run(ap.send_audio_packet(t, hdr, audio))
    pkts.append({"header": h(hdr), "audio": h(audio), "packet": h(t.data)})
out["audio_packets"] = {"shk": h(shk), "packets": pkts}

# --- Timing math --------------------------------------------------------------------
ntp_samples = [0xEAF0_1234_8000_0000, 0xEC00_0000_0000_0001, 0xEB12_3456_789A_BCDE]
out["timing"] = {
    "ntp2ts": [[n, timing.ntp2ts(n, 44100)] for n in ntp_samples],
    "ts2ntp": [[t, timing.ts2ntp(t, 44100)] for t in [0, 352, 44100, timing.ntp2ts(ntp_samples[0], 44100)]],
    "ts2ms": [[t, timing.ts2ms(t, 44100)] for t in [0, 352, 44100, 66150]],
}
req = TimingPacket.encode(0x80, 0xD2, 7, 0, 1, 2, 3, 4, 0x11223344, 0x55667788)
r = TimingPacket.decode(req)
resp = TimingPacket.encode(r.proto, 0x53 | 0x80, 7, 0, r.sendtime_sec, r.sendtime_frac,
                           0xAAAA0001, 0xBBBB0002, 0xAAAA0001, 0xBBBB0002)
out["timing_packet"] = {"request": h(req), "now_sec": 0xAAAA0001, "now_frac": 0xBBBB0002, "response": h(resp)}
out["sync_packet"] = h(SyncPacket.encode(0x90, 0xD4, 0x0007, 1000, 0xE1E2E3E4, 0x01020304, 67150))

# --- SETUP plists (exact pyatv dicts, deterministic values substituted) ---------
setup1 = {
    "deviceID": "AA:BB:CC:DD:EE:FF",
    "sessionUUID": "11111111-2222-3333-4444-555555555555",
    "timingPort": 50123,
    "timingProtocol": "NTP",
    "isMultiSelectAirPlay": True,
    "groupContainsGroupLeader": False,
    "macAddress": "AA:BB:CC:DD:EE:FF",
    "model": "iPhone14,3",
    "name": "pyatv",
    "osBuildVersion": "20F66",
    "osName": "iPhone OS",
    "osVersion": "16.5",
    "senderSupportsRelay": False,
    "sourceVersion": "690.7.1",
    "statsCollectionEnabled": False,
}
setup2 = {
    "streams": [
        {
            "audioFormat": 0x800,
            "audioMode": "default",
            "controlPort": 50124,
            "ct": 1,
            "isMedia": True,
            "latencyMax": 88200,
            "latencyMin": 11025,
            "shk": shk,
            "spf": 352,
            "sr": 44100,
            "type": 0x60,
            "supportsDynamicStreamID": False,
            "streamConnectionID": 3735928559,
        }
    ]
}
out["setup_plists"] = {
    "session": h(plistlib.dumps(setup1, fmt=plistlib.FMT_BINARY)),
    "stream": h(plistlib.dumps(setup2, fmt=plistlib.FMT_BINARY)),
}

# --- PCM byte order (what pyatv puts on the wire for s16 input) -----------------
pcm = array.array("h", [1, -2, 0x1234, -32768, 32767, 0]).tobytes()  # native LE
out["pcm"] = {"native_le": h(pcm), "wire": h(audio_source._to_audio_samples(pcm))}

# --- Ed25519 / X25519 -------------------------------------------------------------
seed = bytes(range(200, 232))
sk = Ed25519PrivateKey.from_private_bytes(seed)
pk = sk.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
msg = b"pair-verify-info"
xa = X25519PrivateKey.from_private_bytes(bytes(range(10, 42)))
xb = X25519PrivateKey.from_private_bytes(bytes(range(60, 92)))
xb_pub = xb.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
out["ed25519"] = {"seed": h(seed), "public": h(pk), "msg": h(msg), "sig": h(sk.sign(msg))}
out["x25519"] = {"a_priv": h(bytes(range(10, 42))), "b_pub": h(xb_pub), "shared": h(xa.exchange(xb.public_key()))}

dest = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "vectors.json")
os.makedirs(os.path.dirname(dest), exist_ok=True)
with open(dest, "w") as f:
    json.dump(out, f, indent=1)
print("wrote", os.path.abspath(dest))
