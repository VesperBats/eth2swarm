# Multi-PR plans

> Adopted from the Myotis repository (`biafra23/myotis`, `.claude/agent/multi-pr-plans.md`
> at commit `c99c1df1`) on 2026-11-16 at the owner's request, with what is
> specific to that repository (its builds, releases and artifact workflows)
> removed, and the sentences about this repository's own checks written for
> it. The owner rulings cited with a date were made there and carry over;
> `myotis#<n>` is an issue or PR of that repository.
> `CLAUDE.md` (the always-loaded core) keeps a summary and points here.
> "The workflow above" means `workflow.md`; the review rules are in `pull-requests.md`.

### Multi-PR plans land through a feature branch, never piece by piece on `main`

**`main` must be releasable at every commit** (owner ruling, 2026-09-30): a `v*`
tag cut from its head at any moment must not ship half a feature. So a work item
**planned as more than one PR (or more than one branch)** does not merge into
`main` part by part:

1. **Create the feature branch first**, from the current `main`, before the first
   part is coded: `feature/<topic>` (e.g. `git fetch origin main &&
   git push origin origin/main:refs/heads/feature/<topic>`). This rule is the
   permission to create it and to push to it as described here.
2. **Every part PR targets the feature branch, not `main`**, and goes through the
   full workflow above (code → review → PR → wait → address) like any other PR.
   Each part branches from the feature branch's current head — also where a
   generic instruction says to restart from the default branch. Open each part's
   description with "Part k/n of `feature/<topic>`" and list the parts still to
   come, so a later session can pick the plan up from GitHub alone.
3. **Ask the owner, once per plan and before the first part is coded, whether
   you may merge the part PRs into the feature branch yourself** — i.e. work
   through every part without interaction until the last one is merged into
   `feature/<topic>` (owner ruling, 2026-09-30). Ask it together with the plan
   itself. The answer covers that one plan: never infer it from an earlier
   plan's answer, a standing instruction, or a comment on GitHub. It lives in
   the conversation where the owner gave it, so a later session resuming the
   plan without that answer in its own conversation treats it as "no answer
   yet" and may ask again — "once per plan" bounds repeat asking within a
   conversation, not a resumed plan's first question.
   - **Yes** → merge each part into `feature/<topic>` yourself (a merge commit,
     not a squash or rebase; not by enabling auto-merge) once it is done under the
     workflow above: CI green on its current head, no merge conflict, the
     internal review addressed, the automated review POSTED on the PR and its
     findings handled, every review comment answered on its thread, and no open
     changes-requested review. A green `claude-review` job alone is not a
     review: the action skips and still goes green when the copy of
     `claude-review.yml` that runs — the PR's base branch's — differs from
     `main`'s, so a feature branch lagging `main` on that file is enough (see
     the file's header). Nor is a review of a head
     that has since moved beyond fixes for its findings and the other review
     comments: it runs once per PR (not on push), and a re-run replays the
     original commit, so it describes code that is no longer there. A part
     without a posted review, or whose head moved like that after it, is not
     yours to merge: report it "ready to merge" with that noted and let the
     owner decide. Otherwise branch the next part from the new feature head and
     carry on.
   - **No, or no answer yet** → the owner merges each part. Report a green,
     answered part as "ready to merge into `feature/<topic>`"; the next part
     that depends on it waits for that merge.
   - Either way, **a PR into `main` is never yours to merge** — the final PR of
     step 5 included.
4. **Keep the feature branch current with `main`** by merging `main` into it,
   never by rebase or force-push (open part PRs are based on it), whenever
   `main` moves in a way the feature has to absorb. A merge that went through
   **cleanly** may be pushed straight to the feature branch: it adds nothing
   `main` doesn't already carry. "Cleanly" means more than "no conflict
   markers": a textually clean merge of two changes to the same file can still
   fail to compile (myotis#587: `main` gave a constructor a fourth argument, the
   feature's test helper still passed three), so build the merged tree and run
   the affected tests locally BEFORE pushing it. This is the only direct push
   THIS STEP allows, and the restriction is on how `main` reaches the feature
   branch — a merge, never a rebase or force-push; a resolution reviewed — not
   on the feature branch in general: the final PR's review fixes are pushed
   straight to it (step 5).
   A merge that needed **conflict resolution** — or that compiles or passes
   its affected tests only with a change, which is the same thing — goes
   through a PR into the feature
   branch like a part, because the resolution is new code. When a part
   PR goes red in code it did not touch, check the last `main` merge before
   blaming the part. A fix the feature needs in code `main` also carries lands
   on `main` as its own single PR and reaches the feature through this merge,
   not as a feature-only patch.
5. **Only when every planned part is merged into the feature branch**, open the
   final PR `feature/<topic>` → `main`. It is an ordinary PR under the workflow
   above — internal review of the whole combined diff, CI, reviewer comments,
   "ready to merge" — and the merge into `main` is the owner's. Its description
   links every part PR. **Its review comments are fixed on `feature/<topic>`
   itself** (step 4's exception for a bug in code `main` also carries aside):
   commit on the feature branch, push, answer each thread with the
   commit — exactly as on any PR's own branch — and NEVER through another part
   PR (owner ruling, 2026-10-09: myotis#590 did that for myotis#588's review and only added
   a merge-ordering trap, the final PR being mergeable without its own fixes).
   The final PR's review and CI cover what lands on the branch; the clean-merge
   sentence in step 4 does not forbid this push. Its review is the real gate,
   not a formality over parts already reviewed: `claude-review` reads its
   prompt and `CLAUDE.md` from the PR's base, so part PRs were reviewed under
   the feature branch's copies, and this is the one PR held to `main`'s (see
   the header of `claude-review.yml`).

A **single-PR** fix or plan still targets `main` directly; none of the above
applies. If a single-PR task grows a second PR before its first has merged,
switch: create the feature branch from `main` and retarget the open PR's base to
it. If a first part has already landed on `main` and left it unreleasable, tell
the owner rather than stacking more parts on `main`. Skipping the feature branch
for a multi-PR plan is the owner's call for that task, never the author's.
Releases are unaffected: whatever is still on a feature branch is simply not in
a release cut from `main`. This rests on the `pull_request_target` trigger of
`claude-review.yml` being unscoped (any base branch), which is what gives part
PRs the Claude review; do not narrow it to `main`.
