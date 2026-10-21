//! Streaming `multipart/form-data` uploads (the default `multipart` feature).
//!
//! ```rust
//! use axeon::{Server, ServerError};
//!
//! let mut app = Server::new();
//! app.route("/upload")
//!     .body_limit(100 * 1024 * 1024)
//!     .stream_body()
//!     .post(|req| async move {
//!         let mut form = req.multipart()?;
//!         let mut saved = Vec::new();
//!         while let Some(field) = form.next_field().await? {
//!             if field.file_name().is_some() {
//!                 // Never build paths from the client's file name.
//!                 let path = std::env::temp_dir().join(format!("upload-{}", saved.len()));
//!                 let size = field.save_to(&path).await?;
//!                 saved.push(size);
//!             } else {
//!                 let _value = field.text().await?;
//!             }
//!         }
//!         Ok::<_, ServerError>(format!("saved {} files", saved.len()))
//!     });
//! ```
//!
//! # Checked uploads
//!
//! For files from untrusted clients, cap each field's size, check its real
//! type from its first bytes, and get its SHA-256 while it streams to
//! storage:
//!
//! ```rust
//! use axeon::{Json, Server, ServerError};
//!
//! let mut app = Server::new();
//! app.route("/documents").body_limit(60 * 1024 * 1024).stream_body().post(|req| async move {
//!     let mut form = req.multipart()?;
//!     let field = form.next_field().await?.ok_or_else(|| ServerError::BadRequest("no file".into()))?;
//!     let path = std::env::temp_dir().join("document.bin");
//!     let upload = field
//!         .limit(50 * 1024 * 1024)                           // 413 above 50 MiB
//!         .accept(&["application/pdf", "image/png", "image/jpeg"]) // 415 otherwise
//!         .save(&path)                                       // or .write_to(&mut any_async_writer)
//!         .await?;
//!     Ok::<_, ServerError>(Json(axeon::json!({
//!         "size": upload.size(),
//!         "sha256": upload.sha256_hex(),
//!         "type": upload.content_type(),
//!     })))
//! });
//! ```

use crate::error::{ServerError, ServerResult};
use crate::message::Request;
use bytes::Bytes;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// A `multipart/form-data` body read field by field as it arrives.
pub struct Multipart {
    inner: multer::Multipart<'static>,
}

/// One field of a multipart form: a text value or an uploaded file.
pub struct Field {
    inner: multer::Field<'static>,
    limit: Option<u64>,
    accept: Option<Vec<String>>,
}

fn multipart_error(error: multer::Error) -> ServerError {
    match error {
        multer::Error::StreamReadFailed(source) => match source.downcast::<ServerError>() {
            Ok(error) => *error,
            Err(source) => ServerError::BadRequest(source.to_string()),
        },
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            ServerError::PayloadTooLarge
        }
        other => ServerError::BadRequest(format!("invalid multipart body: {}", other)),
    }
}

impl Request {
    /// Reads the body as `multipart/form-data`, one field at a time. Use it
    /// on a [`stream_body`](crate::RouteBuilder::stream_body) route
    /// to handle large uploads without buffering them; it also works on
    /// buffered routes. Fails with 415 for other content types.
    pub fn multipart(&self) -> ServerResult<Multipart> {
        let content_type = self
            .get_header("content-type")
            .ok_or_else(|| ServerError::UnsupportedMediaType("expected multipart/form-data".to_string()))?;
        let boundary = multer::parse_boundary(content_type)
            .map_err(|_| ServerError::UnsupportedMediaType("expected multipart/form-data".to_string()))?;
        Ok(Multipart { inner: multer::Multipart::new(self.body_stream()?, boundary) })
    }
}

impl Multipart {
    /// The next field, or `None` after the last one. A field must be read
    /// (or dropped) before asking for the next.
    pub async fn next_field(&mut self) -> ServerResult<Option<Field>> {
        Ok(self.inner.next_field().await.map_err(multipart_error)?.map(|inner| Field { inner, limit: None, accept: None }))
    }
}

