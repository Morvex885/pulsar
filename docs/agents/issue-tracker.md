# Issue tracker: GitHub

Issues and specs live in RisinT96/pulsar. Use the `gh` CLI and explicitly
pass `--repo RisinT96/pulsar` to repository-scoped commands; this checkout
also has an upstream remote, and `gh` may otherwise select it.

## Conventions

- Create: `gh issue create --repo RisinT96/pulsar --title "..." --body-file <file>`
- Read: `gh issue view <number> --repo RisinT96/pulsar --json number,title,body,labels,comments`
- List: `gh issue list --repo RisinT96/pulsar --state open`, adding filters as needed.
- Sub-issues: use `gh issue create --parent <parent>` or
  `gh issue edit <parent> --add-sub-issue <child>` (gh 2.94+). Where
  unavailable, put `Part of #<parent>` at the top of the child body.
- Blocking: use GitHub's native issue dependencies with
  `gh issue create --blocked-by <n>,<n>` or
  `gh issue edit <child> --add-blocked-by <blocker>` (gh 2.94+).
  Where unavailable, put `Blocked by: #<n>, #<n>` at the top of the
  child body. A ticket is unblocked when every blocker is closed.
- Count open blockers:
  `gh api repos/RisinT96/pulsar/issues/<n> --jq .issue_dependencies_summary.blocked_by`.
- Comment: `gh issue comment <number> --repo RisinT96/pulsar --body "..."`
- Label: `gh issue edit <number> --repo RisinT96/pulsar --add-label "..."`
  or `--remove-label "..."`.
- Close: post the explanation as a comment, then close the issue.
  On an already-closed issue, `gh issue close --comment` drops the comment.

## Pull requests as a triage surface

**PRs as a request surface: no.** Set to `yes` if external PRs should be
treated as feature requests. Triage then uses the corresponding `gh pr`
commands; GitHub issues and PRs share a number space.

## Skill operations

- “Publish to the issue tracker” means create a GitHub issue on the fork.
- “Fetch the relevant ticket” means read the issue as described above.
- `/wayfinder` uses an issue labelled `wayfinder:map` as its map, linked
  child issues as tickets, `wayfinder:<type>` labels for ticket types,
  and native dependencies for blocking. Claim a ticket by assigning it
  to `@me`; resolve it with a comment, closure, and a context pointer
  appended to the map.
