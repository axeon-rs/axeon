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

use crate::error::{ServerError, ServerResult};
use crate::message::Request;
use bytes::Bytes;
use http::HeaderMap;
use std::path::Path;

/// A `multipart/form-data` body read field by field as it arrives.
pub struct Multipart {
    inner: multer::Multipart<'static>,
}

/// One field of a multipart form: a text value or an uploaded file.
pub struct Field {
    inner: multer::Field<'static>,
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
        Ok(self.inner.next_field().await.map_err(multipart_error)?.map(|inner| Field { inner }))
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

    /// Reads the whole field into memory.
    pub async fn bytes(self) -> ServerResult<Bytes> {
        self.inner.bytes().await.map_err(multipart_error)
    }

    /// Reads the whole field as UTF-8 text.
    pub async fn text(self) -> ServerResult<String> {
        self.inner.text().await.map_err(multipart_error)
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
