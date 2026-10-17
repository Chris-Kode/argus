//! Command-line arguments and the `ARGUS_PERF` switch.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, bail};
use argus_ui::{LaunchOptions, OpenRequest, PerfOptions};

const USAGE: &str = "\
Usage: argus [--repo PATH [--base BRANCH] [--head BRANCH]] [--auto-scroll]

Options:
  --repo PATH      Open this repository instead of the most recent one
  --base BRANCH    Base branch to compare from (with --repo)
  --head BRANCH    Head branch to compare (with --repo)
  --auto-scroll    Scroll through the diff once it shows, print the perf
                   report and quit (implies ARGUS_PERF=1)
  -h, --help       Show this message
  -V, --version    Show the version

Environment:
  ARGUS_HOME       User-data directory (default ~/.argus)
  ARGUS_PERF=1     Log startup and frame timings, then print a report
                   (see docs/perf.md)";

/// What `main` should do.
pub enum Command {
    /// Start the app.
    Run(LaunchOptions),
    /// Print this text and exit.
    Print(String),
}

/// Parses the process arguments and `ARGUS_PERF`. `launched` is the
/// process start.
pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    launched: Instant,
) -> anyhow::Result<Command> {
    let mut repo = None;
    let mut base = None;
    let mut head = None;
    let mut auto_scroll = false;
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|arg| anyhow::anyhow!("invalid argument {arg:?}"))?;
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("`{name}` needs a value\n\n{USAGE}"))
        };
        match arg.as_str() {
            "--repo" => repo = Some(PathBuf::from(value("--repo")?)),
            "--base" => base = Some(string(value("--base")?)?),
            "--head" => head = Some(string(value("--head")?)?),
            "--auto-scroll" => auto_scroll = true,
            "-h" | "--help" => return Ok(Command::Print(USAGE.to_owned())),
            "-V" | "--version" => {
                return Ok(Command::Print(format!(
                    "argus {}",
                    env!("CARGO_PKG_VERSION")
                )));
            }
            // macOS passes a process serial number to apps opened from Finder.
            other if other.starts_with("-psn_") => {}
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    if repo.is_none() && (base.is_some() || head.is_some()) {
        bail!("`--base` and `--head` need `--repo`\n\n{USAGE}");
    }
    let open = match repo {
        Some(path) => Some(OpenRequest {
            repo: std::path::absolute(&path)
                .with_context(|| format!("invalid repository path {}", path.display()))?,
            base,
            head,
        }),
        None => None,
    };
    let perf = (perf_env() || auto_scroll).then_some(PerfOptions { auto_scroll });
    Ok(Command::Run(LaunchOptions {
        open,
        perf,
        launched,
    }))
}

fn string(value: OsString) -> anyhow::Result<String> {
    value
        .into_string()
        .map_err(|value| anyhow::anyhow!("invalid branch name {value:?}"))
}

/// `ARGUS_PERF` set to anything but empty, `0` or `false`.
fn perf_env() -> bool {
    std::env::var("ARGUS_PERF").is_ok_and(|value| !matches!(value.trim(), "" | "0" | "false"))
}
