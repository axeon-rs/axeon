use crate::plugins::Plugins;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

#[derive(Eq, Hash, PartialEq, Copy, Clone, Debug)]
pub enum Method {
    GET,
    POST,
    PUT,
    DELETE,
    HEAD,
    CONNECT,
    OPTIONS,
    TRACE,
    PATCH,
}

impl Method {
    pub fn from_string(s: &str) -> Method {
        match s {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "DELETE" => Method::DELETE,
            "HEAD" => Method::HEAD,
            "CONNECT" => Method::CONNECT,
            "OPTIONS" => Method::OPTIONS,
            "TRACE" => Method::TRACE,
            "PATCH" => Method::PATCH,
            _ => Method::GET,
        }
    }
}

#[derive(Debug)]
pub struct Body {
    pub(crate) content_type: String,
    pub(crate) data: Vec<u8>,
}

#[derive(Debug)]
enum MultipartError {
    BoundaryNotFound,
    InvalidFormat,
    Utf8Error,
}

impl Body {
    pub fn new() -> Body {
        Body {
            content_type: String::new(),
            data: Vec::new(),
        }
    }

    pub fn from_string(s: &str) -> Body {
        Body {
            content_type: "text/plain".to_string(),
            data: s.as_bytes().to_vec(),
        }
    }

