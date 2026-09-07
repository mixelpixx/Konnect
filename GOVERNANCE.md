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
- **Merge commits only** (`gh pr merge N --merge`), so authorship survives.
- **The active `main: CI must pass` ruleset is the protection source of truth.**
  It requires pull requests, all ten CI checks, and resolved review threads;
  blocks force-pushes and deletion; and permits only merge commits. Repository
  auto-merge and automatic deletion of merged topic branches are enabled.
- A write collaborator cannot bypass the ruleset. Any owner-only direct-push
  exception is reserved for the documented release recipe's bump/stamp commits;
  it is not an ordinary merge shortcut.
- **Run the full local gate after each merge** before landing the next one:

  ```
  cargo fmt --all -- --check
  cargo clippy --workspace --locked --all-targets -- -D warnings
  cargo test --workspace --locked --lib --tests
  cargo test --workspace --locked --doc
  ```

  Capture the exit code directly. Piping into `tail` or `echo` swallows it, and
  that has put a red commit on `main` twice.
- **CODEOWNERS is a routing hint, not a veto.** It auto-requests the right
  reviewer; it does not block a merge.

### Review-to-merge execution

1. Review the PR's exact head, issue accounting, focused diff, compatibility
   impact, and available evidence. Resolve every review conversation.
2. If work remains, apply the one `status:*` label naming the next actor. Do not
   arm auto-merge.
3. When the PR is genuinely merge-ready, replace its workflow-state label with
   `status:ready-to-merge`. If checks are still running, arm GitHub auto-merge
   with the **merge commit** method. If every requirement is already green,
   merge with `gh pr merge N --merge` after the same final verification.
4. Any new commit, force-push, base change, required-check regression, or newly
   unresolved conversation invalidates the readiness decision. Return the PR to
   the appropriate state, review the new exact head, and arm it again only after
   the gate is restored. GitHub may automatically disable auto-merge after a
   fork contributor pushes; that is expected safety behavior.
5. After merge, synchronize local `main`, run the complete local gate below,
   verify terminal issue closure and post the acceptance evidence, then promote
   only the next PR in the documented dependency order. GitHub deletes the
   merged topic branch automatically.

## Claiming work

The queue is only legible if claims are visible.

- **Assign the issue to yourself** when you take it, and add `claimed`. A claim
  written only in a comment is invisible to everyone not reading that thread.
- Design-first on the issue for anything non-trivial: agree the approach, then
  open one focused PR.
- Priority labels `P0`/`P1`/`P2` and `area:*` labels are how the queue is read
  at a glance. Keep them current.

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
- A PR with copied prerequisites, a stale base, or unresolved conflicts is not
  ready for final review. Maintainers may return it to draft and request a clean
  reconstruction rather than repeatedly resolving contributor branch history.
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
7. After each merge: update `main`, run the full gate, promote the next PR,
   and arm auto-merge only once the exact head has been reviewed.

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

## Licensing

Konnect is AGPL-3.0 with commercial licences available, so contributions must
be relicensable. Submitting a contribution accepts the CLA in
[CONTRIBUTING.md](CONTRIBUTING.md). If you cannot agree to it, open an issue
describing the change instead — a reimplementation from a description is fine.
