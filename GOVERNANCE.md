# How Konnect is run

One page, so nobody has to reconstruct this from issue threads.

## Maintainers

- **[@mixelpixx](https://github.com/mixelpixx)** — project owner. Decides
  scope, releases, licensing, and anything not settled below.
- **[@neusse](https://github.com/neusse)** — maintainer. Owns the areas listed
  in [`.github/CODEOWNERS`](.github/CODEOWNERS).

### Current repository access model

Konnect currently lives in a personal GitHub account. [GitHub gives a personal
repository one owner and write collaborators](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/repository-access-and-collaboration/permission-levels-for-a-personal-account-repository);
it does not offer the granular Triage, Maintain, and Admin roles available to
organization repositories.
Accordingly, `@neusse` can triage, label, assign, review, push, merge, and arm
auto-merge, but cannot change repository settings or bypass the `main` ruleset.
Only `@mixelpixx`, as owner, can administer those controls.

Moving Konnect to an organization is accepted in principle but is not part of
the current workflow. Until that happens, [GitHub's native merge queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)
is not available here. The ordered queue below, the `status:*` labels, and
auto-merge provide the deliberately smaller substitute.

Konnect is deliberately built and reviewed through **two different AI
toolchains** — Claude Code on one side, OpenAI Codex on the other. That is not
duplication to be tidied away. Every defect found so far in the agent-facing
guidance Konnect ships was found from *outside* the toolchain that ships it,
because a reviewer inside it cannot see its blind spots. Keep the two stacks
independent.

## Merging

- **A reviewed green PR may be merged by its author or a maintainer.** No
  approving-review count is required, but review is still a real decision:
  every conversation must be resolved and the exact current head must satisfy
  the issue, evidence, and queue requirements below.
- **PRs authored by `@mixelpixx` or `@neusse` carry standing merge
  authorization once they satisfy that readiness gate.** The queue maintainer
  does not wait for an additional per-PR or per-head approval. After each such
  merge, synchronize `main`, verify the result, recompute the immediate queue,
  and continue through consecutive ready PRs by either maintainer. Stop before
  the first next-to-land PR authored by anyone else and obtain explicit
  maintainer authorization for that exact head; do not skip it merely to reach
  a maintainer-authored PR later in the queue.
- **A substantive review finding is a stop condition for every author.** Record
  the complete finding, apply `status:waiting-on-author`, and leave auto-merge
  off. Standing authorization begins only after the author resolves every
  finding and the new exact head receives a no-findings review with all ten
  required checks green. A reviewing maintainer does not silently repair
  another maintainer's PR and merge it in the same review pass; taking over a
  specific finding requires an explicit request.
- **Merge commits only** (`gh pr merge N --merge`), so authorship survives.
- **The active `main: CI must pass` ruleset is the protection source of truth.**
  It requires pull requests, all ten CI checks, and resolved review threads;
  requires the head branch to include current `main`; blocks force-pushes and
  deletion; and permits only merge commits. Repository auto-merge and automatic
  deletion of merged topic branches are enabled.
- A write collaborator cannot bypass the ruleset. Any owner-only direct-push
  exception is reserved for the documented release recipe's bump/stamp commits;
  it is not an ordinary merge shortcut.
- **Do not repeat the full local gate after every merge.** The required checks
  on the reviewed exact head are the merge evidence when the merge commit
  contains only that head plus the reviewed base. Run the full local gate only
  when the merged result differs from that composition, required evidence was
  unavailable or incomplete, or a concrete post-merge symptom creates a
  specific reason to retest:

  ```
  cargo fmt --all -- --check
  cargo clippy --workspace --locked --all-targets -- -D warnings
  cargo test --workspace --locked --lib --tests
  cargo test --workspace --locked --doc
  ```

  When the conditional gate is required, capture each exit code directly.
  Piping into `tail` or `echo` swallows it, and that has put a red commit on
  `main` twice.
- **CODEOWNERS is a routing hint, not a veto.** It auto-requests the right
  reviewer; it does not block a merge.
- **A pending review request is not the review decision.** The ruleset requires
  no approving-review count or code-owner approval. A maintainer records a
  completed exact-head review as a GitHub comment review when reviewing someone
  else's PR; the one `status:*` label still names the next actor. For their own
  PR, where GitHub does not permit self-review, the maintainer records the
  assessment in an ordinary PR comment. The record establishes review, not
  authorization: maintainer-authored PRs use the standing authorization above,
  while every other author's PR needs explicit maintainer authorization for the
  exact ready head.
  After a completed review, a maintainer may clear a stale automatic CODEOWNERS
  request only when that owner has no specific decision pending. Preserve
  explicit review requests, requested changes, and owner decisions about scope,
  releases, or licensing. See the
  [review-request workflow](docs/BRANCH_AND_PULL_REQUEST_WORKFLOW.md#review-requests-and-completion).

### Review-to-merge execution

1. Review the PR's exact head, issue accounting, focused diff, compatibility
   impact, and available evidence. Resolve every review conversation.
2. If a substantive finding remains, record one complete actionable review,
   apply `status:waiting-on-author`, and stop processing that PR. Do not patch
   another maintainer's PR during the same review pass or arm auto-merge unless
   explicitly asked to take over that specific finding.
3. Refresh only the next-to-land PR when `main` moves. Record the old head and
   current base first. If it is merely behind, use
   `gh pr update-branch N --rebase`. If it is `DIRTY`, treat a mechanical
   conflict caused by intervening `main` changes as maintainer integration work
   when permissions allow: resolve it, preserve the PR's unique behavior, and
   verify the semantic diff before publishing the rewritten head. Return
   cumulative/copied history, unavailable edit permission, an ambiguous
   conflict, or changed behavior to the author.
4. Treat the refreshed SHA as a new exact head. Required CI must rerun. When the
   unique commits and diff are semantically unchanged, the maintainer may record
   a focused review of the refresh against the completed substantive review;
   changed behavior requires substantive re-review.
5. When the PR is genuinely merge-ready, replace its workflow-state label with
   `status:ready-to-merge`. If checks are still running, arm GitHub auto-merge
   with the **merge commit** method. If every requirement is already green,
   merge with `gh pr merge N --merge` after the same final verification. For a
   PR authored by `@mixelpixx` or `@neusse`, standing authorization applies;
   otherwise record explicit maintainer authorization for that exact head
   before executing the merge.
6. Any new commit, force-push, base change, required-check regression, or newly
   unresolved conversation invalidates the readiness decision. Return the PR to
   the appropriate state, review the new exact head, and arm it again only after
   the gate is restored. GitHub may automatically disable auto-merge after a
   fork contributor pushes; that is expected safety behavior.
7. After merge, synchronize local `main`, verify the merge commit, terminal
   issue closure, and acceptance evidence, then promote only the next PR in the
   documented dependency order. Continue automatically while that immediate
   successor is ready and authored by `@mixelpixx` or `@neusse`; stop at the
   first other author, blocker, or end of the queue. Run the complete local gate
   above only when one of its explicit conditions applies. GitHub deletes the
   merged topic branch automatically.

## Claiming work

The queue is only legible if claims are visible.

- **Assign the issue to yourself** when you take it, and add `claimed`. A claim
  written only in a comment is invisible to everyone not reading that thread.
- Design-first on the issue for anything non-trivial: agree the approach, then
  open one focused PR.
- Priority labels `P0`/`P1`/`P2` and `area:*` labels are how the queue is read
  at a glance. Keep them current.

## Proportionate scope review

Address the demonstrated problem in supported workflows with the smallest
coherent solution. Additional edge-case handling requires observed demand or a
credible material safety, data-loss, or security risk. Prefer a clear refusal,
warning, documented limitation, or explicit caller input over speculative
inference or broad compatibility machinery. A rare trigger can still justify a
small shared correctness fix; rarity alone is not a reason to reject it.

During issue triage and PR review, distinguish the demonstrated defect from
optional expansion. Keep acceptance criteria bounded to the agreed outcome;
new variations need new evidence or an explicit scope decision. Use the
risk-proportionate validation rule below for unavailable environments: missing
secondary-environment evidence alone does not justify rejection.

Apply `status:needs-scope-review` only when the implementation boundary requires
a maintainer decision. It replaces the current `status:*` label and means the
next actor is the maintainer, not the contributor. Leave one concise comment
stating the decision needed, supported behavior, explicit exclusions, smallest
acceptable fix, and decision owner. Chris retains product-scope decisions;
routine boundaries follow the existing ownership model.

Once the boundary is agreed, record it in the issue's acceptance criteria,
remove the label, and apply the actual next-action status. Preserve priority,
area, assignees, and existing claims unless evidence changes them. The label is
neither `wontfix` nor a permanent architectural classification, and starts no
closure timer. Unlike `architecture:kicad-replacement`, it can also identify an
overbroad solution within an existing capability.

This is a decision aid, not another gate on every small fix. An independently
safe, scoped correction may proceed while broader expansion is deferred; a
tracker's unfinished work is not automatically its dependency. Required CI,
material safety evidence, and substantive review findings remain binding.

## Same-class defect review

When a demonstrated defect involves a shared mechanism or a repeated tool
pattern, triage and review must include a bounded sibling-path sweep. Identify
the violated invariant, inspect the affected helper's callers and equivalent
paths, and record which are covered, exposed, or intentionally different, with
the reason. Prefer one shared owner for the invariant over per-tool checks.

The issue or PR records the sweep boundary, evidence, and any remaining gaps
with a linked issue and next actor. Distinguish completion of this PR's accepted
fix from coverage of the defect class. Add a regression that exercises the
shared boundary and representative callers; use the existing negative-control
rule for a new guard. If practical, check that callers cannot bypass the boundary.

A focused safe fix may land with explicitly tracked sibling gaps. A material
safety gap in the change itself still blocks it. This rule does not require
whole-tracker completion, speculative variations, or a whole-codebase audit on
every PR. Apply proportionate scope review when the sweep suggests expansion.

For direct KiCad file writers, identify every file changed and which KiCad
program can overwrite it; atomicity and revision checks alone do not establish
editor ownership. Use the existing shared guards and the
[file-ownership rule](docs/KICAD_INTEGRATION.md#file-ownership-before-mutation).

## Branches and the PR queue

The detailed contributor workflow is in
[docs/BRANCH_AND_PULL_REQUEST_WORKFLOW.md](docs/BRANCH_AND_PULL_REQUEST_WORKFLOW.md).
Maintainers apply these queue rules:

- Independent changes use independent branches from current `main`.
- Contribution branches start from the latest `main`, not a release tag, except
  for an explicitly requested release-line backport.
- A dependent series exposes one mergeable step at a time. Deeper work stays in
  the contributor's fork or remains draft against an agreed upstream base; it
  must not appear as several ready, cumulative PRs against an unchanged `main`.
- After a prerequisite merges, the author reconstructs the next PR from current
  `main` with only its unique commits. A previous green run on a cumulative head
  is obsolete.
- A PR with copied prerequisites or unresolved conflicts is not ready for final
  review. A merely behind branch can be refreshed by a maintainer only when it
  reaches the front of the queue and the update is clean. Misleading cumulative
  history still requires reconstruction rather than repeated maintainer repair.
- A short-lived `integration/<topic>` branch requires maintainer agreement on
  scope, ownership, synchronization, evidence, terminal issue closure, and an
  expiry. Child PRs receive focused review and CI before one final integration PR
  is merged to `main`. Unrelated work continues on `main`.

These rules protect review quality without requiring every related change to be
one large PR. The unit of review remains one focused outcome.

### Admission control

The constraint is not how much work exists, it is how much *unfinished* work
sits in the review queue at once. Unreviewed inventory is what goes stale.

- One overlapping review-ready PR per contributor.
- One next-to-merge PR per dependency chain.
- In a high-conflict subsystem, at most one *ready to merge* plus two
  *waiting on review* behind it.
- Work that overlaps an admitted PR waits for its base-forming predecessor.
- A large item is split into independently valid increments **before** it is
  claimed. "Large" is a signal to split the acceptance criteria, not
  permission to open a cumulative branch.

Develop ahead as much as you like. What does not work is several cumulative
PRs sitting in the active queue, each invalidated whenever `main` moves.

### Landing order

Within an overlap set the order is not arbitrary:

1. Prerequisites and invariant-defining PRs first.
2. Independent, non-overlapping work may pass a blocked stack.
3. Within an overlap set, choose one base-forming PR.
4. Reconstruct only the *immediate* successor once its prerequisite lands —
   not every descendant.
5. Shared generated files land before their consumers. Tool counts are no
   longer in this category: `cargo xtask fix-doc-counts` derives them, so a
   consumer regenerates rather than conflicts.
6. **Release, version and count changes land last** — never through the middle
   of an active queue. v0.10.0 ignored this and invalidated eleven open PRs in
   one push. This rule exists because of that, not in anticipation of it.
7. After each merge: update `main`, verify the merge and issue state, run the
   full local gate only when its documented conditions apply, promote and refresh
   only the next PR, and arm auto-merge only once the refreshed exact head has
   been reviewed and its required checks pass.

## Releases

- **Never hand-edit tool counts.** `cargo xtask fix-doc-counts` rewrites them
  all from `router/registry.rs`. Hand-editing is what made every tool-adding PR
  conflict with every other one.
- **Announce intent before bumping**, and **land count-changing PRs first.**
  Every PR that adds a tool touches the same handful of documented counts, so a
  release that moves those counts conflicts with the entire open queue at once.
  This is not hypothetical: v0.10.0 did it to eleven open PRs.
- Version choice: a new tool, a renamed tool, or a changed response shape is a
  **minor**. Fixes — even ones that narrow behaviour nobody could have relied
  on — are a **patch**.
- Release notes state behaviour changes **and** known limitations. A reader who
  sees "fixed" and stops checking has been failed by the notes.
- Before creating the tag, manually dispatch `.github/workflows/release.yml`
  against the exact candidate ref. That non-publishing entry point runs the
  release-profile target matrix, PCM packages, and reusable real-KiCad gate;
  only a pushed `v*` tag may execute the publication job.
- The pre-release gate is CI, the real-KiCad E2E workflow, the live IPC tests,
  and an end-to-end benchmark run against the candidate. The benchmark has
  twice found what CI could not; it is a step, not a nicety.

## Evidence

The house rule, and the reason most of this file exists:

- **A response field must be derived from the result, never echoed from the
  request.** Most defects in this project's history are that one mistake.
- **A check that could not run is `BLOCKED`, never a silent pass.** That status
  describes the evidence item; it does not automatically decide whether the
  whole pull request is blocked.
- **Fixtures come from real KiCad output.** A hand-authored fixture tends to
  share the assumption the code got wrong, so it agrees with the bug.
- **Neuter every new guard** and confirm the test catches it. A passing test
  proves nothing until you have watched it fail.

### Risk-proportionate validation

Required hosted CI, deterministic regression tests, and evidence for material
safety properties remain hard merge gates. Environment-dependent observations
are shared project work:

- A contributor supplies real-environment evidence from an affected environment
  they reasonably have access to. They are not expected to personally own every
  supported operating system, KiCad version, or hardware configuration.
- Hosted CI owns supported-platform regression coverage. Maintainers recruit the
  original reporter or community testers when their environment can resolve a
  remaining uncertainty more directly.
- An unavailable secondary-environment observation may become explicit
  validation debt when the change is focused and reversible, required CI is
  green, deterministic coverage is adequate, and the unobserved path is not a
  credible data-loss, security, or compatibility hazard. Name the untested
  environment in the issue completion record or release checklist.
- Missing evidence blocks the pull request when it is necessary to establish
  the change's core behavior or a material safety property and no adequate test
  or proxy exists. State that specific risk instead of requiring every platform
  by default.

Validation debt is permission to gather field evidence after a safe merge, not
permission to represent an unavailable check as passed or to bypass required CI.

## Wontfix review window

`wontfix` is a reviewed disposition, not an immediate silent closure. Applying
the label to an open issue starts a 30-day evidence window. Automation comments
with the exact UTC closing date so a reporter or contributor has one final,
visible opportunity to identify a supported interface, overlooked evidence or
changed requirement.

- Removing `wontfix` cancels the pending closure.
- Reapplying it starts a new 30-day window.
- If the issue remains open and labeled at the deadline, automation records the
  completed window and closes it as `not planned`.
- A maintainer may remove the label when new evidence changes the decision; the
  ordinary triage and architecture-review process then resumes.

The workflow enforces timing only. It never chooses the disposition.

## Licensing

Konnect is AGPL-3.0 with commercial licences available, so contributions must
be relicensable. Submitting a contribution accepts the CLA in
[CONTRIBUTING.md](CONTRIBUTING.md). If you cannot agree to it, open an issue
describing the change instead — a reimplementation from a description is fine.