    pub fn from_bytes(b: Vec<u8>) -> Body {
        Body {
            content_type: "application/octet-stream".to_string(),
            data: b,
        }
    }

    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.data).to_string()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn json<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.content_type == "application/json" {
            // Use from_slice instead of converting to string first
            serde_json::from_slice(&self.data).ok()
        } else {
            None
        }
    }

    pub fn x_www_form_urlencoded<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.content_type == "application/x-www-form-urlencoded" {
            serde_json::from_value(Self::parse_urlencoded(&self.data).ok()?).ok()
        } else {
            None
        }
    }

    pub fn form_data<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.content_type.starts_with("multipart/form-data") {
            serde_json::from_value(
                Self::parse_multipart(&self.content_type, &self.data).ok()?
            ).ok()
        } else {
            None
        }
    }

    fn parse_multipart(content_type: &str, body: &[u8]) -> Result<Value, MultipartError> {
        let boundary = Self::extract_boundary(content_type)?;
        let delimiter = format!("--{boundary}");
        
        let parts = Self::split_body(body, &delimiter)?;
        let mut json = Map::with_capacity(parts.len());

        for part in parts {
            let (headers, content) = Self::split_headers_content(part)?;
            let headers = Self::parse_headers(headers)?;

            if let Some(name) = headers.get("name") {
                let clean_name = name.trim_matches('"');
                let value = if let Some(filename) = headers.get("filename") {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
                    json!({
                        "filename": filename.trim_matches('"'),
                        "content": encoded,
                        "content_type": headers.get("content-type").unwrap_or(&"application/octet-stream".into())
                    })
                } else if let Ok(text) = String::from_utf8(content.to_vec()) {
                    Value::String(text)
                } else {
                    continue;
                };

                Self::set_nested_value(&mut json, clean_name, value);
            }
        }

        Ok(Value::Object(json))
    }

    fn parse_urlencoded(data: &[u8]) -> Result<Value, std::io::Error> {
        let data_str = String::from_utf8_lossy(data);
        let mut json = Map::new();

        for pair in data_str.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let decoded_key = urlencoding::decode(key).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to decode key")
                })?.into_owned();
                
                let decoded_value = urlencoding::decode(value).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to decode value")
                })?.into_owned();

                Self::set_nested_value(&mut json, &decoded_key, Value::String(decoded_value));
            }
        }

        Ok(Value::Object(json))
    }

    // Optimize body splitting with pre-allocated buffer
    fn split_body<'a>(body: &'a [u8], delimiter: &str) -> Result<Vec<&'a [u8]>, MultipartError> {
        let pattern = format!("\r\n{delimiter}\r\n").into_bytes();
        let end_pattern = format!("\r\n{delimiter}--\r\n").into_bytes();
        
        // Estimate number of parts to pre-allocate vector
        let estimated_parts = body.windows(pattern.len()).filter(|w| w == &pattern).count();
        let mut parts = Vec::with_capacity(estimated_parts + 1);
        
        let mut start = 0;
        while let Some(pos) = Self::find_subsequence(&body[start..], &pattern) {
            parts.push(&body[start..start + pos]);
            start += pos + pattern.len();

            if start + end_pattern.len() <= body.len() 
                && &body[start..start + end_pattern.len()] == end_pattern {
                break;
            }
        }

        if start < body.len() {
            parts.push(&body[start..body.len() - end_pattern.len()]);
        }

        Ok(parts)
    }

    // Recursive helper method to set nested values
    fn set_nested_value(json: &mut Map<String, Value>, key: &str, value: Value) {
        if key.is_empty() {
            return;
        }

        // Parse the key into parts and track if each part is an array index
        let parts: Vec<(String, bool)> = {
            let raw_parts: Vec<&str> = key.split(|c| c == '[' || c == ']')
                .filter(|s| !s.is_empty())
                .collect();
            
            let mut result = Vec::with_capacity(raw_parts.len());
            let mut i = 0;
            while i < raw_parts.len() {
                let part = raw_parts[i].to_string();
                let is_array_index = part.parse::<usize>().is_ok();
                result.push((part, is_array_index));
                i += 1;
            }
            
            // Handle trailing empty brackets
            if key.ends_with("[]") {
                result.push(("".to_string(), true));
            }
            
            result
        };

        if parts.is_empty() {
            return;
        }

        fn set_value_recursive(current: &mut Value, parts: &[(String, bool)], index: usize, value: &Value) {
            if index >= parts.len() {
                *current = value.clone();
                return;
            }

            let (part, is_index) = &parts[index];
            
            match current {
                Value::Object(map) => {
                    if *is_index {
                        // Convert object to array if we encounter an array index
                        let array = map.entry(parts[index - 1].0.clone())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        
                        if let Value::Array(vec) = array {
                            if part.is_empty() {
                                // Empty brackets - append to array
                                if index + 1 >= parts.len() {
                                    vec.push(value.clone());
                                } else {
                                    vec.push(Value::Object(Map::new()));
                                    if let Some(last) = vec.last_mut() {
                                        set_value_recursive(last, parts, index + 1, value);
                                    }
                                }
                            } else {
                                // Numeric index - set at specific position
                                let idx = part.parse::<usize>().unwrap_or(0);
                                while vec.len() <= idx {
                                    vec.push(Value::Object(Map::new()));
                                }
                                if index + 1 >= parts.len() {
                                    vec[idx] = value.clone();
                                } else {
                                    set_value_recursive(&mut vec[idx], parts, index + 1, value);
                                }
                            }
                        }
                    } else {
                        // Handle object property
                        let next = map.entry(part.clone())
                            .or_insert_with(|| {
                                if index + 1 < parts.len() {
                                    if parts[index + 1].1 {
                                        Value::Array(Vec::new())
                                    } else {
                                        Value::Object(Map::new())
                                    }
                                } else {
                                    value.clone()
                                }
                            });
                        set_value_recursive(next, parts, index + 1, value);
                    }
                }
                Value::Array(vec) => {
                    if part.is_empty() {
                        // Empty brackets - append
                        if index + 1 >= parts.len() {
                            vec.push(value.clone());
                        } else {
                            vec.push(Value::Object(Map::new()));
                            if let Some(last) = vec.last_mut() {
                                set_value_recursive(last, parts, index + 1, value);
                            }
                        }
                    } else {
                        // Numeric index - set at position
                        let idx = part.parse::<usize>().unwrap_or(0);
                        while vec.len() <= idx {
                            vec.push(Value::Object(Map::new()));
                        }
                        if index + 1 >= parts.len() {
                            vec[idx] = value.clone();
                        } else {
                            set_value_recursive(&mut vec[idx], parts, index + 1, value);
                        }
                    }
                }
                _ => {}
            }
        }

        let (root_key, _is_array_root) = &parts[0];
        let mut current = json.entry(root_key.clone())
            .or_insert_with(|| {
                if parts.len() > 1 && parts[1].1 {
                    Value::Array(Vec::new())
                } else {
                    Value::Object(Map::new())
                }
            });

        set_value_recursive(&mut current, &parts, 1, &value);
    }

    // Helper functions

    fn extract_boundary(content_type: &str) -> Result<String, MultipartError> {
        content_type
            .split(';')
            .find_map(|s| s.trim().strip_prefix("boundary="))
            .map(|s| s.trim_matches('"').to_string())
            .ok_or(MultipartError::BoundaryNotFound)
    }

    fn split_headers_content(part: &[u8]) -> Result<(&[u8], &[u8]), MultipartError> {
        let sep = b"\r\n\r\n";
        part.windows(sep.len())
            .position(|w| w == sep)
            .map(|pos| (&part[..pos], &part[pos + sep.len()..]))
            .ok_or(MultipartError::InvalidFormat)
    }

    fn parse_headers(headers: &[u8]) -> Result<HashMap<String, String>, MultipartError> {
        let mut map = HashMap::new();
        let headers_str = std::str::from_utf8(headers).map_err(|_| MultipartError::Utf8Error)?;

        for line in headers_str.split("\r\n") {
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_lowercase();
                let value = value.trim().trim_matches('"');

                // Special handling for Content-Disposition
                if key == "content-disposition" {
                    for param in value.split(';').skip(1) {
                        if let Some((k, v)) = param.trim().split_once('=') {
                            map.insert(k.to_string(), v.to_string());
                        }
                    }
                } else {
                    map.insert(key, value.to_string());
                }
            }
        }

        Ok(map)
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}

impl From<Vec<u8>> for Body {
    fn from(b: Vec<u8>) -> Body {
        Body::from_bytes(b)
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub data: HashMap<String, Value>,
    pub body: Body,
    pub plugins: Plugins,
}

impl Request {
    pub fn get_header(&self, key: &str) -> Option<&str> {
        match self.headers.get(key) {
            Some(v) => Some(v),
            None => None,
        }
    }

    pub fn get_method(&self) -> &Method {
        &self.method
    }

    pub fn get_data(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    pub fn set_data<T>(&mut self, key: &str, value: T)
    where
        T: serde::Serialize,
    {
        if let Ok(value) = serde_json::to_value(value) {
            self.data.insert(key.to_string(), value);
        }
    }

    // New method to get typed data
    pub fn get_typed_data<T>(&self, key: &str) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        self.data.get(key).and_then(|value| {
            // Use from_value instead of clone + from_value for better performance
            serde_json::from_value(value.to_owned()).ok()
        })
    }
}

pub enum ParseError {
    InvalidRequest,
}

impl std::fmt::Debug for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "ParseError")
    }
}
