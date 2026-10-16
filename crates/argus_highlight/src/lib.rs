//! Tree-sitter syntax highlighting for Argus.
//!
//! Highlighting runs on a background pool and never on the UI thread; visible
//! text is painted unhighlighted first and recolored when highlights arrive.
//! Filled in by issue #11.
