---
name: tuicr
description: Use tuicr's review CLI to read and add comments in active TUI review sessions, and launch tuicr in cmux, tmux, Zellij, or Herdr when a user needs an interactive review pane.
---

# tuicr Review Workflow

Use `tuicr review` as the default agent interface. The TUI is where the human
reviews code; the CLI is how the agent discovers active sessions, reads user
comments, and, only when appropriate, adds agent-authored comments.

## Core Rule

First decide which workflow the user is asking for:

1. **User-led review of agent-generated changes**
   - The user wants to inspect the patch and write comments in tuicr.
   - Your job is to open or find the session, then retrieve the user's comments
     with `tuicr review comments` when they say comments are ready. If you are
     explicitly waiting while the user reviews, poll the same command
     periodically and look for new comment IDs.
   - Once you have addressed a comment, reply to it in the session (see
     **Reply To User Comments**) so the answer sits with the code.
   - Do not add your own review comments, do not preemptively review your own
     patch, and do not impersonate the user's comments.

2. **Agent review of an AI-generated patch**
   - The user wants you to understand, critique, or summarize a patch.
   - You may inspect the patch and propose findings.
   - If you can confidently identify this workflow and the target session, add
     findings directly with `tuicr review add` and an explicit `--username`
     identifying the agent. Ask first when the workflow or session is ambiguous.

If the user's intent is ambiguous, ask which workflow they want.

## Attach To A Session

1. Determine the repository directory from the user's request, current working
   directory, or recent file operations. Ask if it is ambiguous.

2. List persisted sessions:

   ```bash
   tuicr review list --repo /path/to/repo   # checkout + its repo's PR sessions
   tuicr review list --repo owner/repo      # all sessions for a forge repo
   tuicr review list --all                  # every session across all repos
   ```

   `--repo` is a selector: a checkout path also surfaces PR sessions for that
   checkout's `origin` repo, and a forge coordinate like `owner/repo` matches
   local and PR sessions by owner/repo. Each row carries a `kind` (`local` or
   `pr`) and a usable `slug`. Use `--all` when you don't know the repo.

   `[]` with exit 0 also means "not a repo root" — a subdirectory returns it
   too. Pass the root, then `--all`, before concluding nothing is open.

3. Choose the session:
   - If the CLI clearly reports exactly one relevant active session with
     `"active": true`, attach to it.
   - If multiple sessions are active, or the correct session is not clear, ask
     the user which slug to use. One repo can hold a worktree and a
     commit-range session at once, and adding to the wrong one exits 0.
   - If the user provided a slug or session JSON path, use it directly.
   - For a PR review, pass the PR slug from the listing (e.g.
     `gh:owner/repo/pr/N`) to `--session`; it is self-contained and needs no
     `--repo`.
   - If there is no active session, start or wait for one as described below.
   - Until active-session discovery is formalized as a stable protocol, treat
     `"active": true` as a convenience signal. If slug resolution fails, ask the
     user for the slug or repo path used by the session.

The CLI works even if the agent is not running inside tmux, Zellij, or Herdr,
so do not require a multiplexer just to connect to an existing active session.

## Start A Session

When the user needs an interactive tuicr pane and no active session exists:

| Environment | Action |
|-------------|--------|
| `$CMUX_WORKSPACE_ID` is set | Run `tuicr-wrapper-cmux.sh /path/to/repo -- <scope>` |
| `$TMUX` is set | Run `tuicr-wrapper.sh /path/to/repo -- <scope>` |
| `$ZELLIJ` is set | Run `tuicr-wrapper-zellij.sh /path/to/repo -- <scope>` |
| `$HERDR_ENV` is `1` | Run `tuicr-wrapper-herdr.sh /path/to/repo -- <scope>` |
| None is set | Tell the user you are waiting for them to start `tuicr` in the repo, then attach with `tuicr review list` after they say it is ready |

`<scope>` is `-w` for uncommitted working-tree changes or `-r <revset>` for a
commit range — always pass one explicitly so the user is never left to pick
staged/unstaged/commit-range manually in the TUI.

If more than one multiplexer marker is set, prefer the innermost multiplexer if
that is clear; otherwise ask. cmux hosts a Ghostty terminal, so `$TERM_PROGRAM`
reads `ghostty` inside cmux — check `$CMUX_WORKSPACE_ID`, not the terminal name.