impl Field {
    /// The form field name.
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// The file name sent by the client, for file fields. Treat it as
    /// untrusted: don't use it to build paths.
    pub fn file_name(&self) -> Option<&str> {
        self.inner.file_name()
    }

    /// The field's content type, e.g. `image/png`.
    pub fn content_type(&self) -> Option<&str> {
        self.inner.content_type().map(|mime| mime.as_ref())
    }

    /// The field's own headers.
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// The next chunk of the field's data, or `None` at its end.
    pub async fn chunk(&mut self) -> ServerResult<Option<Bytes>> {
        self.inner.chunk().await.map_err(multipart_error)
    }

    /// Reads the whole field into memory, up to its [`limit`](Field::limit).
    pub async fn bytes(mut self) -> ServerResult<Bytes> {
        let Some(limit) = self.limit else { return self.inner.bytes().await.map_err(multipart_error) };
        let mut data = bytes::BytesMut::new();
        while let Some(chunk) = self.chunk().await? {
            if (data.len() + chunk.len()) as u64 > limit {
                return Err(ServerError::PayloadTooLarge);
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data.freeze())
    }

    /// Reads the whole field as UTF-8 text, up to its [`limit`](Field::limit).
    pub async fn text(self) -> ServerResult<String> {
        let bytes = self.bytes().await?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ServerError::BadRequest("field is not valid UTF-8".into()))
    }

    /// Caps this field at `bytes`: reading more fails with 413
    /// ([`ServerError::PayloadTooLarge`]). The route's body limit still
    /// applies to the whole form.
    pub fn limit(mut self, bytes: u64) -> Self {
        self.limit = Some(bytes);
        self
    }

    /// Only accepts files whose content is one of `types`, such as
    /// `application/pdf` or `image/*`, detected from the first bytes with
    /// [`sniff`] (the client's `Content-Type` isn't trusted). Anything else,
    /// including content of no known type, fails with 415 in
    /// [`write_to`](Field::write_to) and [`save`](Field::save) before a
    /// byte is written.
    pub fn accept(mut self, types: &[&str]) -> Self {
        self.accept = Some(types.iter().map(|t| t.to_ascii_lowercase()).collect());
        self
    }

    fn check_type(&self, detected: Option<&'static str>) -> ServerResult<()> {
        let Some(accept) = &self.accept else { return Ok(()) };
        let allowed = detected.is_some_and(|detected| {
            accept.iter().any(|pattern| match pattern.strip_suffix("/*") {
                Some(prefix) => detected.split('/').next() == Some(prefix),
                None => pattern == detected,
            })
        });
        if allowed {
            Ok(())
        } else {
            Err(ServerError::UnsupportedMediaType(format!(
                "file type {} is not accepted",
                detected.unwrap_or("unknown")
            )))
        }
    }

    /// Streams the field into `writer` (a file, an object-store upload, a
    /// buffer...), checking its [limit](Field::limit) and
    /// [type](Field::accept) and computing its SHA-256 on the way. Flushes
    /// the writer at the end, but doesn't close it: finish an object-store
    /// upload yourself.
    ///
    /// On an error, part of the field may already be written: discard
    /// it. [`save`](Field::save) does that for files.
    pub async fn write_to<W: AsyncWrite + Unpin + ?Sized>(mut self, writer: &mut W) -> ServerResult<Upload> {
        let name = self.name().map(str::to_string);
        let file_name = self.file_name().map(str::to_string);
        let declared = self.content_type().map(str::to_string);
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        // Held back until the type is known, so a rejected file writes nothing.
        let mut head: Vec<Bytes> = Vec::new();
        let mut head_len = 0;
        let mut detected = None;
        let mut checked = false;
        while let Some(chunk) = self.chunk().await? {
            size += chunk.len() as u64;
            if self.limit.is_some_and(|limit| size > limit) {
                return Err(ServerError::PayloadTooLarge);
            }
            hasher.update(&chunk);
            if checked {
                writer.write_all(&chunk).await?;
                continue;
            }
            head_len += chunk.len();
            head.push(chunk);
            if head_len >= SNIFF_LEN {
                detected = sniff(&head.concat());
                self.check_type(detected)?;
                checked = true;
                for chunk in head.drain(..) {
                    writer.write_all(&chunk).await?;
                }
            }
        }
        if !checked {
            detected = sniff(&head.concat());
            self.check_type(detected)?;
            for chunk in head.drain(..) {
                writer.write_all(&chunk).await?;
            }
        }
        writer.flush().await?;
        Ok(Upload { name, file_name, declared, detected, size, sha256: hasher.finalize().into() })
    }

