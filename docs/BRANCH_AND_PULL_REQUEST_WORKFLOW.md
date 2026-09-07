# Branch and Pull Request Workflow

Konnect accepts independent pull requests, short dependent series, and
maintainer-approved integration branches. Choose the smallest model that makes
every review show one coherent change.

The base branch is part of the review contract. A green check on a branch that
contains obsolete prerequisites does not prove that its unique change works on
current `main`.

**Contributions start from the latest `upstream/main`, not the latest release
tag.** Release tags are stable consumption points for users and packagers; they
do not contain work merged after the release. A PR based on a release can be
green in isolation while omitting fixes and contracts already present on
`main`.

## Current repository mechanics

Konnect is currently a personal-account repository. Write collaborators can
triage, review, merge, and enable auto-merge, but only the owner can administer
repository settings. Organization-only Maintain/Admin role separation and the
native GitHub merge queue are not available here yet.

The repository therefore uses one explicit ordered queue rather than pretending
that GitHub is sequencing PRs for us. One active `main: CI must pass` ruleset
requires pull requests, all ten hosted checks, resolved review conversations,
no force-push or deletion, and merge commits only. Auto-merge is enabled for a
maintainer to arm after exact-head review, and merged topic branches are deleted
automatically.

## Default: one independent change

Use an independent branch when a change can be reviewed and merged without
another unmerged pull request.

```text
git fetch upstream
git switch -c fix/example upstream/main
# edit, test, and commit
git push -u origin fix/example
```

Open the pull request against `main`. Before final review:

1. fetch `upstream`;
2. rebase the branch onto current `upstream/main`;
3. resolve conflicts in the branch rather than asking the merge commit to guess;
4. push rewritten history with `--force-with-lease`, never plain `--force`;
5. wait for the required checks to pass on the new head.

Do not use `vX.Y.Z` or another release tag as a development base unless a
maintainer explicitly requests a backport to that release line.

Do not stack independent changes merely because one contributor is developing
them at the same time. Separate branches let either change merge, wait, or be
abandoned without moving the other.

## Dependent changes: expose one mergeable step at a time

A dependent series is appropriate only when change B cannot build, test, or be
reviewed meaningfully until change A exists.

Record the complete order on the tracking issue and in every PR description:

```text
#123 -> #124 -> #125
```

Only the next PR in that sequence should be ready for review against `main`.
Keep later work on separate local or fork branches. Link those branches from the
tracking issue if visibility is useful, but do not open cumulative PRs against
`main` that repeat every prerequisite commit.

After the prerequisite merges, reconstruct the next branch so it contains only
its unique work on current `upstream/main`. For a simple one-commit step:

```text
git fetch upstream
git switch -c fix/next-clean upstream/main
git cherry-pick <unique-commit>
# resolve conflicts, test, and inspect the diff
git push --force-with-lease origin HEAD:fix/next
```

For several unique commits, use `git rebase --onto` or cherry-pick the precise
range. In either case, verify both views before requesting review:

```text
git log --oneline upstream/main..HEAD
git diff --stat upstream/main...HEAD
```

The log must contain only the commits this PR owns. The diff must not reintroduce
already merged prerequisites. Old CI results are superseded by any rewritten
head or changed base.

### When a stacked PR base is possible

A PR can target an immediate prerequisite branch only when that base branch
exists in the upstream repository. A branch in a contributor's fork cannot be
used as the base branch of a PR in the upstream repository.

Collaborators may use an upstream prerequisite branch when maintainers agree,
but the deeper PR stays draft until its parent is merged. Afterward, retarget it
to `main`, synchronize it with current `main`, and rerun CI. Do not leave a chain
of ready PRs whose displayed diffs all contain the same unmerged changes.

## Integration branch: an explicit exception

A short-lived integration branch is useful when a tightly coupled program needs
several contributors or individually reviewed steps, but cannot keep restacking
against `main`. It is not the default for a large change.

