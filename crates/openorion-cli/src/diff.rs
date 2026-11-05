//! Leaf-level diff of two states as JSON pointers (`/mixers/3/channels/0/level`).

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: String,
    pub a: Option<Value>,
    pub b: Option<Value>,
}

pub fn diff(a: &Value, b: &Value) -> Vec<Change> {
    let mut out = Vec::new();
    walk(String::new(), Some(a), Some(b), &mut out);
    out
}

fn walk(path: String, a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Change>) {
    match (a, b) {
        (Some(Value::Object(x)), Some(Value::Object(y))) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                walk(format!("{path}/{}", escape(k)), x.get(k), y.get(k), out);
            }
        }
        (Some(Value::Array(x)), Some(Value::Array(y))) => {
            for i in 0..x.len().max(y.len()) {
                walk(format!("{path}/{i}"), x.get(i), y.get(i), out);
            }
        }
        (a, b) if a != b => out.push(Change {
            path: if path.is_empty() { "/".into() } else { path },
            a: a.cloned(),
            b: b.cloned(),
        }),
        _ => {}
    }
}

fn escape(k: &str) -> String {
    k.replace('~', "~0").replace('/', "~1")
}

pub fn show(v: &Option<Value>) -> String {
    match v {
        None => "(absent)".into(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn leaves() {
        let a = json!({"m": [{"l": 1, "p": 2}], "x": true});
        let b = json!({"m": [{"l": 3, "p": 2}], "y": 1});
        let d = diff(&a, &b);
        let paths: Vec<_> = d.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["/m/0/l", "/x", "/y"]);
        assert_eq!(d[0].b, Some(json!(3)));
        assert!(diff(&a, &a).is_empty());
    }
}
