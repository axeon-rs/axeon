use crate::http::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use flate2::write::{GzEncoder, DeflateEncoder};
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
            for skip_type in &self.skip_types {
                if ct.starts_with(skip_type) {
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
            ],
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
            let mut response = next.handle(req).await?;
            
            // Get the accepted encodings from the request
            let accept_encoding = response.headers
                .get("accept-encoding")
                .map(|h| h.to_lowercase());

            let content_type = response.headers.get("content-type");
            let original_body = response.body.clone();
            let should_compress = config.should_compress(
                content_type.as_deref().map(|x| x.as_str()),
                original_body.len()
            );

            if should_compress {
                if let Some(accepted) = accept_encoding {
                    let mut compressed = Vec::new();
                    
                    if accepted.contains("gzip") {
                        let mut encoder = GzEncoder::new(Vec::new(), config.level);
                        encoder.write_all(original_body.as_bytes())?;
                        compressed = encoder.finish()?;
                        response.headers.insert("Content-Encoding".to_string(), "gzip".to_string());
                    } else if accepted.contains("deflate") {
                        let mut encoder = DeflateEncoder::new(Vec::new(), config.level);
                        encoder.write_all(original_body.as_bytes())?;
                        compressed = encoder.finish()?;
                        response.headers.insert("Content-Encoding".to_string(), "deflate".to_string());
                    }

                    if !compressed.is_empty() {
                        response.body = String::from_utf8_lossy(&compressed).to_string();
                        response.headers.insert(
                            "Content-Length".to_string(),
                            compressed.len().to_string()
                        );
                        // Add Vary header to help caches
                        response.headers.insert("Vary".to_string(), "Accept-Encoding".to_string());
                    }
                }
            }

            Ok(response)
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self::new(self.config.clone()))
    }
}