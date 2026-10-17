//! Highlighting errors.

/// Why a blob has no highlights. Every case falls back to plain text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HighlightError {
    /// The job was cancelled (its file scrolled far away or was closed).
    #[error("highlighting was cancelled")]
    Cancelled,
    /// The bundled queries of this language did not compile.
    #[error("no usable grammar for {0}")]
    Grammar(&'static str),
    /// Tree-sitter failed.
    #[error("highlighting failed: {0}")]
    Failed(String),
}
