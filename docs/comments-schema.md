# Review-comments schema (v1)

`vdiff` and [`vdiff.nvim`](https://github.com/jowi-dev/vdiff.nvim) share one
on-disk format for review comments captured while reviewing a change set.
`vdiff.nvim` is the writer; `vdiff` is the reader (`vdiff --export-comments`).
This document is the contract between them — neither project changes this
format without updating both.

## Location

```
<git-dir>/vdiff/comments.json
```

`<git-dir>` is the repository's actual git directory, from `git rev-parse
--git-dir` (or equivalent) resolved against the reviewed worktree — not
`<worktree-root>/.git` joined by hand, which breaks under `git worktree add`
or submodules where `.git` is a gitlink file rather than a directory. Storing
comments under the git dir keeps them out of `git status`/diffs of the
reviewed repo, and gives each worktree its own independent comment store.

## Shape

The file is a JSON array of comment objects, pretty-printed, sorted by
`(path, start_line)`:

```json
[
  {
    "id": "c1",
    "path": "src/lib.rs",
    "start_line": 3,
    "end_line": 5,
    "text": "Does this need a null check?",
    "created_at": "2026-08-17T14:32:00Z"
  },
  {
    "id": "c2",
    "path": "src/lib.rs",
    "start_line": 40,
    "end_line": 40,
    "text": "Architecture note: this module should own retries.",
    "node": "rust:my_crate::lib",
    "created_at": "2026-08-17T14:35:12Z"
  }
]
```

### Fields

| Field         | Type   | Notes                                                                                          |
|---------------|--------|--------------------------------------------------------------------------------------------------|
| `id`          | string | Short, stable identifier: `"c<n>"`, one more than the highest existing `n`. Lets a comment be referenced (e.g. manual deletion by hand-editing the JSON) without relying on array position. |
| `path`        | string | Repo-relative path the comment is anchored to.                                                  |
| `start_line`  | number | 1-based, inclusive: first line of the commented range.                                          |
| `end_line`    | number | 1-based, inclusive: last line of the commented range. Equal to `start_line` for a single line.   |
| `text`        | string | The comment body. Multi-line text is valid even though a given compose UI may only produce single-line text. |
| `node`        | string | *Optional* — omitted entirely (not `null`) when absent. Set only for a node-level ("architecture") comment anchored to a whole graph node rather than a specific line range. |
| `created_at`  | string | ISO-8601 UTC timestamp, set once at creation, never updated.                                     |
| `resolved_at` | string | *Optional* — omitted entirely (not `null`) when the comment is unaddressed. ISO-8601 UTC timestamp set when the comment is marked addressed; cleared (field removed again) if it's un-marked. |

Ordering and pretty-printing are part of the contract, not incidental: the
store is always re-sorted and re-serialized in `(path, start_line)` order on
every save, so a `git diff` of `comments.json` (if anyone chooses to track
it) stays readable instead of reordering itself on every write.

## Division of responsibility

- **`vdiff.nvim`** owns *writing*: `:VdiffComment`, its compose UI, and the
  `require('vdiff').comment_range(start_line, end_line, {node = ...})` API
  vdiff's embedded Neovim session calls for graph-node ("architecture")
  comments. It also owns comment creation, text edits, and deletion, plus
  rendering comment extmarks in the buffer.
- **`vdiff`** owns *reading*, plus exactly one narrow write: `resolved_at`
  toggling. `vdiff --export-comments` renders the store as markdown, and the
  graph pane's `m` key toggles a comment's (or a node's comments')
  `resolved_at` between `Some(now)` and `None` (issue #14) — the only field
  vdiff itself ever mutates. It never touches `text`, never creates or
  deletes a comment.

**Lockstep requirement:** because both projects write to the same file,
`vdiff.nvim` must round-trip `resolved_at` (and any other field it doesn't
recognize) unchanged whenever it re-saves a comment it didn't itself
resolve — losing it on a save from the nvim side would silently un-resolve
a comment vdiff had just marked addressed.

## Versioning

This is schema **v1**. A future incompatible change bumps this document
(and, if the shape itself needs to change, an explicit `schema` field would
be added to disambiguate old files) rather than silently breaking either
project's reader/writer.

## Status summary output

`vdiff --comments-status` prints a single pretty-printed JSON object to
stdout and exits 0 -- a small, stable read surface for external tooling
(a ticket board like tm/tskmstr, say) that wants to know whether a
branch's review comments are addressed, without parsing `comments.json`
itself:

```json
{
  "repo": "vdiff",
  "branch": "main",
  "total": 3,
  "unresolved": 2
}
```

| Field        | Type   | Notes                                                                 |
|--------------|--------|------------------------------------------------------------------------|
| `repo`       | string | Repo display name -- the reviewed worktree's directory name, same derivation as `--export-comments`'s markdown header. |
| `branch`     | string | Current branch name (`HEAD`'s shorthand, or `"HEAD"` for a detached head). |
| `total`      | number | Total number of comments in the store, resolved or not.               |
| `unresolved` | number | Comments with no `resolved_at` set -- still unaddressed.               |

A missing store (no comments captured yet) reports `total: 0, unresolved:
0` rather than erroring; a corrupt store is a fatal CLI error, same as
`--export-comments`. This is the intended read surface for tooling that
only needs the counts -- prefer it over parsing `comments.json` directly,
which is free to gain fields this summary doesn't expose.
