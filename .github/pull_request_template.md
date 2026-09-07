## Summary

<!--
What user-visible problem does this solve? Keep the scope to one reviewable outcome.
Use "Part of #N" for a partial PR. Use "Closes #N" only for the terminal PR that
satisfies every current acceptance criterion.
-->

Issue: #

## Approach

<!-- Explain the root cause, design, and important alternatives or trade-offs. -->

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

## Validation

<!-- Paste the exact commands and results. Note environment-dependent checks not run and why. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked --lib --tests` (what CI runs)
- [ ] `cargo test --workspace --locked --doc`
- [ ] `cargo clippy --workspace --locked --all-targets -- -D warnings`
- [ ] Relevant viewer, plugin, packaging, and real-KiCad checks

## Review checklist

- [ ] The diff is focused and contains no generated output, personal data, or unrelated cleanup.
- [ ] The branch includes current `upstream/main`, has no merge conflicts, and CI passed on this exact head.
- [ ] The branch was based on latest `upstream/main`, not a release tag (unless this is an approved backport).
- [ ] The PR shows only its unique commits and diff; dependencies and series position are explicit.
- [ ] Every review conversation is resolved; any post-review push or base change has been reviewed again on the new exact head.
- [ ] New names follow `docs/NAMING_CONVENTIONS.md`; public renames include compatibility handling.
- [ ] New behavior and failure paths have regression coverage.
- [ ] File mutations are atomic and preserve unrelated content.
- [ ] IPC mutations verify the requested board and do not leave partial batches.
- [ ] If tools were added/removed: counts and docs updated per CONTRIBUTING.md (registry `tool_count`, `tool-directory.md`, DEV.md stats, README count).

## Maintainer merge state

<!-- Maintainers complete this section. Auto-merge is an execution mechanism, not approval. -->

- [ ] The PR has exactly one current `status:*` workflow label.
- [ ] `status:ready-to-merge` applies to this exact head SHA.
- [ ] All required checks and review conversations satisfy the `main` ruleset.
- [ ] Auto-merge uses a merge commit, or an already-green PR will be merged with `gh pr merge N --merge`.
- [ ] Terminal issue closure and the next PR to promote are identified.
