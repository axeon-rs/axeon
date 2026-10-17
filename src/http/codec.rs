//! Minimal HTTP/1.x wire handling: reading request heads and bodies under
//! size limits, and encoding response heads.

use crate::error::ServerError;
use crate::http::{Method, Response};
use std::collections::HashMap;
use std::io;
use std::time::SystemTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Size limits applied while reading a request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Max bytes for the request line plus all header lines.
    pub max_header_size: usize,
    pub max_headers: usize,
    pub max_body_size: usize,
}

pub(crate) struct RequestHead {
    pub method: Method,
    pub target: String,
    /// Minor version of HTTP/1.x
    pub version_minor: u8,
    pub headers: HashMap<String, String>,
}

pub(crate) enum ReadError {
    /// Connection closed cleanly before a new request started.
    Eof,
    Io(io::Error),
    /// The request is malformed or over a limit; respond with this and close.
    Reject(ServerError),
}

impl From<io::Error> for ReadError {
    fn from(err: io::Error) -> Self {
        ReadError::Io(err)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyFraming {
    None,
    Length(usize),
    Chunked,
}

fn bad_request(msg: &str) -> ReadError {
    ReadError::Reject(ServerError::BadRequest(msg.to_string()))
}

/// Reads one line (terminated by LF) of at most `limit` bytes into `buf`,
/// without the line terminator. Returns `Ok(None)` on EOF before any byte.
async fn read_line<R>(reader: &mut R, buf: &mut Vec<u8>, limit: usize) -> Result<Option<usize>, ReadError>
where
    R: AsyncBufRead + Unpin,
{
    buf.clear();
    let n = (&mut *reader).take(limit as u64 + 1).read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if n > limit {
        return Err(ReadError::Reject(ServerError::HeadersTooLarge));
    }
    if buf.last() != Some(&b'\n') {
        return Err(ReadError::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(n))
}

pub(crate) async fn read_head<R>(reader: &mut R, limits: &Limits) -> Result<RequestHead, ReadError>
where
    R: AsyncBufRead + Unpin,
{
    let mut budget = limits.max_header_size;
    let mut line = Vec::with_capacity(256);

    // Request line. RFC 9112 allows ignoring empty lines before it.
    let mut empty_lines = 0;
    loop {
        let n = match read_line(reader, &mut line, budget).await {
            Ok(Some(n)) => n,
            Ok(None) => return Err(ReadError::Eof),
            Err(ReadError::Reject(ServerError::HeadersTooLarge)) => {
                return Err(ReadError::Reject(ServerError::UriTooLong))
            }
            Err(e) => return Err(e),
        };
        budget -= n;
        if !line.is_empty() {
            break;
        }
        empty_lines += 1;
        if empty_lines > 4 {
            return Err(bad_request("Invalid request line"));
        }
    }

    let request_line = std::str::from_utf8(&line).map_err(|_| bad_request("Invalid request line"))?;
    let mut parts = request_line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) if !m.is_empty() && !t.is_empty() => (m, t, v),
        _ => return Err(bad_request("Invalid request line")),
    };
    let version_minor = match version {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        _ => return Err(bad_request("Unsupported HTTP version")),
    };
    let method = Method::parse(method)
        .ok_or_else(|| ReadError::Reject(ServerError::NotImplemented(format!("method {}", method))))?;
    let target = target.to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut count = 0;
    loop {
        let n = read_line(reader, &mut line, budget)
            .await?
            .ok_or(ReadError::Io(io::ErrorKind::UnexpectedEof.into()))?;
        budget -= n;
        if line.is_empty() {
            break;
        }
        count += 1;
        if count > limits.max_headers {
            return Err(ReadError::Reject(ServerError::HeadersTooLarge));
        }
        // Obsolete line folding is rejected (RFC 9112 §5.2).
        if line[0] == b' ' || line[0] == b'\t' {
            return Err(bad_request("Invalid header"));
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or_else(|| bad_request("Invalid header"))?;
        let name = std::str::from_utf8(&line[..colon]).map_err(|_| bad_request("Invalid header"))?;
        // No whitespace is allowed between the field name and the colon.
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err(bad_request("Invalid header"));
        }
        let value = String::from_utf8_lossy(&line[colon + 1..]);
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        let name = name.to_ascii_lowercase();
        match headers.get_mut(&name) {
            Some(existing) => {
                existing.push_str(if name == "cookie" { "; " } else { ", " });
                existing.push_str(value);
            }
            None => {
                headers.insert(name, value.to_string());
            }
        }
    }

    Ok(RequestHead {
        method,
        target,
        version_minor,
        headers,
    })
}

/// Works out how the request body is delimited, rejecting ambiguous
/// framing that could enable request smuggling.
pub(crate) fn body_framing(headers: &HashMap<String, String>, max_body_size: usize) -> Result<BodyFraming, ServerError> {
    if let Some(te) = headers.get("transfer-encoding") {
        if headers.contains_key("content-length") {
            return Err(ServerError::BadRequest(
                "Both Transfer-Encoding and Content-Length present".to_string(),
            ));
        }
        if te.trim().eq_ignore_ascii_case("chunked") {
            return Ok(BodyFraming::Chunked);
        }
        return Err(ServerError::NotImplemented(format!("transfer-encoding {}", te)));
    }

    if let Some(cl) = headers.get("content-length") {
        // Duplicate headers were joined with ", "; every value must agree.
        let mut length: Option<u64> = None;
        for part in cl.split(',') {
            let part = part.trim();
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ServerError::BadRequest("Invalid Content-Length".to_string()));
            }
            let value: u64 = part.parse().map_err(|_| ServerError::PayloadTooLarge)?;
            if length.is_some_and(|l| l != value) {
                return Err(ServerError::BadRequest("Conflicting Content-Length".to_string()));
            }
            length = Some(value);
        }
        let length = length.unwrap_or(0);
        if length > max_body_size as u64 {
            return Err(ServerError::PayloadTooLarge);
        }
        return Ok(if length == 0 { BodyFraming::None } else { BodyFraming::Length(length as usize) });
    }

    Ok(BodyFraming::None)
}