Before creating `integration/<topic>`, obtain maintainer agreement on:

- the tracking issue and intended user outcome;
- the ordered child PRs and their owners;
- the branch owner and expected deletion date;
- how often current `main` will be incorporated;
- the integration, compatibility, and rollback evidence required at the end;
- which terminal PR will close each issue.

Child PRs target `integration/<topic>` and must show only their unique changes.
They receive the same tests and focused review expected for a PR to `main`.
Unrelated work continues to target `main`; the integration branch must not hold
the ordinary queue hostage.

After all child PRs land, update the integration branch from current `main`,
resolve drift once, and run the complete gate. Open one terminal PR from the
integration branch to `main`. That final review verifies the combined behavior,
issue accounting, compatibility, release notes, and rollback plan; it is not a
substitute for reviewing the child changes.

Delete the integration branch after the terminal merge. If its scope changes or
it remains open past the agreed lifetime, return to the tracking issue and
reconfirm the plan.

## What is merge-ready

A PR is merge-ready only when all of the following are true:

- its base and dependencies match the documented plan;
- its commit list and diff contain only the change it owns;
- current `main` is incorporated and GitHub reports no conflicts;
- every required check passed on the exact current head;
- the issue acceptance criteria, compatibility impact, and validation evidence
  are current;
- partial and terminal issue-closing keywords are correct.

A PR is not merge-ready merely because an earlier cumulative head was green.

## Merge execution loop

The next actor is represented by exactly one workflow label:

- `status:waiting-on-author`: the contributor must change or clarify the PR;
- `status:waiting-on-dependency`: another named change must land first;
- `status:waiting-on-review`: the focused current head is ready for review; and
- `status:ready-to-merge`: review of this exact head is complete and only the
  repository gate or merge execution remains.

For the one next-to-land PR in an overlap set:

1. A maintainer verifies the head SHA, focused diff, dependency position,
   issue-closing references, evidence, and every resolved review conversation.
2. If something remains, the maintainer applies the label for the actual next
   actor and leaves auto-merge off.
3. If the PR is ready but required checks are still running, the maintainer
   applies `status:ready-to-merge` and enables auto-merge with the merge-commit
   method. If all requirements are already satisfied, the maintainer may merge
   immediately with `gh pr merge N --merge` after the same verification.
4. A new commit, rewritten head, base change, failed or missing required check,
   or unresolved conversation returns the PR to review. Recheck the new exact
   head before arming auto-merge again.
5. After GitHub merges it, update local `main`, run the complete gate from
   `GOVERNANCE.md`, verify terminal issue closure, post the acceptance mapping,
   and only then promote or reconstruct the immediate successor.

Auto-merge removes waiting time; it does not relax admission control, review,
CI, or the one-next-PR rule. Until Konnect moves to an organization with a
native merge queue, maintainers must not arm several overlapping PRs and hope
GitHub chooses a safe order.

## Responsibilities when `main` moves

The PR author owns synchronizing the branch and resolving its conflicts. A
maintainer may help, but branch reconstruction is not a standing maintainer
service.

When a stale PR contains copied prerequisite commits, the preferred correction
is to reconstruct it from current `main` with only its unique commits. A
maintainer may return the PR to draft or request reconstruction instead of
reviewing a misleading cumulative diff.

Do not repeatedly rebase a deep series after every unrelated merge. Wait until
the immediate prerequisite lands, then rebuild the next PR once. This keeps the
queue moving while minimizing conflict work for contributors and reviewers.

## Issue closure in a series

Use `Part of #N` for a partial PR. Use `Closes #N` on exactly one terminal PR
only when that merge satisfies every current acceptance criterion. For an
integration branch, child PRs use `Part of`; the terminal PR to `main` carries
the closing references and the acceptance evidence.

See [GOVERNANCE.md](../GOVERNANCE.md) for claiming, merge authority, required
checks, and post-merge validation.
