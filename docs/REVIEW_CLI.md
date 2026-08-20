# Review Session CLI

`tuicr review` exposes persisted review sessions without opening the TUI. It is
intended for scripts and coding agents that need to inspect or update tuicr's
saved review state.

Interactive TUI sessions create a persisted session file as soon as a review
target becomes active, so agents can resolve the announced slug immediately.
If that auto-created file still has no comments and no reviewed files when the
TUI exits, tuicr removes it.

While a TUI is open, tuicr records the active session in
`active_sessions.json` beside the storage manifest with the process id, slug,
session path, and last-seen timestamp. `tuicr review list` also includes an
`active` boolean so agents can select the live session without guessing from
timestamps.

Session arguments accept any of:

- a local slug from `tuicr review list`
- a PR slug, e.g. `gh:slatedb/slatedb/pr/1745` (PR slugs resolve without `--repo`)
- an absolute or relative path to a session JSON file (anything ending in
  `.json` or that exists on disk is treated as a direct path)

## Commands

```bash
tuicr review list --repo .                            # checkout + its repo's PR sessions
tuicr review list --repo slatedb/slatedb              # all sessions for a forge repo
tuicr review list --all                               # every session across all repos
tuicr review comments --session agavra/tuicr@main/worktree
tuicr review comments --session gh:slatedb/slatedb/pr/1745
tuicr review reply --session agavra/tuicr@main/worktree --comment-id <ID> "Fixed."
tuicr review resolve --session agavra/tuicr@main/worktree --comment-id <ID>
```

All `tuicr review` commands emit JSON by default. Timestamps are RFC3339 strings
so callers can parse them without locale-specific handling.

## The `--repo` selector

`--repo` is a repo selector, not just a path. It accepts:

- a checkout path (default `.`) — matches that checkout's local sessions and,
  via its `origin` remote, any PR sessions for the same repo
- a forge coordinate: `owner/repo`, `host/owner/repo`, `forge:host/owner/repo`,
  or a repo / PR URL — matches local and PR sessions by `owner/repo`

This is how PR sessions become discoverable. PR review sessions are keyed by
forge coordinates rather than a local checkout, so naming the repo — either by
standing in its checkout or passing `--repo slatedb/slatedb` — surfaces them.
`list` emits a usable slug for each; pass a PR slug to `--session` to read or
annotate it (no `--repo` needed, since PR slugs are self-contained).

```bash
# from anywhere:
tuicr review list --repo slatedb/slatedb
#   -> [ ..., { "slug": "gh:slatedb/slatedb/pr/1745", "kind": "pr", ... } ]
tuicr review comments --session gh:slatedb/slatedb/pr/1745
```

Azure DevOps repos live under `organization/project/repository`, so their
`owner` is `org/project` and a bare `owner/repo` selector cannot name them. Use
a URL form, which is recognized by host:

```bash
tuicr review list --repo dev.azure.com/myorg/myproject/_git/myrepo
tuicr review list --repo git@ssh.dev.azure.com:v3/myorg/myproject/myrepo
#   -> [ ..., { "slug": "az:myorg/myproject/myrepo/pr/123", "kind": "pr", ... } ]
```

`--repo` for `add` / `comments` is only consulted when resolving a *local*
slug; PR slugs and JSON paths ignore it.

## Add Comments

Use flags for quick manual comments:

```bash
tuicr review add --session agavra/tuicr@main/worktree \
  --target-file src/main.rs \
  --line 42 \
  --side new \
  --type issue \
  "Handle the empty case here."
```

Target flags:

- omit `--target-file` for a review-level comment
- pass `--target-file <path>` for a file-level comment
- add `--line <n>` for a line comment
- add `--end-line <n>` for a range comment
- use `--side old|new` for inline comments

## Reply To Comments

`reply` answers an existing comment, forming a thread:

```bash
tuicr review reply --session agavra/tuicr@main/worktree \
  --comment-id 79c9b3e1-0a7a-4efe-9d43-f7085d7c1a82 \
  --username "Claude" \
  "Fixed in def4567 — the empty case now returns early."
```

The id comes from `tuicr review comments`, and an unambiguous prefix works the
way a short SHA does in git (`--comment-id 79c9b3e1`); an ambiguous one is an
error naming the candidates. A reply is stored beside the comment it answers, so it inherits that comment's file, line, and side and
needs no target flags of its own. Threads are one level deep: replying to a
reply attaches to the same root. Replies carry no comment type — the body is
stored as written.

