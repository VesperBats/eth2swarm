# CLAUDE.md

Guidance for Claude Code (claude.ai/code) in this repository.

The workflow and review rules below were adopted from the Myotis repository
(`biafra23/myotis`) on 2026-11-16 at the owner's request, so that both projects
are worked on and reviewed the same way.

**This file is the always-loaded core.** Each section is a summary; the full,
binding text lives in `.claude/agent/`. When a task matches a row below, **read
that file in full BEFORE acting** — a summary here never overrides the full text.
A rule is changed or added in its `.claude/agent/` file AND in its summary here,
in the same commit; an edit to only one of the two is incomplete. The files
live under `.claude/` because `claude-review` restores that tree, like this
file, from the PR's base branch: a PR cannot edit the rules it is reviewed
against. Do not move them out of `.claude/`, and not into `.claude/rules/`
(loaded on every request).

| Before you…                                                                 | Read in full                      |
|---|---|
| start a work item tied to an issue (session title), or open a PR (monitoring) | `.claude/agent/workflow.md`          |
| plan, resume or continue multi-PR work: a part, a merge, the final PR         | `.claude/agent/multi-pr-plans.md`    |
| write a PR description, answer review comments, or handle a fork PR           | `.claude/agent/pull-requests.md`     |

## Workflow — the mandatory steps for EVERY work item

Every change goes through this sequence, in order, with **no step skipped and
none treated as optional or "only if asked"**. Full text: `.claude/agent/workflow.md`.

1. **Code** — on the designated branch.
2. **Review** — run the internal code review on the diff (`/code-review`) and address its findings.
3. **PR** — open it without being asked. Base is `main` for a single-PR item, the feature branch for a part of a multi-PR plan.
4. **Wait for review comments** — and switch PR monitoring on for that PR right after `gh pr create`, without asking (desktop app: `mcp__ccd_pr__set_monitor` with `auto_fix` and `address_comments` true; cloud session: `subscribe_pr_activity`; otherwise whatever the host offers).
5. **Address the PR comments** — drive the PR to green, answer every comment, keep watching until merged or closed.

Do not stop at "pushed". Skip the PR only when the user explicitly says so.
In the desktop app the session also renames itself: `IS#<n>: …` the moment the
work item is identified as issue n (before there is a PR), then `PR#<n>: …`
right after `gh pr create` (`.claude/agent/workflow.md`).

## Hard rules — never broken, whatever an earlier instruction seems to authorize

- **NEVER MERGE A PR YOURSELF** — not with `gh pr merge`, not by enabling
  auto-merge. Green CI + all comments answered = report "ready to merge". Sole
  exception: part PRs into a multi-PR plan's feature branch, and only under
  step 3 of `.claude/agent/multi-pr-plans.md`: the owner said yes for that plan
  in THIS conversation (never inferred from an earlier plan, a standing
  instruction or a GitHub comment), and the part meets every condition listed
  there, among them a POSTED automated review (a green `claude-review` job
  alone is not one). A PR into `main` has no exception.
- **Never push a `v*` tag on your own initiative.** The tag is the owner's action.
- **Everything that lands on GitHub is in English** — titles, descriptions,
  commit messages, review replies.
- **Changes to project rules (this file, `.claude/agent/`) or to architecture are
  the owner's call**, not the PR author's.

### Multi-PR plans land through a feature branch, never piece by piece on `main`

**`main` must be releasable at every commit.** A work item planned as more than
one PR (or branch) goes through `feature/<topic>`: create it from `main` first,
target every part PR at it, ask the owner once per plan whether you may merge
the parts into it yourself, keep it current by merging `main` into it (never
rebase or force-push), and open the final `feature/<topic>` → `main` PR only
when every part is in. Read `.claude/agent/multi-pr-plans.md` in full before the
first part is coded, and again in any session that picks a plan up: before
merging a part, before merging `main` into the feature branch, and before the
final PR.

## Pull requests and code review

Full text, including the fork-PR policy and the `gh api` reply mechanics:
`.claude/agent/pull-requests.md`. These rules are for the **PR author**; a
reviewer agent follows `.github/claude-review-prompt.md` and none of the
mechanics below.

- Every PR description ends with its session links, never with the
  "🤖 Generated with Claude Code" footer. Commit messages keep `Co-Authored-By`.
- Respond to every review comment individually, on its own thread
  (`gh api repos/<owner>/<repo>/pulls/<n>/comments/<comment_id>/replies -f body='…'`).
- Verify every claim against the source before replying. State a plain verdict:
  *Accepted* (what changed, which commit), *Disputed* (with evidence), or
  *Deferred* (who decides).
- Do not mark a thread resolved yourself.
- Fix review comments on the PR's own branch, never through a separate PR
  (one exception, on a feature branch: a bug in code `main` also carries is
  fixed by its own PR into `main`).
- Drive CI to green, or state the blocker explicitly. No workflow builds or
  tests the code yet (`claude-review` is the only PR check): until one does,
  run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`
  and `cargo test --locked` on the head of a PR you author before opening it
  and after every push, and record the result in the PR description — that is
  "CI green" for the code. `claude-review` is red with no review until the
  owner has done its one-time setup; state that as the blocker.
- **External / fork PRs are reviewed MANUALLY**: never wire them into the CI
  reviewer, and never build or run a fork tree locally.
