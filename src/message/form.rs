//! Parsing of `application/x-www-form-urlencoded` bodies into JSON values,
//! with bracket keys such as `user[name]`, `tags[]` and `items[0][id]`.
//!
//! Limits keep a request from costing much more than its size: keys nest at
//! most [`MAX_DEPTH`] levels, and an array index may only overwrite an
//! existing element or append the next one, so a field adds at most one
//! element (`a[1000000]=x` is rejected rather than allocating a million).

use serde_json::{Map, Value};

/// Maximum number of bracket segments in a key.
pub(crate) const MAX_DEPTH: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormError {
    TooDeep,
    SparseIndex,
    InvalidEncoding,
}

enum Segment {
    Key(String),
    Index(usize),
    Push,
}

/// Splits `a[b][0][]` into root `a` and segments. A key with unbalanced
/// brackets is used literally.
fn parse_key(key: &str) -> Result<Option<(String, Vec<Segment>)>, FormError> {
    let (root, mut rest) = match key.find('[') {
        Some(i) if i > 0 => (&key[..i], &key[i..]),
        _ => return Ok((!key.is_empty()).then(|| (key.to_string(), Vec::new()))),
    };
    let mut segments = Vec::new();
    while let Some(after_open) = rest.strip_prefix('[') {
        let Some(close) = after_open.find(']') else {
            return Ok(Some((key.to_string(), Vec::new())));
        };
        let inner = &after_open[..close];
        segments.push(if inner.is_empty() {
            Segment::Push
        } else if let Ok(index) = inner.parse::<usize>() {
            Segment::Index(index)
        } else {
            Segment::Key(inner.to_string())
        });
        if segments.len() > MAX_DEPTH {
            return Err(FormError::TooDeep);
        }
        rest = &after_open[close + 1..];
    }
    if !rest.is_empty() {
        return Ok(Some((key.to_string(), Vec::new())));
    }
    Ok(Some((root.to_string(), segments)))
}

/// Sets `value` at `key` inside `map`. Later values for the same key win.
pub(crate) fn insert(map: &mut Map<String, Value>, key: &str, value: Value) -> Result<(), FormError> {
    let Some((root, segments)) = parse_key(key)? else {
        return Ok(());
    };
    let mut slot = map.entry(root).or_insert(Value::Null);
    for segment in segments {
        slot = match segment {
            Segment::Key(name) => {
                if !slot.is_object() {
                    *slot = Value::Object(Map::new());
                }
                slot.as_object_mut().expect("just made an object").entry(name).or_insert(Value::Null)
            }
            Segment::Index(index) => {
                if !slot.is_array() {
                    *slot = Value::Array(Vec::new());
                }
                let array = slot.as_array_mut().expect("just made an array");
                if index > array.len() {
                    return Err(FormError::SparseIndex);
                }
                if index == array.len() {
                    array.push(Value::Null);
                }
                &mut array[index]
            }
            Segment::Push => {
                if !slot.is_array() {
                    *slot = Value::Array(Vec::new());
                }
                let array = slot.as_array_mut().expect("just made an array");
                array.push(Value::Null);
                array.last_mut().expect("just pushed")
            }
        };
    }
    *slot = value;
    Ok(())
}

fn decode_component(s: &str) -> Result<String, FormError> {
    urlencoding::decode(&s.replace('+', " "))
        .map(|decoded| decoded.into_owned())
        .map_err(|_| FormError::InvalidEncoding)
}

pub(crate) fn parse_urlencoded(data: &[u8]) -> Result<Value, FormError> {
    let text = std::str::from_utf8(data).map_err(|_| FormError::InvalidEncoding)?;
    let mut map = Map::new();
    for pair in text.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        insert(&mut map, &decode_component(key)?, Value::String(decode_component(value)?))?;
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn urlencoded(body: &str) -> Result<Value, FormError> {
        parse_urlencoded(body.as_bytes())
    }

    #[test]
    fn nested_keys() {
        assert_eq!(
            urlencoded("name=Ada+L&user[role]=admin&tags[]=a&tags[]=b&items[0][id]=1&items[1][id]=2&a%5Bb%5D=c").unwrap(),
            json!({
                "name": "Ada L",
                "user": { "role": "admin" },
                "tags": ["a", "b"],
                "items": [{ "id": "1" }, { "id": "2" }],
                "a": { "b": "c" },
            })
        );
        assert_eq!(urlencoded("a=1&a=2").unwrap(), json!({ "a": "2" }));
        assert_eq!(urlencoded("flag&x=").unwrap(), json!({ "flag": "", "x": "" }));
        assert_eq!(urlencoded("odd[=1&[x]=2").unwrap(), json!({ "odd[": "1", "[x]": "2" }));
    }

    #[test]
    fn limits_are_enforced() {
        assert_eq!(urlencoded("a[1000000]=x"), Err(FormError::SparseIndex));
        assert_eq!(urlencoded("a[1]=x"), Err(FormError::SparseIndex));
        assert!(urlencoded("a[0]=x&a[1]=y&a[0]=z").is_ok());
        let deep = format!("a{}=1", "[x]".repeat(MAX_DEPTH + 1));
        assert_eq!(urlencoded(&deep), Err(FormError::TooDeep));
        let ok = format!("a{}=1", "[x]".repeat(MAX_DEPTH));
        assert!(urlencoded(&ok).is_ok());
        // Far beyond the limit, without recursing.
        let huge = format!("a{}=1", "[x]".repeat(200_000));
        assert_eq!(urlencoded(&huge), Err(FormError::TooDeep));
    }

    mod properties {
        use super::super::*;
        use proptest::prelude::*;

        fn leaves(value: &Value) -> usize {
            match value {
                Value::Object(map) => map.values().map(leaves).sum(),
                Value::Array(items) => items.iter().map(leaves).sum::<usize>().max(1),
                _ => 1,
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2000))]

            /// Arbitrary bytes never panic.
            #[test]
            fn never_panics(input in proptest::collection::vec(any::<u8>(), 0..512)) {
                let _ = parse_urlencoded(&input);
            }

            /// Bracket-heavy input can't produce more values than fields
            /// (no memory amplification).
            #[test]
            fn output_is_bounded_by_input(input in r"([a-z\[\]0-9]{0,12}=[a-z]{0,3}&){0,40}") {
                if let Ok(value) = parse_urlencoded(input.as_bytes()) {
                    let fields = input.split('&').filter(|p| !p.is_empty()).count();
                    prop_assert!(leaves(&value) <= fields.max(1), "{} fields gave {} values", fields, leaves(&value));
                }
            }
        }
    }
}
