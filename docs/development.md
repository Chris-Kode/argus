# Development

## Toolchain

[`rust-toolchain.toml`](../rust-toolchain.toml) pins Rust **1.98.1** with `rustfmt`
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

## Bundling the app

A release build of the app bundle takes a few minutes:

```sh
git clone https://github.com/Chris-Kode/argus.git
cd argus
cargo xtask bundle
```

This writes to `target/bundle/`:

- `Argus.app`;
- `Argus-<version>-macos-<arch>.dmg`: open it and drag Argus to Applications;
- `Argus-<version>-macos-<arch>.zip`: the same app, zipped.

`cargo xtask bundle --universal` builds one app for both Apple silicon and Intel
(first `rustup target add aarch64-apple-darwin x86_64-apple-darwin`).

### Signing and Gatekeeper

By default the app is signed ad hoc, not with a Developer ID, and it is not
notarized. On the Mac that built it, it opens normally. On another Mac, the
first launch is blocked by Gatekeeper ("Apple could not verify…"): open
**System Settings → Privacy & Security** and click **Open Anyway** next to the
Argus message, or clear the download quarantine once:

```sh
xattr -dr com.apple.quarantine /Applications/Argus.app
```

To hand the app to other people, sign it with a Developer ID and notarize it
(this needs an Apple Developer Program membership):

1. Install a **Developer ID Application** certificate in your login keychain
   and find its name with `security find-identity -v -p codesigning`.
2. Store notary credentials in the keychain once, under a profile name of your
   choice (an [app-specific password](https://support.apple.com/102654) works):

   ```sh
   xcrun notarytool store-credentials argus-notary --apple-id you@example.com --team-id TEAMID
   ```

3. Bundle with both:

   ```sh
   cargo xtask bundle --sign "Developer ID Application: Your Name (TEAMID)" --notarize argus-notary
   ```

`--sign` signs the app (hardened runtime, secure timestamp) and the `.dmg`.
`--notarize` sends the `.dmg` to Apple, waits for the verdict (usually a few
minutes) and staples the ticket to the `.dmg` and the app, so the `.zip` made
afterwards carries it too. Gatekeeper then opens Argus without any prompt,
even offline. `--sign` alone gives a Developer-ID-signed app that Gatekeeper
still blocks until it is notarized. `ARGUS_SIGN_IDENTITY` and
`ARGUS_NOTARY_PROFILE` can stand in for the two flags, e.g. in CI.

### Linux (best effort)

`cargo xtask bundle` on Linux writes `target/bundle/argus-<version>-linux-<arch>.tar.gz`
with the `argus` binary, an `argus.desktop` entry and an `argus.png` icon. Put
the binary on your `PATH`, and the desktop entry and the icon in
`~/.local/share/applications` and `~/.local/share/icons`. GPUI needs the usual
Vulkan, Wayland or X11 and font libraries.

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Clippy warnings are errors. `cargo test --workspace` currently runs zero tests
by project policy — see [AGENTS.md](../AGENTS.md).

## Performance

`cargo xtask fixtures` builds the deterministic fixture repositories under
`target/fixtures/`; `ARGUS_PERF=1` (with `--auto-scroll`) prints the timing
report. See [perf.md](perf.md).

## GPUI pin

GPUI is not reliably published on crates.io, so Argus depends on the Zed
repository at one exact commit. Both GPUI crates must always point at the same
rev, declared in the root [`Cargo.toml`](../Cargo.toml) under
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

## Releasing

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds
`cargo xtask bundle --universal` on a macOS runner and publishes the `.dmg` and
`.zip` as a GitHub Release.

The quick way is **Actions → New release → Run workflow** on `main`, picking
`patch`, `minor` or `major`
([`.github/workflows/new-release.yml`](../.github/workflows/new-release.yml)). It
increases that part of the version (`0.1.1` → `0.1.2`, `0.2.0` or `1.0.0`),
commits the bump to `main`, tags it and starts the Release workflow on the
tag. It pushes with the workflow token, so `main` must accept pushes from
GitHub Actions.

By hand:

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`, run
   `cargo build` so `Cargo.lock` follows, and merge it to `main`.
2. Tag that commit and push the tag; it must be the version with a `v`:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

   The job fails if the tag and `Cargo.toml` disagree. A tag with a `-`
   (`v0.2.0-beta.1`) is published as a pre-release.

To build without releasing, run the workflow by hand (Actions → Release → Run
workflow); the files are kept as a workflow artifact.

Installed copies find the release through GitHub's "latest release" API and
download the `Argus-<version>-macos-universal.zip` asset, so keep that name.
Pre-releases are never offered. To try the update flow without publishing,
point `ARGUS_UPDATE_FEED` at a local copy of the release JSON (a `file://` URL
works, for the assets too) and launch the bundled binary,
`Argus.app/Contents/MacOS/argus`, with it set.

### Signing and notarization (not enabled yet)

Releases are signed ad hoc. Once there is an Apple Developer Program
membership:

1. Export the **Developer ID Application** certificate as a `.p12` and create an
   App Store Connect API key (Users and Access → Integrations, role
   *Developer*).
2. Add repository secrets: `MACOS_CERT_P12` (the `.p12`, base64),
   `MACOS_CERT_PASSWORD`, `MACOS_SIGN_IDENTITY` (as `security find-identity`
   prints it), and `NOTARY_KEY` (the `.p8` contents), `NOTARY_KEY_ID`,
   `NOTARY_ISSUER_ID`.
3. In the workflow, before **Bundle**: import the `.p12` into a temporary
   keychain (`security create-keychain`, `security import … -T
   /usr/bin/codesign`, `security set-key-partition-list`), then store a notary
   profile in it with `xcrun notarytool store-credentials argus-notary --key …
   --key-id … --issuer … --keychain …`.
4. Give **Bundle** `ARGUS_SIGN_IDENTITY: ${{ secrets.MACOS_SIGN_IDENTITY }}` and
   `ARGUS_NOTARY_PROFILE: argus-notary`, and drop the Gatekeeper paragraphs from
   the release notes, the README and "Signing and Gatekeeper" above.

Once an installed copy is Developer-ID signed, it accepts only updates signed
by the same team.

## Commits

- Keep the subject short and imperative.
- One logical change per commit; feature work references its issue:
  `#<issue>: <summary>`.
