## Summary

<!--
What user-visible problem does this solve? Keep the scope to one reviewable outcome.
Use "Part of #N" for a partial PR. Use "Closes #N" only for the terminal PR that
satisfies every current acceptance criterion.
-->

Issue: #

## Approach

<!-- Explain the root cause, design, and important alternatives or trade-offs. -->

### Architectural fit

<!--
Does this change extend an existing KiCad capability or shared Konnect module?
If it introduces a workaround or duplicates behavior, explain why and when it
can be retired. Keep the answer short; documentation-only changes may say N/A.
See architectural coordination tracker #590. This is a design review signal,
not a requirement to migrate unrelated legacy behavior.
For scope expansion, identify the smallest fix and explicit exclusions under
GOVERNANCE.md#proportionate-scope-review; a short answer is sufficient.
-->

## Branch and dependencies

<!--
Base branch:
Depends on:
Series order, if any:
Unique commits/acceptance criteria owned by this PR:
Next PR to promote after this one, if any:

Follow docs/BRANCH_AND_PULL_REQUEST_WORKFLOW.md. Do not open a cumulative PR
against main that repeats unmerged prerequisite commits.
-->

## Compatibility and safety

<!--
List public API/config/schema changes and their migration path.
For file or IPC mutations, explain target validation, atomicity/rollback, and failure behavior.
Write "No public compatibility impact" when applicable.
-->

### Same-class sweep

<!-- For a demonstrated shared defect, follow GOVERNANCE.md#same-class-defect-review.
Name the invariant, bounded caller/sibling set, covered/exposed/intentionally
different paths and evidence. Link remaining gaps with a next actor. Otherwise
say N/A with a reason. For file writers, name each changed file and the KiCad
programs that can overwrite it. A focused PR need not complete a whole tracker.
-->

## Validation

### Changed tool behavior

<!-- Complete for affected tool behavior using docs/RELIABILITY_CONTRACT.md.
Keep answers short. Remove this table for documentation-only/mechanical changes;
mark individual inapplicable rows with a reason. Do not invent runtime evidence.
-->

| Behavior | Contract and evidence for this change |
|---|---|
| Accepted inputs and declared defaults | |
| Invalid/unsupported inputs and structured errors | |
| Target, data source and prerequisite state | |
| Observed changes and preserved unrelated objects | |
| Failure before/after mutation, including applied work | |
| Recovery from partial/uncertain results without repeating applied work | |

<!-- Paste the exact commands and results. Note environment-dependent checks not run and why. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked --lib --tests` (what CI runs)
- [ ] `cargo test --workspace --locked --doc`
- [ ] `cargo clippy --workspace --locked --all-targets -- -D warnings`
- [ ] Relevant viewer, plugin, packaging, and real-KiCad checks

## Review checklist

- [ ] The diff is focused and contains no generated output, personal data, or unrelated cleanup.
- [ ] The branch includes current `upstream/main`, has no merge conflicts, and CI passed on this exact head.
- [ ] For a fork PR, maintainer edits are enabled, or the author accepts
  responsibility for the final current-main refresh.
- [ ] The branch was based on latest `upstream/main`, not a release tag (unless this is an approved backport).
- [ ] The PR shows only its unique commits and diff; dependencies and series position are explicit.
- [ ] Every review conversation is resolved; any post-review push or base refresh
  has been reviewed on the new exact head. An unchanged unique diff may use a
  focused refresh review; changed behavior received substantive re-review.
- [ ] New names follow `docs/NAMING_CONVENTIONS.md`; public renames include compatibility handling.
- [ ] New behavior and failure paths have regression coverage.
- [ ] File mutations are atomic and preserve unrelated content.
- [ ] IPC mutations verify the requested board; atomic/partial behavior and safe recovery are explicit under the reliability contract.
- [ ] If tools were added/removed: counts and docs updated per CONTRIBUTING.md (registry `tool_count`, `tool-directory.md`, DEV.md stats, README count).

## Maintainer merge state

<!-- Maintainers complete this section. Auto-merge is an execution mechanism, not approval. -->

- [ ] The PR has exactly one current `status:*` workflow label.
- [ ] `status:ready-to-merge` applies to this exact head SHA.
- [ ] The completed review is recorded; only stale automatic CODEOWNERS requests
  were cleared under the review-request workflow (manual requests remain open).
- [ ] All required checks and review conversations satisfy the `main` ruleset.
- [ ] Merge authorization is established: standing authorization applies to a
  `@mixelpixx`/`@neusse` PR, or explicit authorization names this exact head.
- [ ] Auto-merge uses a merge commit, or an already-green PR will be merged with `gh pr merge N --merge`.
- [ ] Terminal issue closure and the next PR to promote are identified.
