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
current `main` in the head branch, no force-push or deletion, and merge commits
only. A behind PR therefore cannot merge until its branch is updated and the ten
checks rerun on the combined tree. Auto-merge is enabled for a maintainer to arm
after exact-head review, and merged topic branches are deleted automatically.

## Review requests and completion

CODEOWNERS automatically requests its listed owners, including `@mixelpixx` on
most PRs. The ruleset requires **zero approving reviews** and does not require a
code-owner review. A pending request is therefore not a merge blocker or proof
that no maintainer has reviewed the PR. The one `status:*` label names the next
actor; a review of an obsolete head cannot make the current head ready.

When a maintainer finishes a substantive review of someone else's exact PR
head, submit one GitHub **comment review** recording the head, findings or
no-findings conclusion, and next actor (for example,
`gh pr review N --comment --body "Reviewed head ..."`). Use ordinary PR comments
for follow-up discussion.
GitHub does not allow an author to review their own PR; in that case record the
same assessment in an ordinary PR comment. A comment review is a review record,
not an approval to merge or a replacement for required checks, resolved threads,
issue accounting, or the queue. Do not submit an approval merely to clear a
request, and do not confuse an existing `CHANGES_REQUESTED` review with a pending
request.

After the review, inspect outstanding requests. If one came automatically from
CODEOWNERS (`asCodeOwner: true` in GitHub's review-request data), a maintainer
may remove it **only** when the reviewed maintainer has taken responsibility
and the requested owner has no explicit decision pending. Keep manually
requested reviews and requests involving unresolved scope, release, licensing,
or other owner decisions. Verify that the intended request disappeared; if it
returns after a new push, review the new head before clearing it again. Never
remove a request merely to make an unreviewed PR look complete.

## Default: one independent change

Use an independent branch when a change can be reviewed and merged without
another unmerged pull request.

```text
git fetch upstream
git switch -c fix/example upstream/main
# edit, test, and commit
git push -u origin fix/example
```

Open the pull request against `main`. Before its first final review:

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

If `main` advances after that review, do not immediately repeat this sequence.
The queue refreshes only the next-to-land PR under
[Maintainer refresh of a clean behind branch](#maintainer-refresh-of-a-clean-behind-branch).

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

## Maintainer refresh of a clean behind branch

When the next-to-land PR is behind current `main`, a maintainer may refresh
the contributor branch instead of sending it back for mechanical integration. This
path applies only when all of the following are true:

- the PR is next in the documented merge order;
- the commit list and diff contain only the PR's unique work;
- the contributor permits maintainer edits; and
- no unresolved finding requires the author to change the implementation.

Record the current head SHA and current `upstream/main`, then use the rebase form:

```text
gh pr update-branch N --rebase
```

The ordinary update command creates a merge commit in the contributor branch;
Konnect uses `--rebase` for its focused branches. Refreshing rewrites the head
and invalidates earlier readiness. Afterward:

1. verify the new commit list and diff still contain only the same unique work;
2. compare the old and new unique commits or patches and investigate any
   substantive difference;
3. wait for all ten required checks on the new head; and
4. record an exact-head review before applying `status:ready-to-merge` or
   enabling auto-merge.

If GitHub reports `DIRTY`, resolve conflicts caused by intervening `main`
changes when the resolution is mechanical and preserves the PR's unique
behavior. Record the conflict resolution and include it in the semantic-diff
comparison. When the semantic diff is unchanged, this can be a focused refresh review that
references the completed substantive review and the current-main CI. It does not
require repeating every original test or review step. An ambiguous conflict,
changed unique diff, failed check, disabled maintainer edits or copied
prerequisite history is author work: set the appropriate status and return it.

## Merge execution loop

The next actor is represented by exactly one workflow label:

- `status:waiting-on-author`: the contributor must change or clarify the PR;
- `status:waiting-on-dependency`: another named change must land first;
- `status:needs-scope-review`: a maintainer must decide the implementation
  boundary under [proportionate scope review](../GOVERNANCE.md#proportionate-scope-review);
- `status:waiting-on-review`: the focused current head is ready for review; and
- `status:ready-to-merge`: review of this exact head is complete and only the
  repository gate or merge execution remains.

For the one next-to-land PR in an overlap set:

1. If the branch is behind or mechanically conflicted, apply the maintainer
   refresh above. Return ambiguous conflicts or cumulative history to the
   author. Do not refresh deeper queued PRs.
2. A maintainer verifies the head SHA, focused diff, dependency position,
   issue-closing references, evidence, and every resolved review conversation.
   When the implementation boundary is unresolved, use the scope-review rule
   above and record the smallest acceptable fix and deferred expansion. A scope
   decision is maintainer work, not an author defect; keep independent safe fixes
   moving without requiring completion of the broader tracker.
   For demonstrated shared defects, verify the bounded sibling-path accounting
   under [same-class defect review](../GOVERNANCE.md#same-class-defect-review).
   Record remaining gaps separately from this PR's acceptance; a no-findings
   review of one PR is not a claim that the whole defect class is eliminated.
3. If something remains, the maintainer applies the label for the actual next
   actor and leaves auto-merge off. A substantive review finding always means
   `status:waiting-on-author`, including on a PR authored by `@mixelpixx` or
   `@neusse`. Record one complete actionable review and stop processing that PR.
   Do not silently patch another maintainer's PR and merge it in the same review
   pass unless explicitly asked to take over that specific finding.
4. If the PR is ready but required checks are still running, the maintainer
   applies `status:ready-to-merge` and enables auto-merge with the merge-commit
   method. If all requirements are already satisfied, the maintainer may merge
   immediately with `gh pr merge N --merge` after the same verification. A PR
   authored by `@mixelpixx` or `@neusse` has standing merge authorization once
   this exact-head gate is satisfied with no unresolved findings; any other
   author's PR requires explicit maintainer authorization for that exact head.
5. A new commit, rewritten head, base change, failed or missing required check,
   or unresolved conversation returns the PR to review. Recheck the new exact
   head before arming auto-merge again.
6. After GitHub merges it, update local `main`, verify terminal issue closure,
   post the acceptance mapping, and only then promote the immediate successor.
   Continue the loop automatically while each immediate successor is ready and
   authored by `@mixelpixx` or `@neusse`. Stop before the first PR by another
   author, even when a maintainer-authored PR appears later; the queue order is
   not bypassed to consume standing authorization.
   Run the complete local gate only under the conditional rules in
   `GOVERNANCE.md`.

Auto-merge removes waiting time; it does not relax admission control, review,
CI, or the one-next-PR rule. Until Konnect moves to an organization with a
native merge queue, maintainers must not arm several overlapping PRs and hope
GitHub chooses a safe order.

## Responsibilities when `main` moves

The queue owner refreshes the next-to-land PR when it qualifies for the
maintainer path above, including mechanical conflict resolution. The PR author
owns ambiguous semantic conflicts, repairing failed checks and reconstructing
cumulative or misleading history. A maintainer may help with that work, but
branch reconstruction is not a standing service.

When a stale PR contains copied prerequisite commits, the preferred correction
is to reconstruct it from current `main` with only its unique commits. A
maintainer may return the PR to draft or request reconstruction instead of
reviewing a misleading cumulative diff.

Do not repeatedly rebase a deep series after every unrelated merge. Wait until
the immediate prerequisite lands, then refresh or rebuild the next PR once.
This keeps the queue moving while minimizing conflict work for contributors and
reviewers.

## Issue closure in a series

Use `Part of #N` for a partial PR. Use `Closes #N` on exactly one terminal PR
only when that merge satisfies every current acceptance criterion. For an
integration branch, child PRs use `Part of`; the terminal PR to `main` carries
the closing references and the acceptance evidence.

See [GOVERNANCE.md](../GOVERNANCE.md) for claiming, merge authority, required
checks, and the conditions that require post-merge local validation.
