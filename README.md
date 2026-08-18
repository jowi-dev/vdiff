# vdiff

Visual PR review: a branch's change set rendered as a layered dependency
graph, navigable with vim keys, with a real embedded Neovim for reading and
editing files in place. Instead of a linear list of changed files, vdiff
lays out the modules/files touched by a change (plus what connects them) in
layers by dependency depth, so you can see what a change affects and how,
before diving into any one file.

<!-- screenshot: graph view of a change set, plus the embedded-nvim file pane -->

## Status

Young. It works, but the surface area is small and the diff/graph pipeline
so far only understands Rust and Elixir. Expect rough edges.

## Install

Build from source with Cargo:

```sh
cargo build --release
```

Or, if you use Nix flakes:

```sh
nix build
# or, for a dev shell with the toolchain (and nvim) available:
nix develop
```

## Quickstart

```sh
vdiff                  # open the graph for the current repo's change set; file panes are a real embedded Neovim
vdiff --no-nvim        # same, but file panes use the built-in read-only viewer instead
vdiff --base main      # diff against a specific ref instead of the detected default branch
```

`--pr <url>` (reviewing a GitHub PR directly) is on the roadmap, not
available yet.

## Keys

| Key(s)              | Does                                                        |
|---------------------|--------------------------------------------------------------|
| `h` `j` `k` `l`      | Move focus around the graph / scroll a file pane             |
| `Enter`              | Open the focused node's file                                  |
| `d`                  | Diff the current file against the merge-base                 |
| `t`                  | Toggle showing test modules                                   |
| `c`                  | Comment on the focused node (see below — requires `vdiff.nvim`) |
| `gd` / `gr`          | Follow dependencies / dependents from the focused node        |
| `+` / `-` / `=`      | Zoom in / out / reset                                          |
| `Esc`                | Back out (close file pane, close diff, ...)                    |
| `Ctrl-w h` / `Ctrl-w l` | Move focus between the graph and file panes                |

## Review comments

vdiff itself doesn't capture comments — that's the job of a companion
Neovim plugin, [`vdiff.nvim`](https://github.com/jowi-dev/vdiff.nvim). Since
the default embedded-nvim mode runs your own Neovim config, installing that
plugin normally makes `:VdiffComment` and the `c` key work inside vdiff
automatically, with no extra wiring. Comments are stored at
`<git-dir>/vdiff/comments.json` — see
[`docs/comments-schema.md`](docs/comments-schema.md) for the exact format.

vdiff reads that store back out with:

```sh
vdiff --export-comments   # print every captured comment as markdown
```

## AI-review payload

For feeding a change set to an LLM reviewer:

```sh
vdiff --dump json --include-diffs
```

Dumps the graph as JSON with each node's diff content inlined, instead of
launching the GUI.
