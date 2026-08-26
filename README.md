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

Two Cargo features gate the two frontends, both on by default and
independent of each other: `gui` (the egui/eframe graph-canvas GUI) and
`tui` (a ratatui/crossterm terminal UI -- `--tui` -- showing a `git log
--graph`-style vertical rail DAG of every visible module, rather than the
2D graph canvas). A
`--no-default-features` build is fully headless: no window or terminal UI
ever opens, and `--dump`/`--export-comments`/`--publish-comments` are the
only usable entry points -- any invocation that would otherwise launch a
frontend (bare `vdiff`, `--tui`, `--smoke`, `--pr` without a headless flag,
...) exits 1 with a message naming the missing feature instead.

```sh
cargo build --release --no-default-features            # headless CLI only, no egui/eframe/ratatui/syntect in the dependency tree
cargo build --release --no-default-features --features tui  # terminal UI only, no egui/eframe
cargo check --no-default-features                        # verify the headless build stays compiling as CI/local check
```

## Quickstart

```sh
vdiff                  # open the graph for the current repo's change set; file panes are a real embedded Neovim
vdiff --no-nvim        # same, but file panes use the built-in read-only viewer instead
vdiff --base main      # diff against a specific ref instead of the detected default branch
vdiff --tui            # terminal UI instead: a nested 2D graph of the whole change set
```

The terminal UI (`--tui`) reuses the same diff/file panes as the GUI --
including the embedded Neovim, on the same `--no-nvim` opt-out -- but its
graph screen has three interchangeable views, cycled with backtick
(`` ` ``) in `plane -> canvas -> rail -> plane` order -- all three share the
same fold-by-namespace "zoom out" mechanic (`h`/`l` in rail mode, `zc`/`zo`
in canvas/plane mode) and every other binding (`gd`/`gr`, `gt`, `t`, `v`,
`c`, `Enter`, `d`, `q`, `Esc`):

- **Plane** (the default) -- a true 2D nested layout: expanded namespaces
  render as `╭─ Name ─╮` boxes containing their children, spread across
  both dimensions, with orthogonally-routed dependency edges between
  labels. `h`/`j`/`k`/`l` move focus spatially.
- **Canvas** -- a semantic-zoom Sugiyama layout: horizontal bands of node
  labels (one band per dependency layer), with routed inter-band channels.
  `h`/`j`/`k`/`l` move focus spatially; horizontal scrolling auto-pans to
  follow focus.
- **Rail** -- a `git log --graph`/`jj log`-style vertical scroll: one row
  per visible module, top to bottom in dependency-layer order, with a rail
  gutter on the left drawing the dependency edges between rows. `j`/`k`
  move down/up the row list; `h`/`l` collapse/expand the focused row's
  namespace directly (no `z` chord).

All three keep vdiff's change sets (usually 15-40 visible modules) readable
without folding anything by default. The hand-rolled read-only file viewer
and unified/side-by-side diff screen, plus `Ctrl-e`'s suspend-and-run
handoff to a real `nvim` process (lazygit-style, resuming when it exits),
only come into play as a fallback -- when `--no-nvim` was given, no `nvim`
binary is on `PATH`, or the embedded session failed to spawn. Quitting the
embedded session (`ZZ`, `:q`) is not one of those cases: it returns you to
the graph, and the next `Enter`/`d` spawns a fresh session (re-running any
`--nvim-cmd` commands) rather than downgrading the rest of the run.

`--pr <url>` (reviewing a GitHub PR directly) is on the roadmap, not
available yet.

## Keys

GUI (`h`/`j`/`k`/`l` move within/between the graph's dependency layers;
zoom is a 2D-canvas-only concept):

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

`--tui`'s graph screen differs on `` ` ``/`h`/`j`/`k`/`l`/fold only
(everything else above still applies, `Enter`/`d`/`t`/`c`/`gd`/`gr`/`Esc`/
`Ctrl-w h/l` included):

| Key(s)   | Does                                                          |
|----------|-----------------------------------------------------------------|
| `` ` ``  | Cycle graph view: plane -> canvas -> rail -> plane               |
| `h` `j` `k` `l` | Move focus spatially (plane/canvas) or fold/step rows (rail) |
| `zc` `zo`| Collapse/expand the focused namespace (plane/canvas only)       |
| `h` `l`  | Collapse/expand the focused row's namespace (rail only)         |

## Tuning your Neovim config for the review pane

The embedded session runs your own config, chrome and all — and a context
panel or file tree that earns its columns while you're writing code usually
just squeezes the diff while you're reading one. Two ways to turn things off
inside vdiff without touching how you edit normally:

```sh
vdiff --nvim-cmd ContextPanelHide   # one Ex command per flag, repeatable
```

Or, from your own config, key off the session vdiff announces once it has
finished setting up (`vim.g.vdiff` is also set, for anything that just wants
to check). This one is the more reliable of the two if a plugin re-opens its
window later in the session — you can hide it whenever you need to, not just
once at startup:

```lua
vim.api.nvim_create_autocmd("User", {
  pattern = "VdiffSessionStart",
  callback = function()
    vim.cmd("ContextPanelHide")
  end,
})
```

Both run again after every respawn, so quitting a file's session and opening
the next one doesn't bring the chrome back.

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
