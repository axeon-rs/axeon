//! Parsing of `application/x-www-form-urlencoded` and `multipart/form-data`
//! bodies into JSON values, with bracket keys such as `user[name]`,
//! `tags[]` and `items[0][id]`.
//!
//! Limits keep a request from costing much more than its size: keys nest at
//! most [`MAX_DEPTH`] levels, and an array index may only overwrite an
//! existing element or append the next one, so a field adds at most one
//! element (`a[1000000]=x` is rejected rather than allocating a million).

use base64::Engine;
use memchr::memmem;
use serde_json::{json, Map, Value};

/// Maximum number of bracket segments in a key.
pub(crate) const MAX_DEPTH: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormError {
    TooDeep,
    SparseIndex,
    InvalidEncoding,
    BoundaryNotFound,
    InvalidMultipart,
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

fn boundary(content_type: &str) -> Result<String, FormError> {
    let boundary = content_type
        .split(';')
        .find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim().eq_ignore_ascii_case("boundary").then(|| value.trim().trim_matches('"').to_string())
        })
        .ok_or(FormError::BoundaryNotFound)?;
    // RFC 2046: 1 to 70 characters.
    if boundary.is_empty() || boundary.len() > 70 {
        return Err(FormError::BoundaryNotFound);
    }
    Ok(boundary)
}

/// Splits a multipart body into its parts (headers and content), per RFC 2046.
fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Result<Vec<&'a [u8]>, FormError> {
    let delimiter = format!("--{}", boundary);
    let next_delimiter = format!("\r\n--{}", boundary);
    let next = memmem::Finder::new(next_delimiter.as_bytes());

    // The first delimiter starts the body or follows a preamble line.
    let first = if body.starts_with(delimiter.as_bytes()) {
        0
    } else {
        next.find(body).map(|i| i + 2).ok_or(FormError::InvalidMultipart)?
    };
    let mut cursor = first + delimiter.len();
    let mut parts = Vec::new();
    loop {
        let rest = &body[cursor..];
        if rest.starts_with(b"--") {
            return Ok(parts); // close delimiter; anything after is epilogue
        }
        // Optional transport padding, then CRLF.
        let padding = rest.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        if !rest[padding..].starts_with(b"\r\n") {
            return Err(FormError::InvalidMultipart);
        }
        cursor += padding + 2;
        let end = next.find(&body[cursor..]).ok_or(FormError::InvalidMultipart)?;
        parts.push(&body[cursor..cursor + end]);
        cursor += end + next_delimiter.len();
    }
}

/// Reads a `Content-Disposition` header's parameters, honouring quotes.
fn disposition_params(value: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut rest = value.split_once(';').map(|(_, r)| r).unwrap_or("");
    while !rest.trim().is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let after = after.trim_start();
        let (value, remaining) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], quoted[end + 1..].split_once(';').map(|(_, r)| r).unwrap_or("")),
                None => (quoted, ""),
            }
        } else {
            match after.split_once(';') {
                Some((v, r)) => (v.trim(), r),
                None => (after.trim(), ""),
            }
        };
        params.push((name.trim().to_ascii_lowercase(), value.to_string()));
        rest = remaining;
    }
    params
}

pub(crate) fn parse_multipart(content_type: &str, body: &[u8]) -> Result<Value, FormError> {
    let boundary = boundary(content_type)?;
    let mut map = Map::new();
    for part in split_parts(body, &boundary)? {
        let header_end = memmem::find(part, b"\r\n\r\n").ok_or(FormError::InvalidMultipart)?;
        let headers = std::str::from_utf8(&part[..header_end]).map_err(|_| FormError::InvalidEncoding)?;
        let content = &part[header_end + 4..];

        let mut name = None;
        let mut filename = None;
        let mut part_type = None;
        for line in headers.split("\r\n") {
            let Some((header, value)) = line.split_once(':') else { continue };
            let value = value.trim();
            if header.trim().eq_ignore_ascii_case("content-disposition") {
                for (param, param_value) in disposition_params(value) {
                    match param.as_str() {
                        "name" => name = Some(param_value),
                        "filename" => filename = Some(param_value),
                        _ => {}
                    }
                }
            } else if header.trim().eq_ignore_ascii_case("content-type") {
                part_type = Some(value.to_string());
            }
        }
        let Some(name) = name else { continue };

        let value = match filename {
            Some(filename) => json!({
                "filename": filename,
                "content": base64::engine::general_purpose::STANDARD.encode(content),
                "content_type": part_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            }),
            None => match std::str::from_utf8(content) {
                Ok(text) => Value::String(text.to_string()),
                Err(_) => continue,
            },
        };
        insert(&mut map, &name, value)?;
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const CT: &str = "multipart/form-data; boundary=XYZ";

    #[test]
    fn multipart_fields_and_files() {
        let body = "preamble\r\n--XYZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--XYZ  \r\n\
                    Content-Disposition: form-data; name=\"f\"; filename=\"x;y.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--XYZ--";
        assert_eq!(
            parse_multipart(CT, body.as_bytes()).unwrap(),
            json!({ "a": "1", "f": { "filename": "x;y.txt", "content": "aGVsbG8=", "content_type": "text/plain" } })
        );
        // Binary content containing CR/LF and dashes survives unchanged.
        let mut binary = b"--XYZ\r\nContent-Disposition: form-data; name=\"b\"; filename=\"b.bin\"\r\n\r\n".to_vec();
        binary.extend_from_slice(b"\r\n--\x00\xff\r\n");
        binary.extend_from_slice(b"\r\n--XYZ--\r\n");
        let value = parse_multipart(CT, &binary).unwrap();
        let content = base64::engine::general_purpose::STANDARD.decode(value["b"]["content"].as_str().unwrap()).unwrap();
        assert_eq!(content, b"\r\n--\x00\xff\r\n");
    }

    #[test]
    fn part_headers_cannot_override_the_field_name() {
        let body = "--XYZ\r\nContent-Disposition: form-data; name=\"real\"\r\nName: fake\r\n\r\nv\r\n--XYZ--\r\n";
        assert_eq!(parse_multipart(CT, body.as_bytes()).unwrap(), json!({ "real": "v" }));
    }

    #[test]
    fn malformed_multipart_is_an_error_not_a_panic() {
        for body in ["", "x", "--XYZ", "--XYZ\r\nx", "--XYZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--XYZ\r\nab", "--XYZjunk"] {
            assert!(parse_multipart(CT, body.as_bytes()).is_err(), "{:?}", body);
        }
        assert_eq!(parse_multipart("multipart/form-data", b"--XYZ--"), Err(FormError::BoundaryNotFound));
        assert!(parse_multipart(&format!("multipart/form-data; boundary={}", "b".repeat(71)), b"").is_err());
    }
}