tuicr supports both git and Jujutsu (jj) repositories, and jj workspaces may
have no `.git` directory at all. Do not pre-check the directory with
`git rev-parse` or refuse to launch because git does not recognize it; always
run the wrapper and let it validate the repository.

Wrapper paths are relative to this skill directory:

```bash
<skill-directory>/tuicr-wrapper-cmux.sh /path/to/repo -- -w
<skill-directory>/tuicr-wrapper.sh /path/to/repo -- -w
<skill-directory>/tuicr-wrapper-zellij.sh /path/to/repo -- -w
<skill-directory>/tuicr-wrapper-herdr.sh /path/to/repo -- -w
```

The Herdr wrapper requires `jq` to read pane IDs and completion results from
Herdr's JSON responses.

Every wrapper accepts pass-through tuicr arguments after `--`, which is how
you scope the review instead of leaving the scope selector for the user to
fill in — for example `-- -w` for uncommitted working-tree changes or
`-- -r <revset>` for a commit range. Always pass one of these explicitly when
launching a review pane.

If your tool supports command timeouts, use a long timeout, such as 10 minutes,
because the tmux, Zellij, and Herdr wrappers wait for the TUI to exit. The cmux
wrapper is the exception: it returns as soon as the pane is running and prints
the new surface ref between `=== TUICR SURFACE ===` markers. Capture that ref —
it is how you close the pane later with `cmux close-surface --surface <ref>`.
Once the TUI creates its active session, use
`tuicr review list --repo /path/to/repo` to capture the slug. If your
environment cannot run another command while a blocking wrapper is waiting,
read the comments after the user exits tuicr.

## Reconstruct The Diff

To review a patch yourself, rebuild the diff the user sees. The slug's source
segment says which:

| Slug segment | Diff |
|--------------|------|
| `worktree/<head>`, `staged-and-unstaged/<head>` | `git diff HEAD` |
| `staged/<head>` | `git diff --cached` |
| `unstaged/<head>` | `git diff` |
| `commits/<base>..<head>` | `git diff <base>~1..<head>` |
| `pr/<n>` | `gh pr diff <n>` |
| `pristine` | none; every tracked file shown in full |

Range endpoints are inclusive and printed oldest-first, so `<base>` without
`~1` drops the first commit. Check the file count against the listing row's
`file_count`; a mismatch means every line number you derive will be wrong.

## Read User Comments

This is the main review loop for user-led review.

There is no push stream from tuicr to the agent. Read comments by running the
CLI on demand. After the user says comments are ready, or after the TUI exits,
run:

```bash
tuicr review comments --repo /path/to/repo --session <slug>
```

The command emits JSON. Each comment includes fields like:

- `id`
- `location`
- `path`
- `start_line`
- `end_line`
- `side`
- `comment_type`
- `lifecycle_state`
- `author`
- `in_reply_to` (replies only — the id of the comment being answered)
- `resolved` (the whole thread is settled; skip it)
- `content`

Treat these comments as the user's review feedback:

- `issue`: blocking problem to fix first
- `suggestion`: consider implementing or explain why not
- `note`: answer or acknowledge
- `praise`: no action required

### Waiting for a round

Do not poll. Wait for the user to hand the review over:

```bash
tuicr review watch --repo /path/to/repo --session <slug>
```

It blocks until the user runs `:submit agent` in the TUI, then prints the
session's comments with an `outcome` of `handoff`. **Run it in the background**
so you stay free to work; you will be woken when it returns. Other outcomes:
`timeout` (nothing happened — start another watch if the review is still open),
`gone` (the session was discarded — stop waiting), and `changed` if you passed
`--any`.

Prefer the default over `--any`: a review is written in pieces, and waking on
every saved comment means answering half a thought. Tell the user the command
if they do not know it — a handoff they never make looks to them like an agent
that ignored their review.

If the user says comments are ready in chat instead, just read them with
`tuicr review comments`; the handoff is a convenience, not a requirement.

### Say you picked it up

The moment a watch returns `handoff` — before reading, before working — tell the
review you have it:

```bash
tuicr review working --repo /path/to/repo --session <slug> \
  --username "Claude Opus 5" --message "reading your six comments"
```

The user is watching a screen that otherwise cannot tell an agent thinking from
an agent that never heard them, and the first thing they do about silence is
submit again. The status bar shows the message verbatim while you work, so make
it what you are actually doing; run it again to change it during a long round.

