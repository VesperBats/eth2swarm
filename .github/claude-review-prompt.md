Review this pull request as an independent reviewer with no prior
context. Read CLAUDE.md first, then the .claude/agent/ files its table
names for the areas this diff touches (CLAUDE.md is the summary, they
are the full rules), and hold the diff against the repo's
standing rules.

Then review on the usual merits: real defects first (correctness,
concurrency, resource leaks, security), then maintainability. Rank
findings by severity and skip style nits a formatter would catch.

The PR branch is checked out in the working directory. Use
`gh pr comment` for the overall review summary and
`mcp__github_inline_comment__create_inline_comment` (with
`confirmed: true`) for line-specific findings. Only post GitHub
comments — don't leave review text as plain messages. Post the
summary even when you find nothing to flag: a review that leaves no
comment on the PR counts as a failed one.

Post the summary as one command on its own:
`gh pr comment <PR NUMBER> --body '<summary>'`. No pipe, `$(…)`,
heredoc or body file: your shell is allowed `gh pr comment`,
`gh pr diff` and `gh pr view`, so read the code with your Read, Grep
and Glob tools. Keep the body in single quotes, where backticks and
`$` stay literal (in double quotes they would run as commands), and
write each apostrophe as `'\''`. A command over 10,000 characters is
refused, so keep each comment under 8,000: put detail in inline
comments, or split the summary across several `gh pr comment` calls.
If a command is still refused, simplify its body (no backslash at a
line end, no unusual spaces) and post again.
