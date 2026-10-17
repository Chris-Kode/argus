//! Tree-sitter syntax highlighting for Argus, off the UI thread (spec §4).
//!
//! - [`Language::detect`] picks a bundled grammar from the file name, the
//!   extension or a shebang; anything else is plain text.
//! - [`HighlightService`] runs highlighting on its own worker threads. Each
//!   job parses the **full** blob once, so multi-line constructs (a block
//!   comment opening above a hunk) are right on every line, and splits the
//!   result into per-line [`Span`]s ([`Highlights::line`]), addressed by the
//!   same 1-based line numbers as the diff.
//! - Results are cached per blob and language ([`HighlightKey`]). Jobs have
//!   a [`Priority`] (on screen first, then prefetch) that can change while
//!   they are queued, and dropping a [`HighlightJob`] cancels it, even
//!   mid-parse.
//! - Blobs above [`MAX_HIGHLIGHT_BYTES`] or [`MAX_HIGHLIGHT_LINES`] come back
//!   as [`Outcome::TooLarge`] unless the request is forced.
//! - Span kinds ([`HighlightKind`]) are Zed's theme syntax keys (`keyword`,
//!   `string`, `comment`, `function`, `type`, …); the UI maps them to
//!   colors.
//!
//! Visible text never waits for any of this: the viewer paints plain text
//! first and repaints when highlights arrive.
//!
//! See [`grammars`] for the bundled grammars and their licences.

mod error;
pub mod grammars;
mod highlights;
mod kind;
mod language;
mod service;

pub use error::HighlightError;
pub use highlights::{Highlights, Span};
pub use kind::{HighlightKind, RECOGNIZED_NAMES};
pub use language::Language;
pub use service::{
    DEFAULT_CACHE_BYTES, HighlightJob, HighlightKey, HighlightRequest, HighlightService,
    JobControl, MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINES, Outcome, Priority, SourceText,
};
