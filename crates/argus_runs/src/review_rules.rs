//! `~/.argus/review-rules.md`: user-level rules appended to Claude's system
//! prompt on every run (`--append-system-prompt-file`, spec §6.2).

use std::io;
use std::path::Path;

use tokio::io::AsyncWriteExt as _;

/// The rules written on first launch.
pub const DEFAULT_REVIEW_RULES: &str = "\
# Argus review rules

These rules are appended to Claude's system prompt for every Argus run.
Edit them freely; Argus never overwrites this file.

- Change only what the review comment asks for.
- Keep the existing code style; do not reformat unrelated code.
- Do not add new dependencies unless the comment asks for it.
- If you must touch another file, say which and why in your final message.
- End with a short summary of the change.
";

/// Creates the review rules file with [`DEFAULT_REVIEW_RULES`] if it does not
/// exist yet; returns whether it was created.
///
/// The file is opened with `create_new`, so an existing file — edited by the
/// user or not — is never truncated or rewritten. Must run on the tokio
/// runtime.
pub async fn ensure_review_rules(path: &Path) -> io::Result<bool> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    file.write_all(DEFAULT_REVIEW_RULES.as_bytes()).await?;
    file.flush().await?;
    Ok(true)
}
