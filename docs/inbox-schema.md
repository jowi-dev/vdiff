# Review-inbox schema (v1)

`vdiff --inbox --json` prints the review inbox (see the README's "Review
inbox" section) as one JSON object on stdout, for other tools -- a board, a
tmux picker, a status line -- to consume without depending on vdiff's
internals. The output is recomputed from GitHub on every run; vdiff stores
nothing between runs.

## Shape

```json
{
  "scope": "owner/repo",
  "total": 2,
  "counts": { "review_requested": 1, "new_commits": 1, "replies": 0 },
  "entries": [
    {
      "repo": "owner/repo",
      "number": 42,
      "title": "Add retry to the uploader",
      "author": "alice",
      "url": "https://github.com/owner/repo/pull/42",
      "updated_at": "2026-10-06T11:59:07Z",
      "category": "review_requested",
      "review_requested": true,
      "head_changed_since_review": false,
      "new_commits_since_review": null,
      "unanswered_replies": 0,
      "reason": "review requested"
    },
    {
      "repo": "owner/repo",
      "number": 40,
      "title": "Rename partners to markets",
      "author": "bob",
      "url": "https://github.com/owner/repo/pull/40",
      "updated_at": "2026-10-05T18:23:28Z",
      "category": "new_commits",
      "review_requested": false,
      "head_changed_since_review": true,
      "new_commits_since_review": 3,
      "unanswered_replies": 2,
      "reason": "3 new commits since your review, 2 replies"
    }
  ]
}
```

## Top-level fields

| Field | Type | Meaning |
|---|---|---|
| `scope` | string or `null` | The `owner/repo` the inbox was limited to; `null` with `--all-repos`. |
| `total` | integer | `entries.length`. |
| `counts` | object | Entries per `category`, with all three keys always present. |
| `entries` | array | Sorted by `category` in the order below, then by `updated_at`, newest first. |

## Entry fields

| Field | Type | Meaning |
|---|---|---|
| `repo` | string | `owner/repo`. |
| `number` | integer | PR number. `repo` + `number` identify an entry; no PR appears twice. |
| `title` | string | PR title. |
| `author` | string | Author's login; empty for a deleted account. |
| `url` | string | PR web URL. |
| `updated_at` | string | ISO-8601 UTC, as GitHub reports it. |
| `category` | string | The group the PR is shown under: the first of its reasons, in the order `review_requested`, `new_commits`, `replies`. |
| `review_requested` | boolean | Your review is currently requested, including a re-request. |
| `head_changed_since_review` | boolean | The PR's head commit differs from the commit of your latest submitted review. `false` if you have never reviewed it. |
| `new_commits_since_review` | integer or `null` | Commits added on top of the reviewed commit. `null` when there is no count: either the head did not change, or the reviewed commit is no longer in the PR's last 100 commits (a force-push or rebase). |
| `unanswered_replies` | integer | Replies from others newer than your last comment, summed over unresolved review threads you started. |
| `reason` | string | Every matching reason as one human-readable line, as the picker shows it. |

Every entry has at least one reason: `review_requested`,
`head_changed_since_review`, or `unanswered_replies > 0`.

## Limits

Each of the two GitHub searches behind the inbox (`review-requested:@me`
and `reviewed-by:@me -author:@me`, both `is:open`) returns at most 30 PRs.
Per PR, the last 100 commits and the first 50 review threads of 50 comments
each are examined.

## Failure

On any `gh` or GitHub error, vdiff prints the error to stderr, prints
nothing to stdout, and exits 1.
