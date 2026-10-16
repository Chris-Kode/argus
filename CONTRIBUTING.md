# Contributing to Argus

## Toolchain

[`rust-toolchain.toml`](rust-toolchain.toml) pins Rust **1.98.1** with `rustfmt`
and `clippy`; rustup installs it on first use. The pin matches the toolchain Zed
builds the pinned GPUI rev with — bump the two together, never separately.

## Build and run

```sh
cargo build --workspace
cargo run -p argus
```

`ARGUS_HOME` overrides the user-data directory (default `~/.argus`):

```sh
ARGUS_HOME=/tmp/argus-dev cargo run -p argus
```

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Clippy warnings are errors. `cargo test --workspace` currently runs zero tests
by project policy — see [AGENTS.md](AGENTS.md).

## GPUI pin

GPUI is not reliably published on crates.io, so Argus depends on the Zed
repository at one exact commit. Both GPUI crates must always point at the same
rev, declared in the root [`Cargo.toml`](Cargo.toml) under
`[workspace.dependencies]`:

| Crate | Git source | Pinned rev |
| --- | --- | --- |
| `gpui` | `https://github.com/zed-industries/zed` | `fb69067729cb16a7f6135e2c1600c26a28c59659` |
| `gpui_platform` | `https://github.com/zed-industries/zed` | `fb69067729cb16a7f6135e2c1600c26a28c59659` |

The rev corresponds to Zed tag **v1.24.2**.

### Bumping the pin

1. Pick a new Zed release tag (prefer a stable `vX.Y.Z` over `main`) and resolve
   its commit with
   `git ls-remote https://github.com/zed-industries/zed 'refs/tags/vX.Y.Z*'`.
2. Update the `rev` on **both** `gpui` and `gpui_platform` in the root
   `Cargo.toml`; never let them diverge.
3. Check `rust-toolchain.toml` in the Zed repository at that rev. If its
   `channel` differs from ours, update our `rust-toolchain.toml` in the same
   commit.
4. Run `cargo update -p gpui -p gpui_platform`, then `cargo build --workspace`
   and fix any API breakage — GPUI changes often and breaks consumers.
5. Update the table above (rev and tag) and call out the bump in the PR.

## Commits

- Keep the subject short and imperative.
- One logical change per commit; feature work references its issue:
  `#<issue>: <summary>`.
