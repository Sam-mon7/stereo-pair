//! SETUP request bodies (pyatv `protocols/raop/protocols/airplayv2.py`).

use plist::{Dictionary, Value};

use crate::raop::timeline::{LATENCY_MAX_FRAMES, LATENCY_MIN_FRAMES, SAMPLE_RATE};

fn dict(items: Vec<(&str, Value)>) -> Value {
    let mut d = Dictionary::new();
    for (k, v) in items {
        d.insert(k.to_string(), v);
    }
    Value::Dictionary(d)
}

/// First SETUP: session with NTP timing pointing at our one timing server.
pub fn session_body(session_uuid: &str, timing_port: u16) -> Value {
    dict(vec![
        ("deviceID", "AA:BB:CC:DD:EE:FF".into()),
        ("sessionUUID", session_uuid.into()),
        ("timingPort", Value::Integer((timing_port as u64).into())),
        ("timingProtocol", "NTP".into()),
        ("isMultiSelectAirPlay", true.into()),
        ("groupContainsGroupLeader", false.into()),
        ("macAddress", "AA:BB:CC:DD:EE:FF".into()),
        ("model", "iPhone14,3".into()),
        ("name", "pyatv".into()),
        ("osBuildVersion", "20F66".into()),
        ("osName", "iPhone OS".into()),
        ("osVersion", "16.5".into()),
        ("senderSupportsRelay", false.into()),
        ("sourceVersion", "690.7.1".into()),
        ("statsCollectionEnabled", false.into()),
    ])
}

/// Second SETUP: one realtime (type 0x60) raw-PCM stream, 352 frames/packet, 44.1 kHz.
pub fn stream_body(control_port: u16, shk: &[u8; 32], stream_connection_id: u32) -> Value {
    let stream = dict(vec![
        ("audioFormat", Value::Integer(0x800u64.into())),
        ("audioMode", "default".into()),
        ("controlPort", Value::Integer((control_port as u64).into())),
        ("ct", Value::Integer(1u64.into())),
        ("isMedia", true.into()),
        ("latencyMax", Value::Integer((LATENCY_MAX_FRAMES as u64).into())),
        ("latencyMin", Value::Integer((LATENCY_MIN_FRAMES as u64).into())),
        ("shk", Value::Data(shk.to_vec())),
        ("spf", Value::Integer(352u64.into())),
        ("sr", Value::Integer((SAMPLE_RATE as u64).into())),
        ("type", Value::Integer(0x60u64.into())),
        ("supportsDynamicStreamID", false.into()),
        ("streamConnectionID", Value::Integer((stream_connection_id as u64).into())),
    ]);
    dict(vec![("streams", Value::Array(vec![stream]))])
}

/// Integer field of a plist dictionary.
pub fn get_u64(v: &Value, key: &str) -> Option<u64> {
    v.as_dictionary()?.get(key)?.as_unsigned_integer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testvec::{unhex, vectors};

    fn decode(bytes: &[u8]) -> Value {
        Value::from_reader(std::io::Cursor::new(bytes)).unwrap()
    }

    #[test]
    fn setup_bodies_match_pyatv_dicts() {
        let v = vectors();
        let p = &v["setup_plists"];
        let ours = session_body("11111111-2222-3333-4444-555555555555", 50123);
        assert_eq!(ours, decode(&unhex(&p["session"])));

        let shk: [u8; 32] = core::array::from_fn(|i| 50 + i as u8);
        let ours = stream_body(50124, &shk, 3735928559);
        // pyatv announces latencyMin 11025 (250 ms); we deliberately announce
        // LATENCY_MIN_FRAMES (4410 = 100 ms) so low-latency settings are accepted.
        let mut expected = decode(&unhex(&p["stream"]));
        let stream = expected.as_dictionary_mut().unwrap().get_mut("streams").unwrap().as_array_mut().unwrap()[0].as_dictionary_mut().unwrap();
        assert_eq!(stream.get("latencyMin").and_then(Value::as_unsigned_integer), Some(11025));
        stream.insert("latencyMin".into(), Value::Integer((crate::raop::timeline::LATENCY_MIN_FRAMES as u64).into()));
        assert_eq!(ours, expected);

        // and our binary encoding decodes back to the same thing
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &ours).unwrap();
        assert_eq!(decode(&buf), ours);
    }
}
