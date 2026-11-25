# CLAUDE.md

Guidance for Claude Code (claude.ai/code) in this repository.

The workflow and review rules below were adopted from the Myotis repository
(`biafra23/myotis`, commit `c99c1df1`) on 2026-11-16 at the owner's request, so
that both projects are worked on and reviewed the same way.

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
| work on, or come across, a security issue that is not public yet             | `.claude/agent/security-fixes.md`    |
| upload to a Bee node, or buy, top up or dilute a postage batch (bee-factory included: the file says when it is exempt) | `.claude/agent/postage.md` |

## Workflow — the mandatory steps for EVERY work item

Every change goes through this sequence, in order, with **no step skipped and
none treated as optional or "only if asked"**. Full text: `.claude/agent/workflow.md`.

1. **Code** — on the designated branch (an embargoed security fix: a local branch, never pushed; see below).
2. **Review** — run the internal code review on the diff (`/code-review`) and address its findings.
3. **PR** — open it without being asked. Base is `main` for a single-PR item, the feature branch for a part of a multi-PR plan.
4. **Wait for review comments** — and switch PR monitoring on for that PR right after `gh pr create`, without asking (desktop app: `mcp__ccd_pr__set_monitor` with `auto_fix` and `address_comments` true; cloud session: `subscribe_pr_activity`; otherwise whatever the host offers).
5. **Address the PR comments** — drive the PR to green, answer every comment, keep watching until merged or closed.

Do not stop at "pushed". Skip the PR only when the user explicitly says so,
or for an embargoed security fix (next section).
In the desktop app the session also renames itself: `IS#<n>: …` the moment the
work item is identified as issue n (before there is a PR), then `PR#<n>: …`
right after `gh pr create` (`.claude/agent/workflow.md`).

## Embargoed security fixes

Full text: `.claude/agent/security-fixes.md` (owner's request, 2026-11-15, on myotis#870).
A vulnerability that is not public yet (handed over in private, or found by
you in shipped code: then tell the owner in the conversation first, before it
appears anywhere on GitHub; this repository has no `SECURITY.md` and no private
vulnerability reporting yet)
does NOT take Workflow steps 3-5. **Until the owner has merged the fix, nothing
leaves the machine**: no push to any branch of this repo, the designated one
included, and no PR, issue or comment about it. From the merge until the
advisory is published, pushes and PRs are normal again but neutrally worded.
Fix it on a local branch, run CI's checks locally, and the end-to-end test for a
fix inside its paths (the advisory's private fork runs no CI), `/code-review`
locally, and send the owner
a `git format-patch` for the private fork. The owner merges, releases and
publishes the advisory; no tag goes on a `main` whose checks are red.

## Postage batches — ask before using one, never buy one

Full text: `.claude/agent/postage.md` (owner's request, 2026-11-20). It applies on
every Bee node except the local bee-factory. To tell, ask the node the command
talks to: `GET /wallet` reports `chainID` 1337 there. Any other value, or no
answer, is not bee-factory; a port, a URL or the `--rpc` chain settles nothing.
- **Ask the owner which batches to use**, before the first upload of an index in
  a work item, and again in any later conversation. Show the candidates with
  their depth, fill, lifetime and current use. Pass the answer as `--batch` and
  `--lower-batch`, never leaving it out: today's tools then fall back to a
  node's "first usable batch".
- **A new batch is proposed, never bought.** Propose its depth (`ethswarm stamps
  plan` once it exists, until then max(21, ⌈log₂(stamped chunks)⌉ + 2)), its
  lifetime and why, its cost and its label. Immutable, and serving one index
  only.
- **Never buy, top up or dilute a batch yourself**: give the owner the command,
  with the immutable flag named. Record every batch ID an index uses in the work
  item's PR or issue.
- **An answer covers what it names**: those batches for that index in that work
  item, in this conversation, never inferred from an earlier one.

Mutable batches are refused everywhere, bee-factory included.

## Hard rules — never broken, whatever an earlier instruction seems to authorize

- **NEVER MERGE A PR YOURSELF** — not with `gh pr merge`, not by enabling
  auto-merge, not by adding it to the merge queue. Green CI + all comments
  answered = report "ready to merge". Sole exception: part PRs into a multi-PR plan's feature branch, and only under
  step 3 of `.claude/agent/multi-pr-plans.md`: the owner said yes for that plan
  in THIS conversation (never inferred from an earlier plan, a standing
  instruction or a GitHub comment), and the part meets every condition listed
  there, among them a POSTED automated review (a green `claude-review` job
  alone is not one). A PR into `main` has no exception. The owner queues it,
  and the merge queue runs `CI` and the end-to-end test on the merged commit.
  If the queue ejects it for a failure of the PR's own, fix it on the PR's
  branch; an outside failure (runner, image pull, a red `main`) is reported
  and requeued by the owner (`.claude/agent/workflow.md`, step 5).
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
when every part is in. The merge queue is on `main` only: part PRs merge into
the feature branch directly, the final PR goes through the queue. Read
`.claude/agent/multi-pr-plans.md` in full before the first part is coded, and
again in any session that picks a plan up: before merging a part, before
merging `main` into the feature branch, and before the final PR.

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
- Drive CI to green, or state the blocker explicitly. The checks: `CI`
  (rustfmt, clippy `-D warnings`, tests, all `--locked`; zero tests run fails
  it), `integration` (the bee-factory end-to-end test, path-filtered on PRs:
  run the script by hand for a PR it skips that can still change that
  behaviour; the merge queue always runs it) and `claude-review`. Run the
  cargo checks locally before opening a PR too. Until the owner has added the `CLAUDE_CODE_OAUTH_TOKEN` secret, `claude-review`'s
  fallback job is red with no review: state that as the blocker. A PR whose
  base branch has no `claude-review.yml` gets no review run at all: say so,
  it is reviewed by hand.
- **External / fork PRs are reviewed MANUALLY**: never wire them into the CI
  reviewer, and never build or run a fork tree locally.
