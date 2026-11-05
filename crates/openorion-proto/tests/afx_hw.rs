//! AFX config replies read from the real Orion (fw 1.50, 2026-10-27,
//! `fixtures/afx-configs-hw.json`, gets 7/1..7/50). Checks that every
//! entry length matches the hardware stride: the device instances fill
//! exactly `instances * entry_len` bytes (the rest is zero padding), and
//! every decoded field lies inside its table range.

mod common;

use openorion_proto::decode::*;
use openorion_proto::names::*;
use openorion_proto::*;

#[test]
fn entry_lengths_match_hardware() {
    let (replies, _, _) = common::load_dump("afx-configs-hw.json");
    for &ty in AfxType::ALL {
        if !ty.on_orion() || ty == AfxType::Equalizer {
            continue;
        }
        let raw = &replies[&(7, ty.as_u8() as u32)];
        let body = &raw[16..];
        let len = afx_entry_len(ty).unwrap();
        let n = afx_max_instances(ty) as usize;
        let end = body.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        assert!(
            end <= n * len && end > (n - 1) * len,
            "{ty:?}: data ends at {end}, {n} entries of {len} B"
        );

        let get = Get::AfxConfigs(ty, 0);
        let GetReply::AfxConfigs { configs, .. } = decode_reply(&get, body).unwrap() else {
            panic!("{ty:?}: not an afx reply");
        };
        for (i, c) in configs.iter().take(n).enumerate() {
            if let AfxParams::Raw(r) = &c.params {
                for (f, v) in afx_fields(ty).unwrap().iter().zip(&r.fields) {
                    assert!(
                        (f.min..=f.max).contains(v),
                        "{ty:?}[{i}].{} = {v} outside {}..={}",
                        f.name,
                        f.min,
                        f.max
                    );
                }
            }
        }
    }
}
