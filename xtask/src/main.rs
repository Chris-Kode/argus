//! Argus dev tasks, run through the `cargo xtask` alias.
//!
//! ```sh
//! cargo xtask fixtures [--seed N] [--out DIR] [--only NAME]...
//! cargo xtask bundle [--universal] [--out DIR]
//! ```
//!
//! `bundle` builds the release binary and packages it: `Argus.app`, a `.dmg`
//! and a `.zip` on macOS, a `.tar.gz` on Linux (see [`bundle`]).
//!
//! `fixtures` generates the performance fixture repositories (spec §12) under
//! `target/fixtures/`. Each one is rebuilt from scratch, deterministically
//! from the seed: two runs with the same seed produce the same commits, so
//! the same SHAs (see `docs/perf.md`).

mod bundle;
mod content;
mod fast_import;
mod fixtures;
mod icon;
mod png;
mod rng;

use std::path::PathBuf;

use anyhow::{Context as _, bail};

/// The default seed; changing it changes every fixture SHA.
const DEFAULT_SEED: u64 = 0x00A2_6E55;

const USAGE: &str = "\
Usage: cargo xtask <command>

Commands:
  bundle [--universal] [--out DIR]
      Build the release binary and package it into DIR (default:
      target/bundle). macOS: Argus.app, a .dmg and a .zip, for this Mac's
      architecture or, with --universal, arm64 and x86_64 together. Linux:
      a .tar.gz with the binary, a .desktop entry and the icon.
  fixtures [--seed N] [--out DIR] [--only NAME]...
      Generate the perf fixture repos (huge-files, huge-file, many-branches)
      into DIR (default: target/fixtures). Existing fixtures are replaced.
  help
      Show this message.";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("bundle") => bundle_command(args.collect()),
        Some("fixtures") => fixtures_command(args.collect()),
        Some("help" | "--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Parses the `bundle` options and packages the app.
fn bundle_command(args: Vec<String>) -> anyhow::Result<()> {
    let root = workspace_root();
    let mut options = bundle::Options {
        out: root.join("target").join("bundle"),
        universal: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--universal" => options.universal = true,
            "--out" => {
                options.out = PathBuf::from(args.next().context("`--out` needs a value")?);
            }
            other => bail!("unknown option `{other}`\n\n{USAGE}"),
        }
    }
    bundle::run(&root, &options)
}

/// Parses the `fixtures` options and generates the selected fixtures.
fn fixtures_command(args: Vec<String>) -> anyhow::Result<()> {
    let mut seed = DEFAULT_SEED;
    let mut out = workspace_root().join("target").join("fixtures");
    let mut only = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("`{name}` needs a value"))
        };
        match arg.as_str() {
            "--seed" => {
                let raw = value("--seed")?;
                seed = parse_seed(&raw).with_context(|| format!("invalid seed `{raw}`"))?;
            }
            "--out" => out = PathBuf::from(value("--out")?),
            "--only" => only.push(value("--only")?),
            other => bail!("unknown option `{other}`\n\n{USAGE}"),
        }
    }
    for name in &only {
        if !fixtures::NAMES.contains(&name.as_str()) {
            bail!(
                "unknown fixture `{name}`; expected one of: {}",
                fixtures::NAMES.join(", ")
            );
        }
    }
    let selected: Vec<&str> = fixtures::NAMES
        .iter()
        .copied()
        .filter(|name| only.is_empty() || only.iter().any(|wanted| wanted == name))
        .collect();
    fixtures::generate(&out, seed, &selected)
}

/// A decimal or `0x`-prefixed hexadecimal seed.
fn parse_seed(raw: &str) -> Result<u64, std::num::ParseIntError> {
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => raw.parse(),
    }
}

/// The workspace root: this crate lives in `<root>/xtask`.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().map(PathBuf::from).unwrap_or(manifest)
}
