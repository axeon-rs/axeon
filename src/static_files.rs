//! Static file serving: streamed from disk, with range requests and
//! conditional (304) responses.

use crate::message::{Request, Response};
use bytes::Bytes;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Ranges up to this size are read into memory (and can be compressed by
/// `Compression` middleware); larger ones are streamed from disk.
const BUFFER_LIMIT: u64 = 256 * 1024;
const STREAM_CHUNK: usize = 64 * 1024;

pub(crate) struct StaticFiles {
    root: PathBuf,
    cache_control: String,
    /// Served for unmatched page navigations (single-page apps).
    fallback: Option<PathBuf>,
}

/// The outcome of a `Range` header.
#[derive(Debug, PartialEq, Eq)]
enum ByteRange {
    /// Serve these bytes (inclusive) with 206.
    Partial(u64, u64),
    /// 416 Range Not Satisfiable.
    Unsatisfiable,
    /// No usable range: serve the whole file.
    Full,
}

/// Parses a single-range `bytes=` header for a file of `len` bytes.
/// Multiple ranges and invalid syntax are ignored (whole file), which the
/// spec allows.
fn parse_range(header: &str, len: u64) -> ByteRange {
    let Some((unit, spec)) = header.split_once('=') else { return ByteRange::Full };
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((start, end)) = spec.trim().split_once('-') else { return ByteRange::Full };
    let parse = |s: &str| -> Option<u64> {
        let s = s.trim();
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
    };
    match (start.trim().is_empty(), end.trim().is_empty()) {
        // bytes=-N: the last N bytes
        (true, false) => match parse(end) {
            None => ByteRange::Full,
            Some(0) => ByteRange::Unsatisfiable,
            Some(_) if len == 0 => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Partial(len.saturating_sub(n), len - 1),
        },
        // bytes=A-
        (false, true) => match parse(start) {
            None => ByteRange::Full,
            Some(a) if a >= len => ByteRange::Unsatisfiable,
            Some(a) => ByteRange::Partial(a, len - 1),
        },
        // bytes=A-B
        (false, false) => match (parse(start), parse(end)) {
            (Some(a), Some(b)) if a <= b => {
                if a >= len {
                    ByteRange::Unsatisfiable
                } else {
                    ByteRange::Partial(a, b.min(len - 1))
                }
            }
            _ => ByteRange::Full,
        },
        (true, true) => ByteRange::Full,
    }
}

/// Weak comparison of an `If-None-Match` list against our ETag.
fn etag_matches(header: &str, etag: &str) -> bool {
    let bare = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    header.trim() == "*" || header.split(',').any(|tag| bare(tag) == bare(etag))
}

fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

impl StaticFiles {
    pub(crate) fn new(root: PathBuf, cache_control: String, fallback: Option<PathBuf>) -> Self {
        Self { root, cache_control, fallback }
    }

    /// The single-page-app fallback for a browser navigation to a client
    /// route: a GET/HEAD that accepts HTML, for a path without a file
    /// extension. Asset and API requests still get 404.
    pub(crate) async fn fallback(&self, req: &Request) -> Option<Response> {
        let fallback = self.fallback.as_ref()?;
        let accepts_html = req.get_header("accept").is_some_and(|accept| accept.contains("text/html"));
        let last_segment = req.path.rsplit('/').next().unwrap_or("");
        if !accepts_html || last_segment.contains('.') {
            return None;
        }
        let path = tokio::fs::canonicalize(self.root.join(fallback)).await.ok()?;
        if !path.starts_with(&self.root) {
            return None;
        }
        let metadata = tokio::fs::metadata(&path).await.ok()?;
        let mut response = self.serve_path(&path, &metadata, req).await?;
        // The app shell must be revalidated so deploys take effect.
        response.header("Cache-Control", "no-cache");
        Some(response)
    }

    /// Serves the file for a GET or HEAD request, or `None` if there is no
    /// such file under the root.
    pub(crate) async fn serve(&self, req: &Request) -> Option<Response> {
        let decoded = urlencoding::decode(req.path.trim_start_matches('/')).ok()?;
        // `\` is a separator on Windows, so `%5C` could slip a hidden name
        // past the check below; `:` reaches alternate data streams there.
        // Neither belongs in a URL path, so refuse them everywhere.
        if decoded.contains(['\\', '\0']) || (cfg!(windows) && decoded.contains(':')) {
            return None;
        }
        // Hidden files (.env, .git) are never served, except /.well-known/.
        let hidden = decoded
            .split('/')
            .enumerate()
            .any(|(i, segment)| segment.starts_with('.') && !(i == 0 && segment == ".well-known"));
        if hidden {
            return None;
        }
        let mut path = tokio::fs::canonicalize(self.root.join(decoded.as_ref())).await.ok()?;
        if !path.starts_with(&self.root) {
            return None;
        }
        let mut metadata = tokio::fs::metadata(&path).await.ok()?;
        if metadata.is_dir() {
            // A directory serves its index.html.
            path = path.join("index.html");
            metadata = tokio::fs::metadata(&path).await.ok()?;
        }
        if !metadata.is_file() {
            return None;
        }
        self.serve_path(&path, &metadata, req).await
    }

    /// Serves one file, with conditional and range request handling.
    async fn serve_path(&self, path: &Path, metadata: &std::fs::Metadata, req: &Request) -> Option<Response> {
        let path = path.to_path_buf();
        let len = metadata.len();
        let modified = metadata.modified().ok();
        let modified_secs = modified.and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs());
        let etag = format!("\"{:x}-{:x}\"", len, modified_secs.unwrap_or(0));
        let last_modified = modified.map(httpdate::fmt_http_date);

        let mut response = Response::new(200);
        response.header("ETag", &etag).header("Cache-Control", &self.cache_control);
        if let Some(last_modified) = &last_modified {
            response.header("Last-Modified", last_modified);
        }

        // Conditional requests: If-None-Match wins over If-Modified-Since.
        let not_modified = match req.get_header("if-none-match") {
            Some(tags) => etag_matches(tags, &etag),
            None => match (req.get_header("if-modified-since"), modified_secs) {
                (Some(since), Some(secs)) => httpdate::parse_http_date(since)
                    .ok()
                    .and_then(|since| since.duration_since(UNIX_EPOCH).ok())
                    .is_some_and(|since| secs <= since.as_secs()),
                _ => false,
            },
        };
        if not_modified {
            response.status = 304;
            return Some(response);
        }

        response.header("Content-Type", content_type(&path)).header("Accept-Ranges", "bytes");

        // Range requests (GET only), unless If-Range says the file changed.
        let range_header = req.get_header("range").filter(|_| req.method == crate::Method::GET);
        let if_range_ok = match req.get_header("if-range") {
            None => true,
            Some(tag) if tag.trim_start().starts_with('"') => tag.trim() == etag,
            Some(date) => last_modified.as_deref() == Some(date.trim()),
        };
        let (start, end) = match range_header.filter(|_| if_range_ok).map(|h| parse_range(h, len)) {
            Some(ByteRange::Partial(start, end)) => {
                response.status = 206;
                response.header("Content-Range", format!("bytes {}-{}/{}", start, end, len));
                (start, end)
            }
            Some(ByteRange::Unsatisfiable) => {
                let mut response = Response::new(416);
                response.header("Content-Range", format!("bytes */{}", len)).header("Accept-Ranges", "bytes");
                return Some(response);
            }
            Some(ByteRange::Full) | None => (0, len.saturating_sub(1)),
        };
        let length = if len == 0 { 0 } else { end - start + 1 };

        let mut file = tokio::fs::File::open(&path).await.ok()?;
        if start > 0 {
            file.seek(SeekFrom::Start(start)).await.ok()?;
        }
        let content_type = content_type(&path);
        if length <= BUFFER_LIMIT {
            let mut body = vec![0; length as usize];
            file.read_exact(&mut body).await.ok()?;
            response.body = Bytes::from(body);
            Some(response)
        } else {
            let stream = ReaderStream::with_capacity(file.take(length), STREAM_CHUNK);
            let mut streamed = Response::stream_with_length(content_type, length, stream);
            streamed.status = response.status;
            for (name, value) in response.headers.iter() {
                streamed.headers.insert(name.clone(), value.clone());
            }
            Some(streamed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        use ByteRange::*;
        assert_eq!(parse_range("bytes=0-9", 100), Partial(0, 9));
        assert_eq!(parse_range("bytes=90-", 100), Partial(90, 99));
        assert_eq!(parse_range("bytes=-10", 100), Partial(90, 99));
        assert_eq!(parse_range("bytes=-500", 100), Partial(0, 99));
        assert_eq!(parse_range("bytes=50-500", 100), Partial(50, 99));
        assert_eq!(parse_range("BYTES = 1-2", 100), Partial(1, 2));
        assert_eq!(parse_range("bytes=100-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=100-200", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Full);
        assert_eq!(parse_range("bytes=9-1", 100), Full);
        assert_eq!(parse_range("bytes=abc", 100), Full);
        assert_eq!(parse_range("bytes=+1-2", 100), Full);
        assert_eq!(parse_range("items=0-1", 100), Full);
    }

    #[test]
    fn compares_etags_weakly() {
        assert!(etag_matches("\"a-1\"", "\"a-1\""));
        assert!(etag_matches("W/\"a-1\"", "\"a-1\""));
        assert!(etag_matches("\"x\", \"a-1\"", "\"a-1\""));
        assert!(etag_matches("*", "\"a-1\""));
        assert!(!etag_matches("\"a-2\"", "\"a-1\""));
    }

    mod properties {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(5000))]

            /// Any accepted range lies within the file.
            #[test]
            fn ranges_stay_in_bounds(header in r"(?i)(bytes|items)? ?= ?[0-9+-]{0,12}(,[0-9-]{0,6})?", len in 0u64..10_000) {
                if let ByteRange::Partial(start, end) = parse_range(&header, len) {
                    prop_assert!(start <= end && end < len, "{} for len {} gave {}-{}", header, len, start, end);
                }
            }

            #[test]
            fn arbitrary_headers_never_panic(header in ".{0,64}", len in any::<u64>()) {
                let _ = parse_range(&header, len);
                let _ = etag_matches(&header, "\"a-1\"");
            }
        }
    }
}