    /// Like [`write_to`](Field::write_to) into a new file at `path`,
    /// which is removed if anything fails.
    pub async fn save(self, path: impl AsRef<Path>) -> ServerResult<Upload> {
        let path = path.as_ref();
        let mut file = tokio::fs::File::create(path).await?;
        match self.write_to(&mut file).await {
            Ok(upload) => Ok(upload),
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                Err(error)
            }
        }
    }

    /// Writes the field's data to a new file at `path` as it arrives,
    /// returning the number of bytes written. The file is removed if reading
    /// fails.
    pub async fn save_to(self, path: impl AsRef<Path>) -> ServerResult<u64> {
        use futures::StreamExt;
        let chunks = self.inner.map(|chunk| chunk.map_err(multipart_error));
        crate::body::save_stream(Box::pin(chunks), path.as_ref()).await
    }
}

/// What [`Field::write_to`] and [`Field::save`] stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    name: Option<String>,
    file_name: Option<String>,
    declared: Option<String>,
    detected: Option<&'static str>,
    size: u64,
    sha256: [u8; 32],
}

impl Upload {
    /// The form field name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The client's file name; untrusted.
    pub fn file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }

    /// The content type detected from the data with [`sniff`], if known.
    pub fn content_type(&self) -> Option<&'static str> {
        self.detected
    }

    /// The content type the client declared; untrusted.
    pub fn declared_content_type(&self) -> Option<&str> {
        self.declared.as_deref()
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The SHA-256 of the data, e.g. to find duplicate documents.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// The SHA-256 as 64 lowercase hex digits.
    pub fn sha256_hex(&self) -> String {
        self.sha256.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

/// How many leading bytes [`sniff`] looks at.
const SNIFF_LEN: usize = 16;

/// The content type of `data` from its first bytes ("magic numbers"), for
/// common binary formats: PDF, PNG, JPEG, GIF, WebP, TIFF, BMP, HEIC/AVIF,
/// MP4, ZIP (including DOCX/XLSX, which are ZIP files), gzip and 7z. `None`
/// for anything else, including text formats, which can't be told apart
/// reliably.
///
/// ```rust
/// use axeon::multipart::sniff;
///
/// assert_eq!(sniff(b"%PDF-1.7\n..."), Some("application/pdf"));
/// assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
/// assert_eq!(sniff(b"hello"), None);
/// ```
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    let starts = |magic: &[u8]| data.starts_with(magic);
    let at = |offset: usize, magic: &[u8]| data.get(offset..offset + magic.len()) == Some(magic);
    Some(if starts(b"%PDF-") {
        "application/pdf"
    } else if starts(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        "image/gif"
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if starts(b"II*\0") || starts(b"MM\0*") {
        "image/tiff"
    } else if starts(b"BM") && data.len() >= 14 {
        "image/bmp"
    } else if at(4, b"ftyp") {
        match data.get(8..12) {
            Some(b"heic" | b"heix" | b"mif1" | b"msf1") => "image/heic",
            Some(b"avif") => "image/avif",
            _ => "video/mp4",
        }
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        "application/zip"
    } else if starts(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if starts(b"7z\xBC\xAF\x27\x1C") {
        "application/x-7z-compressed"
    } else {
        return None;
    })
}
