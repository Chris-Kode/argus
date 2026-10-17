//! Recognizing a run that failed because Claude Code is not signed in.
//!
//! Argus relies on the user's existing Claude Code sign-in and stores no API
//! keys (spec §11.6), so the only remedy it can offer is to sign in again.

/// What the UI shows when a run fails with an authentication error.
pub const AUTH_ERROR_HINT: &str = "Run `claude` in a terminal to sign in";

/// Lower-case fragments of the messages Claude Code prints when it has no
/// valid credentials. Claude Code 2.1 reports `Not logged in · Please run
/// /login` as the assistant text and in the `result` event.
const AUTH_ERROR_PATTERNS: [&str; 8] = [
    "not logged in",
    "please run /login",
    "invalid api key",
    "oauth token has expired",
    "oauth token revoked",
    "authentication_error",
    "authentication failed",
    "api error: 401",
];

/// Whether `text` (stderr, or the `result` text of a failed run) reports a
/// missing or expired sign-in.
pub fn is_auth_error(text: &str) -> bool {
    let text = text.to_lowercase();
    AUTH_ERROR_PATTERNS
        .iter()
        .any(|pattern| text.contains(pattern))
}
