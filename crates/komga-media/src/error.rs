//! Error type for the media pipeline.
//!
//! Variants map to komga's checked exceptions so the HTTP layer can reproduce its status codes:
//! - `NotReady` → MediaNotReadyException (404 "Book analysis failed")
//! - `Unsupported` → MediaUnsupportedException (400, carries an ERR_ code when known)
//! - `EntryNotFound` → EntryNotFoundException (404)
//! - `Conversion` → ImageConversionException (404 with message)
//! - `NoSuchFile` → NoSuchFileException (404 "File not found, it may have moved")
//! - `PageOutOfBounds` → IndexOutOfBoundsException (400 "Page number does not exist")

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("book media is not ready")]
    NotReady,

    #[error("{message}")]
    Unsupported {
        message: String,
        code: Option<String>,
    },

    #[error("entry does not exist: {0}")]
    EntryNotFound(String),

    #[error("{0}")]
    Conversion(String),

    #[error("no thumbnail could be found")]
    NoThumbnail,

    #[error("file not found: {0}")]
    NoSuchFile(String),

    #[error("page {0} does not exist")]
    PageOutOfBounds(usize),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl MediaError {
    pub fn unsupported(message: impl Into<String>) -> Self {
        MediaError::Unsupported {
            message: message.into(),
            code: None,
        }
    }

    pub fn unsupported_coded(message: impl Into<String>, code: impl Into<String>) -> Self {
        MediaError::Unsupported {
            message: message.into(),
            code: Some(code.into()),
        }
    }
}

pub type Result<T> = std::result::Result<T, MediaError>;
