# Fixing a privately reported vulnerability

> Adopted from the Myotis repository (`biafra23/myotis`, `.claude/agent/security-fixes.md`
> at commit `c99c1df1`) on 2026-11-16 at the owner's request. The procedure,
> its two periods and its owner rulings carry over; rewritten for this
> repository are the intro (Myotis has a `SECURITY.md` and private reporting,
> this repository has neither yet) and the build, check and release steps
> (1, 3, 5 and 7 to 9), which named Myotis's Gradle build, APK,
> reproducibility and release tooling. `myotis#<n>` is an issue or PR of that
> repository.

> Read in full before you start work on a security issue that is not public
> yet: one the owner hands you in private (from a draft security advisory the
> owner opened, or a report received some other private way), or **one you
> come across yourself** in code
> `main` already ships, during other work or a review. For that last one,
> stop and tell the owner in the conversation before it appears anywhere on
> GitHub: in a commit, a PR or a review reply. The owner's request,
> 2026-11-15, on myotis#870.

This repository has no `SECURITY.md` and no private vulnerability reporting
yet, so a reporter has been promised nothing in writing; this procedure makes
the commitment itself: the discussion stays in the owner's private advisory,
and the advisory is published once the fix is released (step 9).
The standing Workflow points the other way: every work item gets a public PR,
and PRs are the project's public record. This procedure replaces steps 3 to 5
of the Workflow for an embargoed fix, and step 1's branch (a local one, never
pushed), and nothing else. Every other rule in
`CLAUDE.md` still applies, the hard rules first.

## When it applies

- A vulnerability that is not public yet, in this repository's code or in how
  it uses a dependency.
- The owner decides whether a report is embargoed. A report that turns out not
  to be a vulnerability, or one that is already public (a published CVE in a
  dependency, an issue someone opened), takes the normal Workflow. Ask when
  unsure; never assume "not embargoed".

## Two periods: before the merge, and until the advisory is published

**Until the owner has merged the fix, nothing leaves the machine** but the
patch and report sent to the owner (step 5).

- **No push to any branch of this repository.** The session's designated
  branch is public too: a pushed commit is a disclosure, whatever its message
  says. This overrides the harness's "commit and push when done".
- No PR, issue, discussion, comment or review reply about it, on GitHub or
  anywhere else.
- Nothing goes to an external service: no gist, no paste site, no issue
  tracker of a dependency. Reporting a dependency's bug upstream is the
  owner's call, through that project's private channel.

**From the merge until the advisory is published, only the wording is held
back.** The code is public from the merge on, so pushes and PRs are normal
again (steps 7 to 9 need them), but every one of them stays neutral:

- A branch name, commit message, code comment, PR, changelog entry or
  release note says what changed ("validate X before Y"). It never says that it
  was a vulnerability, how it is exploited, what it exposes or who reported
  it.
- A release made for the fix, and its notes, use that neutral wording too.

## Steps

1. **Check that the build runs here, before you code.** Run
   `cargo build --locked`, which resolves and fetches the dependencies
   (`cargo --version` alone fetches none, so a blocked registry would not
   show). If the session cannot build (no toolchain, a proxy that blocks the
   registry), say so to the owner at once:
   the fix is then built and tested where it can be (a local session, the
   owner's machine) before the patch goes into the private fork.
2. **Code** the fix on a local branch off the current `main`. Do not push it.
   Its commit messages, code comments and test names become permanent in
   `git log` at the merge: word them neutrally from the start (below).
3. **Run what CI would run, locally.** GitHub runs no CI and no status
   checks in an advisory's temporary private fork, and no integration (the
   CI reviewer among them) can reach it. Run:
   - What `ci.yml` runs (`.claude/agent/pull-requests.md`, "Drive CI to
     green"): `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D
     warnings` and `cargo test --locked`.
   - What `integration.yml` runs, for a fix inside its paths: start a local
     bee-factory and run `EPOCH_SIZE=16 EPOCHS=4 scripts/e2e-bee-factory.sh`
     with `ETH2SWARM_RPC` and the Bee URLs of your stack (the script's header;
     `http://localhost:8545` and the default Bee ports without the port patch).
   - A check that ran nothing (0 tests, a filtered or ignored test) is "not
     run" in the report, never "passed".
   - A test that fails without the fix and passes with it. That test is the
     reproduction, and it lands on public `main` with the merge, before the
     fixed release. Whether it ships with the fix or follows as a second patch
     once the advisory is published is the owner's disclosure decision: ask,
     and keep it as a separate commit either way.
   - A fix that adds or bumps a dependency changes `Cargo.lock`: name the
     crates and versions it moves in the report, for the owner to review like
     code.
4. **Review** the diff with `/code-review`, locally (never `--comment`), and
   address its findings.
5. **Hand it to the owner.** Write the patch to an ignored path, never the
   checkout's root, where a later `git add -A` would publish it:
   `git format-patch -o target/security-fix origin/main` (`/target` is
   ignored; a `cargo clean` deletes it, so hand the files over before one).
   Write a short report beside it. It opens with **"not built"** if step 1 found no build. Then:
   the commit of `main` the patch is based on (the one the checks ran on; if
   `main` has moved, they no longer cover what is merged), what the fix does,
   every check of step 3 as passed, failed, or not run with the reason, and
   what step 7 will have to do. In a cloud session the container is not the
   owner's machine: send both with the host's file tool (`SendUserFile`, or
   what the host offers), never through GitHub. The owner applies the patch
   in the advisory's temporary private fork, where the reporter and other
   collaborators the owner adds can review it.
6. **The owner merges** the fork's PR into `main` from the advisory. That
   merge is the owner's action, as every merge is. Never merge, and never
   enable auto-merge. GitHub titles that merge commit `Merge pull request
   from GHSA-…`, which no neutral wording reaches, and enforces none of
   `main`'s protection rules for it: from that moment `main` shows a marked
   security fix with no fixed release out. So the report of step 5 says what
   the release will need (step 8), so the owner can prepare it and merge when
   the release can follow directly. The timing is the owner's.
7. **After the merge, before the release.** The fix is public from the merge
   on. The owner, or a session the owner asks, checks every workflow run on
   the merge commit (`CI`, and `integration` where the fix is inside its
   paths; a push to `main` runs both, like a PR). A red result is fixed at
   once by a normal PR into
   `main`, in the neutral wording above. No `v*` tag goes on a commit whose
   checks are red.
8. **Release.** This repository has made no release yet, so what counts as
   the fixed release (a `v*` tag, or the merge itself) is the owner's call:
   ask, and say so in the report of step 5. A release for the fix uses the
   neutral wording above. The tag is the owner's action. The fix's PR lives in the advisory's fork, so a
   changelog entry or release note has no PR number to cite; it cites the
   merge commit instead.
9. **Publish.** Publishing the advisory, and requesting a CVE for it, are the
   owner's actions, after the fixed release is out. Then a normal PR adds the
   advisory's id (`GHSA-…`) to that release's changelog entry, if the
   repository keeps one, for the owner to merge. The release notes may link
   the advisory too; editing them is the owner's call.

## Why it is shaped this way

- The advisory's temporary private fork is the one private place GitHub
  offers for the fix, and nothing automated runs there. The local checks of
  step 3 stand in for CI until the merge, and the CI runs on the merge commit
  (step 7) keep the release gate: no fixed release is tagged on an unchecked
  `main`.
- `main` must be releasable at every commit, the advisory merge included.
  With no CI before that merge, the local checks of step 3 are what keep it
  so: they are not optional, and a check that could not run is named in the
  report so the owner can decide before merging.
