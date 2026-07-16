//! Crate error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OmniError {
    /// ort 2.0-rc carries a typed `Error<R>` per call site (`Error<SessionBuilder>`,
    /// `Error<Session>`, …); we flatten any of them to its `Display` string so the
    /// crate isn't coupled to ort's error-context taxonomy.
    #[error("onnx runtime: {0}")]
    Ort(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("image: {0}")]
    Image(#[from] image::ImageError),
    #[error("model: {0}")]
    Model(String),
}

pub type Result<T> = std::result::Result<T, OmniError>;

/// Lift any ort error (typed or not) into [`OmniError::Ort`] via `Display`.
pub(crate) fn ort_err<E: std::fmt::Display>(e: E) -> OmniError {
    OmniError::Ort(e.to_string())
}
