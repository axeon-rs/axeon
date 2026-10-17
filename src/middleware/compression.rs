use crate::http::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::Compression;
use std::io::Write;

#[derive(Clone)]
pub struct CompressionConfig {
    pub level: Compression,
    pub min_size: usize,
    pub skip_types: Vec<String>,
}

impl CompressionConfig {
    fn should_compress(&self, content_type: Option<&str>, content_length: usize) -> bool {
        // Don't compress if content is too small
        if content_length < self.min_size {
            return false;
        }

        // Don't compress if content type is in skip list
        if let Some(ct) = content_type {
            let ct = ct.to_ascii_lowercase();
            for skip_type in &self.skip_types {
                if ct.starts_with(skip_type.as_str()) {
                    return false;
                }
            }
        }

        true
    }
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            level: Compression::default(),
            min_size: 1024, // Only compress responses larger than 1KB
            skip_types: vec![
                "image/".to_string(),
                "video/".to_string(),
                "audio/".to_string(),
                "application/pdf".to_string(),
                "application/zip".to_string(),
                "application/gzip".to_string(),
                "font/woff".to_string(),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Deflate,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }
}

/// Picks an encoding from an `Accept-Encoding` value, honouring `q=0`.
/// Prefers gzip over deflate.
fn negotiate(accept_encoding: &str) -> Option<Encoding> {
    let mut gzip = None;
    let mut deflate = None;
    let mut wildcard = None;
    for entry in accept_encoding.split(',') {
        let mut parts = entry.split(';');
        let coding = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let mut q = 1.0f32;
        for param in parts {
            if let Some((k, v)) = param.split_once('=') {
                if k.trim().eq_ignore_ascii_case("q") {
                    q = v.trim().parse().unwrap_or(0.0);
                }
            }
        }
        match coding.as_str() {
            "gzip" | "x-gzip" => gzip = Some(q),
            "deflate" => deflate = Some(q),
            "*" => wildcard = Some(q),
            _ => {}
        }
    }
    let accepts = |explicit: Option<f32>| explicit.or(wildcard).is_some_and(|q| q > 0.0);
    if accepts(gzip) {
        Some(Encoding::Gzip)
    } else if accepts(deflate) {
        Some(Encoding::Deflate)
    } else {
        None
    }
}

fn compress(encoding: Encoding, level: Compression, data: &[u8]) -> std::io::Result<Vec<u8>> {
    match encoding {
        Encoding::Gzip => {
            let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 2), level);
            encoder.write_all(data)?;
            encoder.finish()
        }
        // HTTP "deflate" is the zlib format (RFC 9110 §8.4.1.2), not raw deflate.
        Encoding::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::with_capacity(data.len() / 2), level);
            encoder.write_all(data)?;
            encoder.finish()
        }
    }
}

pub struct CompressionMiddleware {
    config: CompressionConfig,
}

impl CompressionMiddleware {
    pub fn new(config: CompressionConfig) -> Self {
        Self { config }
    }
}

impl Middleware for CompressionMiddleware {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let config = self.config.clone();
        Box::pin(async move {
            // Negotiate from the *request* before handing it to the handler.
            let encoding = req.get_header("accept-encoding").and_then(negotiate);

            let mut response = next.handle(req).await?;

            let compressible = !response.is_streaming()
                && crate::http::response::status_has_body(response.status)
                && response.status != 206
                && response.get_header("content-encoding").is_none()
                && config.should_compress(response.get_header("content-type"), response.body.len());
            if !compressible {
                return Ok(response);
            }
            // The representation varies with Accept-Encoding even when we send it uncompressed.
            response.append_vary("Accept-Encoding");

            if let Some(encoding) = encoding {
                let compressed = compress(encoding, config.level, &response.body)?;
                if compressed.len() < response.body.len() {
                    response.body = compressed.into();
                    response.header("Content-Encoding", encoding.name());
                }
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_encodings() {
        assert_eq!(negotiate("gzip, deflate, br"), Some(Encoding::Gzip));
        assert_eq!(negotiate("deflate"), Some(Encoding::Deflate));
        assert_eq!(negotiate("gzip;q=0, deflate"), Some(Encoding::Deflate));
        assert_eq!(negotiate("*"), Some(Encoding::Gzip));
        assert_eq!(negotiate("*;q=0"), None);
        assert_eq!(negotiate("br, identity"), None);
        assert_eq!(negotiate(""), None);
    }
}
