# GitHub review-threads dump schema (v1)

`vdiff --dump json --include-threads` adds the PR's GitHub review threads to
the `--dump json` envelope, grouped by graph node. It's the headless
counterpart of the thread badges and thread panel in the GUI and TUI, meant
for scripts and review agents that want to know which modules a reviewer's
comments touch.

Threads are fetched through `gh` (which must be installed and
authenticated) for `--pr <n>`'s PR, or for the PR of the checked-out branch.
Nothing is cached: every run asks GitHub. A failed fetch (no PR for the
branch, no `gh`, no network) exits 1 with the reason on stderr. When the
local `HEAD` isn't the PR's head commit, a warning goes to stderr, since
thread lines are relative to the PR head and may have drifted.

## Shape

The envelope gains a `"threads"` key next to `"graph"` (and `"diffs"`, with
`--include-diffs`). Without the flag the key is absent, not `null`.

```json
{
  "graph": { "...": "..." },
  "threads": {
    "pr_number": 35,
    "head_oid": "4569079c2f...",
    "summaries": [
      { "author": "reviewer", "state": "CHANGES_REQUESTED", "body": "Needs work.\nSee threads." }
    ],
    "nodes": {
      "rust:vdiff::review::gh_threads": [
        {
          "id": "PRRT_kwDO...",
          "path": "src/review/gh_threads.rs",
          "line": 42,
          "is_resolved": false,
          "is_outdated": false,
          "comments": [
            { "author": "reviewer", "body": "Why not paginate?" },
            { "author": "jowi-dev", "body": "100 covers every PR so far." }
          ]
        }
      ]
    },
    "unmatched": []
  }
}
```

## Fields

| Field | Type | Meaning |
|---|---|---|
| `pr_number` | integer | The PR the threads belong to. |
| `head_oid` | string | The PR's head commit. Thread lines are relative to it. |
| `summaries` | array | Top-level review bodies, with no file anchor. Reviews with an empty body are left out. |
| `summaries[].state` | string | GitHub's review state: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, or `PENDING`. |
| `nodes` | object | Node id to the threads on any of that node's files. A thread on a file shared by two nodes appears under both. Nodes with no threads are absent. |
| `unmatched` | array | Threads whose file isn't in the dumped graph. |
| `id` | string | GitHub's GraphQL node id for the thread. |
| `path` | string | Repo-relative path the thread is anchored to. |
| `line` | integer or `null` | 1-based line on the PR head. `null` when GitHub can no longer place the thread. |
| `is_resolved` | boolean | The thread was marked resolved on GitHub. |
| `is_outdated` | boolean | Later commits changed the anchored lines. |
| `comments` | array | The opening comment, then replies, in order. A deleted account's `author` is `ghost`. |

## Limits

At most 100 threads, 100 reviews, and 100 comments per thread are fetched.
There is no pagination yet.
