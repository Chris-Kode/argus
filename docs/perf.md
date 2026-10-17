# Performance harness

Speed on very large diffs is why Argus exists. This page covers how to
reproduce the measurements for the targets in spec §12 (issue #1), and the
latest results.

| Scenario (spec §12) | Target | Measured by |
| --- | --- | --- |
| Open a diff with 5,000 changed files | File tree visible < 500 ms | `ARGUS_PERF` on `huge-files`: *file tree first paint* |
| Open a single file with a 100,000-line diff | First paint < 100 ms | `ARGUS_PERF` on `huge-file`: *first diff paint* after the file list |
| Scrolling in diff view | 120 fps, no frame > 16 ms | `--auto-scroll`: frame build and frame interval |
| Syntax highlight of visible region | < 50 ms after scroll stops | `--auto-scroll`: rows on screen loaded and highlighted after the stop |
| 10 parallel runs streaming | UI frame time < 16 ms | not automated (needs `claude`); see [Gaps](#gaps) |
| Idle memory with 5,000-file diff open | < 500 MB | `ARGUS_PERF` report: peak RSS |

## 1. Fixture repositories

```sh
cargo xtask fixtures                    # all three, into target/fixtures/
cargo xtask fixtures --only huge-file   # just one (same SHAs as in a full run)
cargo xtask fixtures --seed 0x1234      # another seed, other SHAs
cargo xtask fixtures --out /tmp/fx      # another directory
```

The generator lives in [`xtask/`](../xtask). It streams commits into
`git fast-import` with fixed identities and timestamps, runs git with the
user and system config disabled, and draws all content from a hand-rolled
seeded PRNG. The same seed therefore always yields the same commits, so the
same SHAs. Existing fixtures are deleted and rebuilt on every run. Each
fixture has `base` and `head` branches, with `head` checked out.

| Fixture | Content |
| --- | --- |
| `huge-files` | `base` has 6,230 files in 420 nested directories (depth 1–6) over ten languages (Rust, TypeScript, Python, Go, C, Java, Markdown, YAML, JSON, TOML), plus PNG binaries. `head` changes **5,000** files: 3,990 edited, 10 mode-only (`+x`), 20 binaries changed, 490 text + 10 binaries added, 250 deleted, 130 pure renames, 100 renames with an edit. |
| `huge-file` | One 100,000-line Rust file with every fourth line rewritten: a single hunk of ~125,000 diff lines (`+25,000 −25,000`). |
| `many-branches` | A small `main`, **5,000** one-commit branches off it under `topic/`, `feature/`, `fix/`, `user/dev/` and `release/` (refs packed, as in a real clone), plus `base` and `head`. |

Generation takes about 12 s for all three, in a debug build on the CI
sandbox.

### Determinism check

Every fixture writes `target/fixtures/<name>.manifest` with the seed, the
`base`/`head` SHAs, the ref count and a hash of every ref. Generate twice and
compare:

```sh
cargo xtask fixtures && cat target/fixtures/*.manifest > /tmp/run1
cargo xtask fixtures && cat target/fixtures/*.manifest > /tmp/run2
diff /tmp/run1 /tmp/run2 && echo identical
```

With the default seed (`0xa26e55`):

| Fixture | `base` | `head` |
| --- | --- | --- |
| `huge-files` | `34843792b194e9e1bbb8ec30aa460a9f136e4b3d` | `be5cafed580a8b584455dfb4db820fc0c511c9e7` |
| `huge-file` | `225c7fcddcfb0a18522873bfa3af3fc15b231a6a` | `3875adef671e656e7299e7c886dd7d7592940470` |
| `many-branches` | `4e8c5a613c7fe40a635418f27ddfe07372551f75` | `e5e8a0b38492e1b4e093962436a92565b15d67ad` |

If these change, the generator changed. That is fine on purpose (update this
table), never by accident.

## 2. UI timing mode (`ARGUS_PERF=1`)

```sh
ARGUS_HOME=/tmp/argus-perf ARGUS_PERF=1 cargo run --release -- \
  --repo target/fixtures/huge-files --base base --head head --auto-scroll
```

- `--repo/--base/--head` open that comparison instead of the last
  repository. They work without `ARGUS_PERF` too.
- `ARGUS_HOME` keeps the fixtures out of your real `~/.argus` recent list.
- Without `--auto-scroll`, the startup part is printed once the first diff
  paints, and the app stays open. `--auto-scroll` implies `ARGUS_PERF=1`.
  After the first diff paint it waits 1 s, then scrolls the diff by 48 px
  every frame until its end or 1,200 frames. It then prints the report and
  quits.

The report goes to stdout and to `<ARGUS_HOME>/logs/argus.log`. It shows:

| Line | Meaning |
| --- | --- |
| *window first frame*, *branches listed*, *file list computed*, *file tree first paint*, *first diff paint* | ms after the workspace starts opening the repository (ms after process start in parentheses). "Paint" milestones are stamped at the end of the frame that first drew file-tree rows, or lines of a loaded file. |
| *frame build* | Per scroll frame, from the root view's render to the end of the window's paint: the CPU cost of the frame (layout, prepaint, paint), excluding GPU submission. |
| *frame interval* | Per scroll frame, paint to paint: what the user sees as frame time, including waits for vsync and the GPU. At 120 Hz the floor is 8.3 ms. |
| *rows on screen loaded and highlighted* | After the scroll stops, how long until every file on screen has no load and no highlight job in flight. |
| *peak RSS* | `getrusage` maximum resident set size of the process. |
| *spec §12 targets* | `ok` / `MISS` against the table above. *First diff paint* is counted from the file list being known, the closest thing to "open a single file". |

The probe lives in [`crates/argus_ui/src/perf.rs`](../crates/argus_ui/src/perf.rs).
Without `ARGUS_PERF` it is not installed, and every hook is a no-op.

## 3. Criterion benches (not included)

The issue also asks for Criterion benches of the non-UI parts:

- file list and single-file diff (#7)
- branch listing (#4)
- highlighting a 5k-line file (#11)
- `stream-json` parsing of 10k events (#20)

They are **not** in this change. [AGENTS.md](../AGENTS.md) hard rule 1
forbids benches, as well as tests and test-only fixtures, so `cargo bench`
has nothing to run. If the owner lifts that rule for benches, the fixtures
above are ready to use as inputs. Until then, the `ARGUS_PERF` milestones
cover two of them end to end: *branches listed* (on `many-branches`) and
*file list computed* (on `huge-files`).

## 4. Results

### Reference machine (Apple M-series, release build)

**Not measured yet.** Spec §12 targets an Apple M-series laptop; run the
three commands below on one and fill in this table:

```sh
for f in huge-files huge-file many-branches; do
  ARGUS_HOME=/tmp/argus-perf ARGUS_PERF=1 cargo run --release -- \
    --repo target/fixtures/$f --base base --head head --auto-scroll
done
```

| Scenario | Target | Result |
| --- | --- | --- |
| File tree visible, 5,000 files | < 500 ms | — |
| First paint, 100,000-line diff | < 100 ms | — |
| Scroll frames | no frame > 16 ms | — |
| Highlight after scroll stops | < 50 ms | — |
| Peak memory, 5,000-file diff | < 500 MB | — |

### CI sandbox (2026-10-17, not representative)

Linux container, 4 vCPU of an AMD Ryzen 9 9950X, 7 GB RAM. No GPU: the window
is Xvfb and Vulkan is Mesa lavapipe, a **software** renderer, so frame times
here measure the CPU rasterizer, not Argus. Use these only to check that the
harness works and to compare runs on the same box. Values are from the
second of two runs (warm page cache):

| Measurement | `huge-files` | `huge-file` | `many-branches` |
| --- | --- | --- | --- |
| branches listed | 192 ms | 141 ms | 217 ms |
| file list computed | 305 ms | 146 ms | 274 ms |
| file tree first paint (target < 500) | **365 ms** | 180 ms | 297 ms |
| first diff paint after list (target < 100) | 256 ms | **294 ms** | 150 ms |
| frame build p50 / p95 / max | 19 / 30 / 50 ms | 17 / 26 / 51 ms | (6 frames) |
| frame interval p50 / p95 / max | 73 / 99 / 132 ms | 66 / 91 / 141 ms | (6 frames) |
| loaded + highlighted after stop (target < 50) | 71 ms | 63 ms | 67 ms |
| peak RSS (target < 500) | **260 MB** | 239 MB | 221 MB |

The first, cold-cache run of `huge-files` painted the file tree at 1.3 s.

## Gaps

These have to be confirmed on the reference machine before follow-up issues
are opened. The sandbox cannot judge GPU-bound numbers.

- **First diff paint on `huge-file`** misses 100 ms here (about 300 ms from
  the file list to the first lines on screen). This includes diffing 100k
  lines and laying out the ~125k-row hunk off the UI thread.
- **Scroll frame times** miss 16 ms here, but under a software renderer; the
  frame *build* p50 (17–19 ms CPU) is the number to watch on real hardware.
- **Highlight after scroll stop** is 60–90 ms here, against a 50 ms target.
- **10 parallel runs streaming** is not automated: it needs a signed-in
  `claude`. A follow-up could drive the run panel with a fake `claude` that
  replays `stream-json`, and record frame times the same way.
- **Criterion benches**: blocked on the AGENTS.md rule (section 3).
