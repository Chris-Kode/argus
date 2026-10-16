# Sandbox image for developing Argus in Runbase: the pinned Rust toolchain, the
# system libraries GPUI needs to build on Linux, git for argus_git, the Claude
# Code CLI for argus_runs, and CPU rendering plus a virtual display so the
# window can be opened and screenshotted with no GPU attached.
# Runbase starts the agent itself, so there is no entrypoint.
#
# The tag must match `channel` in rust-toolchain.toml; otherwise rustup
# downloads the pinned toolchain again at the start of every run.
FROM rust:1.98.1-trixie

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    # Building: C/C++ sources from the -sys crates, rusqlite's bundled SQLite
    # and the tree-sitter grammars go through `cc`; pkg-config finds the libs.
    build-essential pkg-config \
    # argus_git shells out to the git CLI for worktrees, commit, rebase, push.
    git \
    # GPUI text: font-kit links fontconfig and freetype.
    libfontconfig-dev libfreetype-dev \
    # GPUI windowing on X11 and Wayland, and keyboard handling.
    libx11-dev libxcb1-dev libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev \
    # GPUI desktop integration (keyring via oo7, notifications).
    libdbus-1-dev libssl-dev \
    # Rendering without a GPU. The sandbox has no GPU attached, so wgpu runs
    # on Mesa's CPU drivers: lavapipe for Vulkan (preferred), llvmpipe over
    # EGL/GL as the fallback. Xvfb is the X server the window draws into.
    libvulkan1 mesa-vulkan-drivers libegl1 libgl1-mesa-dri xvfb xauth \
    # Diagnostics: `xvfb-run -a vulkaninfo --summary` should list llvmpipe.
    vulkan-tools \
    # Screenshots of the virtual display: `scrot shot.png`.
    scrot \
    # Fonts for fontconfig; without any, GPUI has nothing to shape text with.
    fonts-dejavu-core fonts-noto-core \
    # Node runtime for the Claude Code CLI below.
    nodejs npm \
 && rm -rf /var/lib/apt/lists/*

# argus_runs turns each comment into a headless `claude -p` run, so the CLI
# must be on PATH to exercise runs end to end. Pass
# `--build-arg CLAUDE_CODE_VERSION=x.y.z` to pin a release. It is installed
# globally as root, where the Runbase user can't update it, so the
# auto-updater is turned off.
ARG CLAUDE_CODE_VERSION=latest
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force \
 && claude --version
ENV DISABLE_AUTOUPDATER=1

# GPUI picks Wayland when WAYLAND_DISPLAY is set and X11 when DISPLAY is, so
# the app must run under Xvfb, e.g.:
#   xvfb-run -a -s "-screen 0 1920x1080x24" cargo run -p argus
# and, from another command in the same xvfb-run, `scrot shot.png` to look at
# it. Software rendering is slow but correct; it is for checking the UI, not
# for measuring frame times.
ENV LIBGL_ALWAYS_SOFTWARE=1

# Every sandbox starts with an empty target/, so dependencies are compiled cold
# on each run. Skipping debug info for dependencies keeps that build shorter and
# target/ smaller. This file applies only in the sandbox.
RUN printf '%s\n' \
    '[profile.dev.package."*"]' \
    'debug = false' \
    > "$CARGO_HOME/config.toml"

# Runbase clones the repository and runs as its own user, so the toolchain
# and the crate cache must be writable by any user.
RUN rustup component add clippy rustfmt \
 && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"
