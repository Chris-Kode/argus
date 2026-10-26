# Argus

Review a branch, comment on lines, and let Claude Code turn each comment into a
code change. Every change runs in its own git worktree and lands on your branch
only when you accept it.

![Comment threads in the side-by-side diff](docs/screenshots/comments-light.png)

## Get started

### 1. Install what Argus needs

- **macOS 11 or later** (Apple silicon or Intel).
- **git 2.40+** — `xcode-select --install` or `brew install git`.
- **[Claude Code](https://claude.com/claude-code)** — install it, run `claude`
  once in a terminal and sign in. Argus uses that login and stores no API keys.

### 2. Download Argus

1. Download **`Argus-<version>-macos-universal.dmg`** from the
   [latest release](https://github.com/Chris-Kode/argus/releases/latest).
2. Open it and drag **Argus** to **Applications**.
3. Open Argus. The first time, macOS blocks it because the app is not notarized
   yet: go to **System Settings → Privacy & Security** and click
   **Open Anyway**. Or run once:

   ```sh
   xattr -dr com.apple.quarantine /Applications/Argus.app
   ```

Argus updates itself: when a new version is out, **Update** appears in the
title bar.

### 3. Review your first branch

1. **Open a repository** with `⌘O`.
2. **Pick the branches** in the title bar: base (e.g. `main`) and the branch
   under review.
3. **Comment** — hover a line and click **+** (or use the comment button in a
   file's header for the whole file), write what you want changed and save with
   `⌘↩`.
4. **Run** it with Claude (`r`). Keep commenting while it works; `⌘J` shows the
   live log.

   ![A live run in the run panel](docs/screenshots/runs-light.png)

5. **Review the proposal** — click **View proposal** to see exactly what Claude
   changed. Reply to ask for more, or press `x` twice to reject it.

   ![A proposal tab](docs/screenshots/proposal-light.png)

6. **Accept** (`a`) to turn it into one local commit, with a message Claude
   drafts for you.
7. **Push** from the title bar when you are ready.

## Good to know

- **Your checkout is safe.** Claude works only in worktrees under
  `~/.argus/worktrees/` and never touches your uncommitted changes.
- **Nothing is committed until you accept**, and nothing is pushed until you
  click Push. Argus never force-pushes.
- **Everything Argus stores** lives in `~/.argus/`. Add your own instructions
  for every run in `~/.argus/review-rules.md`.
- **`⌘/`** lists every keyboard shortcut, **`⌘,`** opens the settings (models,
  themes, hidden files and more).

The [guide](docs/guide.md) covers every feature in detail.

## Build from source

```sh
git clone https://github.com/Chris-Kode/argus.git
cd argus
cargo run -p argus
```

Needs [rustup](https://rustup.rs); the pinned toolchain installs itself. See
[docs/development.md](docs/development.md) for bundling the app, signing and Linux
builds.

## License

Apache-2.0, see [LICENSE](LICENSE). Bundles
[IBM Plex](https://github.com/IBM/plex) fonts
([OFL 1.1](crates/argus_ui/assets/fonts/OFL.txt)) and
[Phosphor Icons](https://github.com/phosphor-icons/core)
([MIT](crates/argus_ui/assets/icons/LICENSE)); the Argus eye mark is original
to this project.
