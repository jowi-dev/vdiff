# Review-findings schema (v1)

`vdiff --dump json --include-diffs` is the AI-review **input** payload: a
review agent (any LLM-backed process, not a specific product) reads the
project graph plus per-node diffs and produces findings. `vdiff --findings
<path>` closes the loop in the other direction: it reads that agent's
**output** and renders it on the graph, the focus overlay, and the built-in
file pane. This document is the contract between whatever produces
`findings.json` and `vdiff`'s reader -- an agent prompt built against this
schema should keep working across `vdiff` versions as long as the version
number below doesn't change.

## Shape

The file is a JSON array of finding objects. Order doesn't matter --
`vdiff` re-groups findings by node at load time.

```json
[
  {
    "node_id": "rust:vdiff::graph::layout",
    "severity": "high",
    "summary": "layout() can panic on an empty layer",
    "detail": "layer_extent's fold assumes at least one rect; a layer that survives filtering but loses every node to a later prune isn't defended against."
  },
  {
    "path": "src/pipeline/file_diff.rs",
    "line": 42,
    "severity": "medium",
    "summary": "Silent truncation on non-UTF8 content"
  }
]
```

### Fields

| Field     | Type   | Notes                                                                                                                     |
|-----------|--------|-----------------------------------------------------------------------------------------------------------------------------|
| `node_id` | string | *Optional.* A graph node id exactly as it appears in `--dump json` (e.g. `"rust:vdiff::graph::layout"`). Takes priority over `path` when both are set. |
| `path`    | string | *Optional.* Repo-relative source path. Used to resolve a node when `node_id` is absent. A path backing more than one node (an Elixir file with several `defmodule`s) attaches the finding to every one of them. |
| `line`    | number | *Optional.* 1-based line within `path` the finding is about. Only meaningful alongside `path`; ignored for a `node_id`-only finding since a node id doesn't identify one specific file. |
| `severity`| string | One of `"low"`, `"medium"`, `"high"` (lowercase). Required.                                                                |
| `summary` | string | Required. A short, one-line description -- shown in the focus overlay and the file pane's inline marker. Keep it to one line; there's no wrapping/scrolling in either surface. |
| `detail`  | string | *Optional.* A longer explanation. Not rendered by `vdiff` yet -- carried through so a future detail view doesn't need a schema change to read it. |

At least one of `node_id`/`path` **must** be set -- a finding anchored to
neither can never be attached to a node, and `vdiff` refuses to load the
whole file rather than silently drop it (see "Validation" below).

## Mapping to nodes

- A finding with `node_id` set attaches directly to that node.
- A finding with only `path` set attaches to every node whose backing files
  include that path.
- **Unknown `node_id` or unmatched `path`:** not fatal. Findings are
  commonly generated against the full `--dump json --include-diffs`
  payload, while the graph `vdiff` actually renders may be narrower (the
  default `focus_on_changes` view, or a different `--base`/`--pr`
  resolution than the one the agent ran against) -- a finding naming
  something outside that view is an expected mismatch, not a contract
  violation. `vdiff` silently omits it from the graph rather than refusing
  to start; run with `--all` if you expect a finding to appear and don't
  see it.
- **Malformed JSON, or an entry with neither `node_id` nor `path`:** fatal.
  This is a contract violation an agent's prompt should never produce, and
  limping past it (dropping the bad entry, say) would hide a broken
  pipeline instead of surfacing it. `vdiff` exits with a one-line error
  naming the failing entry's index and refuses to start.

## Rendering

- **Graph node badge:** a node with any attached findings gets a small
  count + severity-color badge (red `high`, orange `medium`, yellow `low` --
  the *highest* severity present picks the color when a node has more than
  one finding), painted alongside the existing changed-tests checkmark
  badge and the reviewed-dimmed fill.
- **Focus overlay:** when the focused node has findings, their summaries
  are listed, each tagged with its severity.
- **File pane:** when the currently open file has findings with a `line`,
  each is marked at that line. This is intentionally minimal (a gutter
  marker/annotation row, no interaction) -- there's no scrolling-to-finding
  or detail popup yet.
- Findings with no `line` (file/node-level) show up in the graph badge and
  focus overlay but have nothing to mark in the file pane.

## Versioning

This is schema **v1**. A future incompatible change bumps this document
(and, if the shape itself needs to change, an explicit `schema` field would
be added to disambiguate old files) rather than silently breaking either
side of the contract.
