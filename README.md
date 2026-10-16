# Argus

Argus is a native desktop code-review tool written in Rust with GPUI and styled
like the Zed editor. You pick a base and a head branch, read the diff, comment on
lines, and have Claude Code apply each comment as a code change you can accept or
reject — with each run isolated in its own git worktree and nothing landing on the
branch until you accept it.

The full product specification lives in
[issue #1 — SPEC: Argus MVP](https://github.com/Chris-Kode/argus/issues/1).
Feature work is tracked as sub-issues of that spec.

## Quick start

```sh
# rustup installs the pinned toolchain from rust-toolchain.toml automatically
cargo run -p argus
```

This opens an empty window titled **Argus**. User data lives in `~/.argus/`;
set `ARGUS_HOME` to override it (used by tests and CI):

```sh
ARGUS_HOME=/tmp/argus-dev cargo run -p argus
```

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for build, lint and GPUI-pin conventions,
and [AGENTS.md](AGENTS.md) for the rules that apply to AI coding agents.

## License

Apache-2.0. See [LICENSE](LICENSE).
