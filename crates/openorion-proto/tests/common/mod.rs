//! Shared test helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use openorion_proto::REPORT_LEN;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// "d1 05 03" or "d10503" → bytes.
pub fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(s.len().is_multiple_of(2), "odd hex: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

pub fn report(bytes: &[u8]) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[..bytes.len()].copy_from_slice(bytes);
    r
}

/// Load an `orionread.py` dump: replies keyed (ext2, ext3), plus the
/// 0x73 and 0x83 cyclic reports.
pub type Replies = BTreeMap<(u32, u32), Vec<u8>>;

pub fn load_dump(name: &str) -> (Replies, Vec<u8>, Vec<u8>) {
    let text = std::fs::read_to_string(fixture(name)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut replies = BTreeMap::new();
    for (k, val) in v["replies"].as_object().unwrap() {
        let (a, b) = k.split_once('/').unwrap();
        let bytes = val.as_str().map(hex).unwrap_or_default();
        replies.insert((a.parse().unwrap(), b.parse().unwrap()), bytes);
    }
    let cyc = |k: &str| v["cyclic"][k].as_str().map(hex).unwrap_or_default();
    (replies, cyc("0x73"), cyc("0x83"))
}