It ends by itself when you hand back with `review update`. If you finish without
touching the code — an answer in the thread, a question, a refusal — end it
explicitly and say why:

```bash
tuicr review working --repo /path/to/repo --session <slug> --done \
  --message "answered in the thread, nothing to reload"
```

An indicator that just disappears reads as an agent that died.

An empty result does not by itself mean the review didn't happen. On exit,
tuicr always prints a line like `tuicr-summary: reviewed 3/3 files, 0 comments
added` to stderr (visible in the pane's scrollback), and `tuicr review list`
reports the same `reviewed_count`/`file_count` for the session. If
`reviewed_count` equals `file_count`, zero comments is a legitimate "nothing to
flag" outcome — treat the review as complete, don't ask the user to confirm.
Only ask whether the user saved comments in the intended session, or whether
another active session should be selected, when `reviewed_count` is less than
`file_count` (the user quit before reviewing everything) or you can't find a
`tuicr-summary:` line at all. If the review may have continued while you were
working, rerun `tuicr review comments` before claiming completion.

## Reply To User Comments

This is how you answer the review. After addressing a comment, reply to it in
the session — the user sees your answer under their comment, in the running
TUI, within about a second. Do not report back only in chat: the reply belongs
next to the code it is about.

```bash
tuicr review reply --repo /path/to/repo --session <slug> \
  --comment-id 79c9b3e1-0a7a-4efe-9d43-f7085d7c1a82 \
  --username "Claude" \
  "Fixed in def4567 — returns early on empty input."
```

Rules for the loop:

- Work in threads, not in single comments. A thread is a root comment plus
  every comment whose `in_reply_to` is that root's `id`; sort it by
  `created_at`. **A thread needs an answer when it is not `resolved` and its
  last entry is not yours.** `resolved` is the user's explicit "this is done" —
  honour it even if the last word is theirs, and never reopen a thread just to
  have the last word.
  Do not ask whether a particular comment has a reply pointing at it: replies
  always point at the thread's root, so a mid-thread comment can never be
  matched that way and you will keep re-reading it as unanswered.
- `author` is what tells the user's comments from your own — compare it against
  the `--username` you reply with.
- Never reply to your own comments, and reply at most once per comment per
  round. Re-read with `tuicr review comments` before replying again — the user
  may have answered in the meantime.
- Say what you did, not that you will: the commit or the change (`Fixed in
  <sha> — …`). If you are not doing it, say so and why; a disagreement belongs
  in the thread, not silently dropped.
- Use the same `--username` all session so your replies stay attributable.
- A reply inherits the comment's file, line, and side. Pass no target flags.
- Replying to a reply attaches to the same thread — you cannot nest deeper.
- The id must come from `tuicr review comments` for that session; an
  unambiguous prefix works, like a short SHA in git. An unknown id is an error,
  not a new comment.

`--input` takes the same JSON as `review add`, with `comment_id` (or
`in_reply_to`) alongside `content`, which is convenient for batching a round of
replies from a script.

### After you change the code

When you have rewritten, amended, or added commits under a review the user has
open, say so:

```bash
tuicr review update --repo /path/to/repo --session <slug> \
  --message "fixed both comments, amended into the original commits"
```

This also ends the "working" indicator, so `review working --done` is only for
rounds that change no code.

Their diff is stale until they reload, and nothing else tells them — a review
pinned to commits you amended away reloads into an identical diff, which reads
as a broken reload rather than a moved branch. Write a real sentence: it is
shown to them verbatim.

Launch reviews with an explicit revision when you can
(`tuicr-wrapper-zellij.sh /path/to/repo -- -r main..HEAD`). A review opened that
way re-resolves its range on `:reload`; one opened from the commit selector is
pinned to the commits it started with and cannot follow the branch.

### Resolving

```bash
tuicr review resolve --repo /path/to/repo --session <slug> --comment-id <id>
```

Resolve a thread when you have finished the work it asked for and nothing is
left to discuss — it folds away in the user's diff, drops out of their `m`/`M`
comment iteration, and drops out of your loop. Be
conservative: leave the thread open when you answered with a question, when you
declined the request, or when the user may still want to push back. Resolving
your own answer is not a way to close a disagreement. The user can reopen a
thread with `:unresolve`; their replies in the TUI reopen it automatically.
Your CLI replies do not reopen a thread the user has already resolved — if
your answer needs their eyes on a settled thread, say so outside the thread.

## Add Agent Comments

Only add comments when the workflow allows it and, for agent-authored review,
after the user approves writing them into tuicr.

Defaults:

- Prefer line comments when a specific file and line are known.
- Use file comments for file-scoped feedback.
- Use review-level comments only for whole-review summaries.
- Use `--type issue` for problems by default.
- Use `suggestion`, `note`, or `praise` when that better matches the intent.
- Pass `--username` so agent comments are visually distinguishable.

Examples:

```bash
tuicr review add --repo /path/to/repo --session <slug> \
  --target-file src/main.rs \
  --line 42 \
  --side new \
  --type issue \
  --username "Codex" \
  "Handle the empty case here."
```

```bash
tuicr review add --repo /path/to/repo --session <slug> \
  --target-file src/main.rs \
  --type suggestion \
  --username "Codex" \
  "Consider splitting this file-level concern into a helper."
```

Omit `--target-file` for a review-level comment. Add `--end-line` for a range
comment. Use `--side old` for removed lines and `--side new` for added or
unchanged lines in the new file.

For structured input, use `--input` with literal JSON, `@path/to/file.json`, or
`-` for stdin. Supported target types are `review`, `file`, `line`, and
`line_range`. One object per call — an array is a parse error. The file key is
`file`, not `path`. `target.type` is inferred from the fields present:

```bash
tuicr review add --session <slug> --username "Codex" --input \
  '{"file":"src/main.rs","line":42,"side":"new","comment_type":"issue","content":"Handle the empty case."}'
```

Then verify. A line outside the diff stores, prints back, and exits 0, but
never renders — invisible to the user, successful-looking to you. Re-read
`tuicr review comments` and check each `start_line` exists on the side you gave
(`new` for added or unchanged, `old` for removed). Keep the returned `id`s:
`review comments` reports no author, so they are the only way to tell your
comments from the user's.

## Legacy Export Output

Older wrapper-driven flows may emit:

```text
=== TUICR INSTRUCTIONS ===
...
=== END TUICR INSTRUCTIONS ===
```

If present, process those instructions. Otherwise prefer
`tuicr review comments`; it is the primary source of review feedback. If the
wrapper mentions clipboard export, ask the user to paste it only when the CLI
comments are unavailable.

## Multiplexer Tips

cmux:

- Switch panes: click the pane, or `cmux focus-pane --pane <ref>`
- Close tuicr: press `q`; the pane closes itself. Force it with `cmux close-surface --surface <ref>`
- List panes: `cmux list-panes`
- Read a pane without focusing it: `cmux read-screen --surface <ref>`

tmux:

- Switch panes: `Ctrl-b` then arrow keys
- Close tuicr: press `q`
- Resize panes: `Ctrl-b` then `Ctrl-arrow`
- Zoom pane: `Ctrl-b` then `z`

zellij:

- Switch panes: `Alt` + arrow keys
- Close tuicr: press `q`
- Resize panes: `Ctrl-n`, then arrow keys
- Toggle fullscreen: `Alt-f`
- Cycle stacked panes: `Alt` + `[` / `]`

Herdr:

- Select a pane: click it in the Herdr UI
- Close tuicr: press `q`; the wrapper then closes the review pane

## Error Handling

| Situation | Action |
|-----------|--------|
| Multiple plausible active sessions | Ask which session slug to use |
| No active session, cmux/tmux/Zellij/Herdr available | Start a new tuicr pane with the matching wrapper |
| No active session, no multiplexer | Tell the user you are waiting for them to start `tuicr` |
| cmux wrapper printed no surface ref | Run `cmux list-panes` to find the pane, or ask the user to start `tuicr` themselves |
| `tuicr` not installed | Tell the user to install tuicr |
| Not a repository | Ask for the correct repo directory |
| Comments are empty, but `reviewed_count` == `file_count` | Treat as a completed review with nothing to flag — don't ask |
| Comments are empty and `reviewed_count` < `file_count` | Confirm the selected session or ask the user to save/add comments |
| `review reply` says the id is unknown | Re-read `tuicr review comments`; the comment was edited away or belongs to another session |

## When Not To Use

- The user only wants raw `git diff` output.
- The user explicitly asks for a non-tuicr review workflow.
- The task is remote PR review and no tuicr PR session is involved.