pub(crate) async fn read_body<R>(reader: &mut R, framing: BodyFraming, limits: &Limits) -> Result<Vec<u8>, ReadError>
where
    R: AsyncBufRead + Unpin,
{
    match framing {
        BodyFraming::None => Ok(Vec::new()),
        BodyFraming::Length(len) => {
            let mut body = vec![0; len];
            reader.read_exact(&mut body).await?;
            Ok(body)
        }
        BodyFraming::Chunked => read_chunked(reader, limits).await,
    }
}

async fn read_chunked<R>(reader: &mut R, limits: &Limits) -> Result<Vec<u8>, ReadError>
where
    R: AsyncBufRead + Unpin,
{
    const MAX_CHUNK_LINE: usize = 1024;
    let mut body = Vec::new();
    let mut line = Vec::with_capacity(32);
    loop {
        read_line(reader, &mut line, MAX_CHUNK_LINE)
            .await?
            .ok_or(ReadError::Io(io::ErrorKind::UnexpectedEof.into()))?;
        let size_end = line.iter().position(|&b| b == b';').unwrap_or(line.len());
        let size_str = std::str::from_utf8(&line[..size_end])
            .map_err(|_| bad_request("Invalid chunk size"))?
            .trim_matches(|c| c == ' ' || c == '\t');
        if size_str.is_empty() || size_str.len() > 16 || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad_request("Invalid chunk size"));
        }
        let size = usize::from_str_radix(size_str, 16).map_err(|_| ReadError::Reject(ServerError::PayloadTooLarge))?;

        if size == 0 {
            // Trailer section, discarded.
            let mut budget = limits.max_header_size;
            loop {
                let n = read_line(reader, &mut line, budget)
                    .await?
                    .ok_or(ReadError::Io(io::ErrorKind::UnexpectedEof.into()))?;
                budget -= n;
                if line.is_empty() {
                    return Ok(body);
                }
            }
        }

        if body.len().saturating_add(size) > limits.max_body_size {
            return Err(ReadError::Reject(ServerError::PayloadTooLarge));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).await?;

        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await?;
        if &crlf != b"\r\n" {
            return Err(bad_request("Invalid chunk terminator"));
        }
    }
}

/// True if the comma-separated header value contains `token` (case-insensitive).
pub(crate) fn has_token(value: &str, token: &str) -> bool {
    value.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// Statuses that never carry a body (RFC 9110 §6.4.1).
pub(crate) fn status_has_body(status: u16) -> bool {
    !(100..200).contains(&status) && status != 204 && status != 304
}

/// Encodes the status line and headers. Framing headers set by the
/// application are replaced, and headers containing CR/LF are dropped to
/// prevent response splitting.
pub(crate) fn encode_head(response: &Response, keep_alive: bool, http10_client: bool) -> Vec<u8> {
    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(b"HTTP/1.1 ");
    head.extend_from_slice(response.status.to_string().as_bytes());
    head.push(b' ');
    head.extend_from_slice(reason_phrase(response.status).as_bytes());
    head.extend_from_slice(b"\r\n");

    let mut has_date = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        let unsafe_header = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n');
        if name.is_empty() || unsafe_header(name) || unsafe_header(value) {
            eprintln!("Dropping response header {:?}: contains CR/LF", name);
            continue;
        }
        has_date |= name.eq_ignore_ascii_case("date");
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }

    if !has_date {
        head.extend_from_slice(b"Date: ");
        head.extend_from_slice(httpdate::fmt_http_date(SystemTime::now()).as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if !keep_alive {
        head.extend_from_slice(b"Connection: close\r\n");
    } else if http10_client {
        head.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    if status_has_body(response.status) {
        head.extend_from_slice(b"Content-Length: ");
        head.extend_from_slice(response.body.len().to_string().as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}
