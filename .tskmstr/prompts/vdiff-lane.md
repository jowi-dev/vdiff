# vdiff work lane

Work the GitHub issue named at the end of this prompt, in this repository,
and nothing else. Note unrelated bugs or cleanups as follow-up issues rather
than fixing them here.

## Start

Run `tm ready <KEY>` and branch on its exit code:

- `0` (ready): proceed normally.
- `3` (stackable): proceed. The issue has exactly one unmerged blocker, that
  blocker has an open PR, and `tm work run` already cut your branch from the
  blocker's branch — its work is in your worktree. Target your PR at the
  blocker's branch, not `main`.
- `1` (blocked): stop without changing anything. Comment the blocker on the
  issue (`tm ticket comment <KEY>`) and say why you stopped.

Then move the issue to In Progress (`tm ticket transition <KEY>`) and read it
in full. If the description leaves the intended behavior ambiguous, write
your questions to the issue as a comment and stop — there is nobody to ask,
and a stopped run with a clear written question is a better outcome than a
guessed-at implementation.

## What this codebase is

A Rust TUI/GUI for visual PR review: a change set rendered as a layered
dependency graph, navigated with vim keys, with an embedded Neovim for
reading and editing files in place. Two frontends sit behind independent
cargo features, both default-on: `gui` (egui/eframe canvas) and `tui`
(ratatui/crossterm). `--no-default-features` must stay a fully headless
build — no window, no terminal UI, and none of egui/eframe/ratatui/syntect in
the dependency tree.

Before changing anything, say which layer you are working in — diff/graph
pipeline, TUI, GUI, or the headless CLI entry points — and whether the change
crosses a feature gate. A change that touches shared code but only compiles
under one feature is the recurring hazard here.

## Workflow

- TDD: write the failing test first, watch it fail for the right reason, then
  write the minimum code that makes it pass.
- Checkpoint commits. One logical change per commit, imperative mood, and the
  tree compiles with tests green at every commit. Do not save up one large
  commit at the end.
- Reuse before you add. Search the module you are working in for existing
  helpers before writing new ones.
- Update the docs that describe what you changed: `README.md` for
  user-visible flags and behavior, `docs/comments-schema.md` and
  `docs/findings-schema.md` when a serialized shape changes, and module docs
  when a module's purpose moves.
- Delegate the reading and the mechanical edits. Use the Explore agent to map
  unfamiliar areas rather than sweeping the repo yourself, and subagents for
  well-specified implementation work; your job is to plan, review their
  diffs, and verify.
- Push back where you disagree. This prompt and the issue are a best
  understanding, not gospel — write disagreements into the PR's Concerns
  section rather than silently working around them.

## Before finishing

All four must be green, in this order:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo check --no-default-features
```

The last one is not optional: it is the guard that keeps the headless build
compiling, and it is the check most easily broken by a change that looks
fine under the default features.

## Finish

Open one PR for this issue with `tm pr create`. In the description include:

- What changed and why, in two or three sentences
- A "Concerns" section with anything you disagreed with or would have done
  differently
- Anything you could not verify

If you become blocked at any step, comment the blocker on the issue and stop.