A TUI with the session open picks the reply up on its next poll (see
`review_watch_interval_ms` in [CONFIG.md](CONFIG.md), default one second), so
it appears in the reviewer's diff without any action on their part.

`--input` takes the same JSON forms as `add`:

```json
{ "comment_id": "79c9b3e1-…", "content": "Fixed in def4567.", "username": "Claude" }
```

`in_reply_to` is accepted as an alias for `comment_id`, so an entry read from
`review comments` can be echoed back with its own text.

## Resolve Threads

`resolve` marks a thread settled; `--unresolve` reopens it:

```bash
tuicr review resolve --session agavra/tuicr@main/worktree --comment-id 79c9b3e1
tuicr review resolve --session agavra/tuicr@main/worktree --comment-id 79c9b3e1 --unresolve
```

Any comment in the thread names it — the root or a reply — since a thread is
settled as a unit. Every comment in it carries the resulting `resolved` flag,
and the command prints the thread's root. Replying to a resolved thread reopens
it: a new message means it was not settled after all.

`resolved` appears on every entry of `review comments`. A caller answering a
review should skip resolved threads.

## JSON Input

For machine input, pass a JSON payload with `--input`. The value can be literal
JSON, `@path/to/payload.json`, or `-` to read stdin.

```bash
tuicr review add --session agavra/tuicr@main/worktree --input - <<'JSON'
{
  "type": "issue",
  "content": "Handle the empty case here.",
  "file": "src/main.rs",
  "line": 42,
  "side": "new"
}
JSON
```

Flat JSON fields:

- `content`: required comment text
- `type` or `comment_type`: comment classification, defaults to `none` (untyped, no `[TYPE]` tag)
- `file`: file path; omit for a review-level comment
- `line`: line number for a line comment
- `start_line` and `end_line`: range bounds
- `side`: `old` or `new`, defaults to `new`

Nested targets are also accepted:

```json
{
  "comment_type": "suggestion",
  "content": "This range can be simplified.",
  "target": {
    "type": "line_range",
    "file": "src/main.rs",
    "start_line": 10,
    "end_line": 14,
    "side": "old"
  }
}
```

Target types:

- `review`
- `file`
- `line`
- `line_range` or `range`

## Output

`list` returns a JSON array:

```json
[
  {
    "slug": "agavra/tuicr@main/worktree",
    "kind": "local",
    "path": "/Users/alice/Library/Application Support/tuicr/reviews/sessions/9f6c1b3e09a54e2a.json",
    "updated_at": "2026-05-22T17:20:00Z",
    "comment_count": 1,
    "reviewed_count": 0,
    "file_count": 3,
    "anchor": "main",
    "active": true
  }
]
```

With `--all`, PR sessions appear alongside local ones with `"kind": "pr"` and a
PR slug:

```json
[
  {
    "slug": "gh:slatedb/slatedb/pr/1745",
    "kind": "pr",
    "path": "/Users/alice/Library/Application Support/tuicr/reviews/sessions/172e168db0d525e5.json",
    "updated_at": "2026-05-22T17:20:00Z",
    "comment_count": 0,
    "reviewed_count": 0,
    "file_count": 12,
    "anchor": "pr/1745",
    "active": false
  }
]
```

`comments` returns a JSON array:

```json
[
  {
    "id": "79c9b3e1-0a7a-4efe-9d43-f7085d7c1a82",
    "location": "src/main.rs:42",
    "path": "src/main.rs",
    "start_line": 42,
    "end_line": 42,
    "side": "new",
    "comment_type": "issue",
    "lifecycle_state": "local_draft",
    "created_at": "2026-05-22T17:20:00Z",
    "author": "user",
    "resolved": false,
    "content": "Handle the empty case here."
  },
  {
    "id": "2f0a5c77-1b19-4d0e-9f42-6c1ac2b3e8d1",
    "location": "src/main.rs:42",
    "path": "src/main.rs",
    "start_line": 42,
    "end_line": 42,
    "side": "new",
    "comment_type": "none",
    "lifecycle_state": "local_draft",
    "created_at": "2026-05-22T17:24:00Z",
    "author": "Claude",
    "in_reply_to": "79c9b3e1-0a7a-4efe-9d43-f7085d7c1a82",
    "resolved": false,
    "content": "Fixed in def4567."
  }
]
```

`author` distinguishes the user's comments from an agent's: agents pass
`--username`, humans get the config `username` or `user`. `in_reply_to` is
present only on replies and names the root comment of the thread. Together
they are how a caller finds the comments it has not answered yet.
