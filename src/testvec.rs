//! Test-only access to `tests/vectors.json` (generated from pyatv by
//! `tools/gen_vectors.py`).

use serde_json::Value;

pub fn vectors() -> Value {
    let text = include_str!("../tests/vectors.json");
    serde_json::from_str(text).expect("vectors.json parses")
}

pub fn unhex(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().expect("hex string")).expect("valid hex")
}

/// A fresh, empty directory under %TEMP% (never the real %APPDATA% config).
pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("stereo-pair-test-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
