//! Applying `DaemonMsg::Diff` to a `DeviceState` (JSON-pointer patch).

use openorion_proto::DeviceState;
use serde_json::Value;

/// Apply a `DaemonMsg::Diff`: `value` at JSON pointer `path` of the state.
/// The paths the core emits for frequent edits take a direct route; any
/// other path goes through a JSON round-trip of the whole state.
pub fn apply_diff(s: &mut DeviceState, path: &str, value: &Value) -> Result<(), String> {
    fn idx(t: &str) -> Result<usize, String> {
        t.parse().map_err(|_| format!("bad index {t:?}"))
    }
    fn de<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, String> {
        T::deserialize(v).map_err(|e| e.to_string())
    }
    const OOR: &str = "index out of range";
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    match parts.as_slice() {
        _ if path.is_empty() => *s = de(value)?,
        ["status"] => s.status = de(value)?,
        ["mixers", m, "channels", c] => {
            let ch = s
                .mixers
                .get_mut(idx(m)?)
                .and_then(|m| m.channels.get_mut(idx(c).ok()?))
                .ok_or(OOR)?;
            *ch = de(value)?;
        }
        ["mixers", m, "master"] => s.mixers.get_mut(idx(m)?).ok_or(OOR)?.master = de(value)?,
        ["routing", b] => *s.routing.get_mut(idx(b)?).ok_or(OOR)? = de(value)?,
        _ => {
            let mut root = serde_json::to_value(&*s).map_err(|e| e.to_string())?;
            set_pointer(&mut root, path, value.clone())?;
            *s = de(&root)?;
        }
    }
    Ok(())
}

/// Set `path` in `root`; a missing last key of an object is inserted.
pub fn set_pointer(root: &mut Value, path: &str, value: Value) -> Result<(), String> {
    if let Some(t) = root.pointer_mut(path) {
        *t = value;
        return Ok(());
    }
    let (parent, last) = path
        .rsplit_once('/')
        .ok_or_else(|| format!("bad pointer {path:?}"))?;
    match root.pointer_mut(parent) {
        Some(Value::Object(o)) => {
            o.insert(last.replace("~1", "/").replace("~0", "~"), value);
            Ok(())
        }
        _ => Err(format!("no such path {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_pointer_inserts_missing_map_key() {
        let mut v = serde_json::json!({"a": {"b": 1}});
        set_pointer(&mut v, "/a/c~1d", serde_json::json!(2)).unwrap();
        assert_eq!(v, serde_json::json!({"a": {"b": 1, "c/d": 2}}));
        assert!(set_pointer(&mut v, "/x/y", Value::Null).is_err());
    }

    #[test]
    fn apply_diff_direct_and_fallback_paths() {
        let base = openorion_proto::decode::empty_state();
        let mut target = base.clone();
        target.mixers[3].channels[0].mute = !base.mixers[3].channels[0].mute;
        target.mixer_links[3][0] = !base.mixer_links[3][0];
        let root = serde_json::to_value(&target).unwrap();
        let mut s = base.clone();
        for p in ["/mixers/3/channels/0", "/mixer_links/3/0"] {
            apply_diff(&mut s, p, root.pointer(p).unwrap()).unwrap();
        }
        assert_eq!(s, target);
        let mut s = base.clone();
        assert!(apply_diff(&mut s, "/mixers/9/channels/0", &Value::Null).is_err());
        assert!(apply_diff(&mut s, "/nope/1/2", &Value::Null).is_err());
        assert_eq!(s, base);
    }
}
