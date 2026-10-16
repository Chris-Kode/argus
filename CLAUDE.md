# CLAUDE.md

@AGENTS.md

The rules in [AGENTS.md](AGENTS.md) apply to every task in this repo. The two
that are easiest to get wrong:

- **Do not write tests.** No `#[test]`, `#[cfg(test)]`, `tests/` directories,
  benches or fixtures — even when an issue's acceptance criteria mention them.
  Skip those criteria and say so in your summary.
- **Do not bump the pins.** `gpui`/`gpui_platform` (root `Cargo.toml`) and the
  Rust toolchain (`rust-toolchain.toml`) move only when a task explicitly asks,
  both together, per [CONTRIBUTING.md](CONTRIBUTING.md).
