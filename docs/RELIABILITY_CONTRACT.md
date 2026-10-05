# Tool reliability contract

Use this contract when implementing or reviewing a tool, and when interpreting
tool results during a design workflow. It defines contribution requirements and
caller behavior; it does not claim every existing handler already satisfies them.
Repository governance remains authoritative for merge gates and evidence.

## For callers: establish what happened

1. Bind the requested document and identify the source of each relevant result:
   live editor, saved file, or a stated mixture. Await state-changing prerequisites
   before dependent checks; issuing save and DRC concurrently does not order them.
2. Inspect the complete response, including errors, warnings, per-item results and
   coverage. A successful transport or `isError: false` alone does not establish
   that every requested item was applied or every required check ran.
3. Verify the actual outcome using readback and the relevant independent evidence
   (for example netlist, ERC/DRC, or rendered inspection). Compare with intent;
   echoing the request is not evidence that the operation applied it.
4. Classify incomplete work before continuing. If a response lacks outcome or
   source information, inspect through available tools or report that uncertainty.
   Use the fields the installed version actually provides; the categories below
   are meanings, not a newly guaranteed response schema.
5. Recover according to observed state. Retry only unapplied items of a partial
   batch. After a timeout or uncertain write, reload and inspect before deciding
   what to repeat. A refusal that proves nothing changed permits correcting the
   request and retrying. An error after a write does not prove rollback.

| Outcome | Required interpretation |
|---|---|
| Complete | The requested operation and relevant verification completed. |
| Partial | Identify applied, failed and unattempted items or checks; completion remains incomplete. |
| Refused/failed before mutation | State the cause and evidence that nothing was applied. |
| Uncertain after possible mutation | Preserve the target and diagnostic; inspect before retrying. |
| Check unavailable | Report missing evidence; do not substitute zero findings or a pass. |

Separate execution from engineering findings: a completed DRC run can report
violations, and a valid search can return no matches. Both differ from a check
that could not run or a result that could not be decoded. Required unavailable
evidence makes a design verdict incomplete; it does not automatically block an
unrelated contribution.

## For callers: editor ownership

For a live PCB mutation, use a supported IPC tool against the exact requested
board. For a saved schematic mutation, save and close that schematic in Eeschema
first. For project-settings tools writing `.kicad_pro`, save and close the
project in KiCad first: the schematic editor can overwrite settings even when
no PCB editor is open. Server-side protection varies by installed version;
this prerequisite does not claim every writer already enforces it.

Preserve an editor-lock or `unsafe_file_fallback` refusal. Close the owning
editor normally, reconcile unsaved state, and retry only after ownership is
resolved. Remove a lock manually only after confirming it is stale and no
editor owns the file; never use deletion or server restart as an automatic
bypass. Read-only inspection and successful MCP connection do not authorize
a file write. Atomic publication does not prevent a later editor save.

## For contributors: describe the changed behavior

For a PR changing tool behavior, complete the behavioral table in the PR template.
Use short answers for the affected tools; mark a row not applicable with a reason.
Pure documentation or mechanical changes need no invented runtime test matrix.

- **Inputs:** defaults apply to omitted options or explicitly documented null.
  Reject malformed present values with a structured error naming the field.
  Validate ranges before narrowing conversions. Treat integral JSON numbers
  consistently across equivalent arguments; never silently truncate fractions.
- **State:** identify the exact target, prerequisites and live/saved data sources.
  Disclose fallback and mixed-source results. Reuse existing target, transaction,
  readback and error helpers; introduce a shared helper when it gives a repeated
  invariant one owner.
- **Mutation:** preflight what can be checked before writing. Preserve unrelated
  objects and content. State atomic or partial batch semantics explicitly;
  preserve compatibility or document the migration. Report any applied work
  when a later step fails.
- **Evidence:** derive completion fields from observed results. Unsupported or
  malformed evidence is not a valid empty result. Preserve useful error context
  and stable error kinds for caller recovery.
- **Recovery:** distinguish refusal from uncertain persistence. Give the caller
  enough identity and applied-item information to avoid repeating successful work.

Review newly introduced suppression at boundaries: ignored persistence results,
error-to-default conversions, discarded row errors, unchecked casts, and requested
values presented as proof. These are review signals, not blanket bans on
`let _ =`, `unwrap_or_default`, or `filter_map`. Optional lookups and cleanup
can legitimately continue; explain the fallback and expose material degradation.
Do not claim all-or-nothing behavior when the backend cannot provide it.

## Bounded migration inventory

The shared `outcome` envelope is adopted for `add_schematic_component`,
`annotate_schematic`, `batch_place_components`, and `run_design_review`.
The four names are checked against the
code-owned catalogue, not an assumption that every tool uses the envelope.
That pilot is intentionally bounded. The following adjacent paths retain their
existing response contracts until their focused issues are resolved. The
machine-checked source of this baseline is
[`reliability-legacy-inventory.json`](reliability-legacy-inventory.json); the
table below is its human-readable explanation.

| Area | Current gap | Tracking | Removal condition |
|---|---|---|---|
| DRC entry points | #408 introduces shared explicit save/refill ordering and source evidence. Default calls still read saved files; general source authority and the outcome-envelope migration remain incomplete. | [#119](https://github.com/mixelpixx/Konnect/issues/119), [#574](https://github.com/mixelpixx/Konnect/issues/574) | One owned DRC execution/result contract identifies source state and unavailable or partial evidence, including the remaining migration. |
| Explicit config loading | A failed explicit plugin config can fall through to defaults and report success. | [#545](https://github.com/mixelpixx/Konnect/issues/545) | Explicit-config refusal is structured, does not start with substituted defaults, and has regression coverage. |

The former live/file-reader entry was resolved by
[#542](https://github.com/mixelpixx/Konnect/issues/542).
`get_layer_list` selects live or saved enabled layers; stackup enrichment is
explicitly saved-file evidence. `get_netclasses` selects live or saved board nets,
while class definitions and assignment patterns remain disclosed `.kicad_pro`
facts. Neither result claims saved project facts are unsaved live settings.
The reviewed legacy ceiling is reduced with that completed source-selection debt;
this does not adopt either reader into the outcome-envelope pilot.

New migrations should reuse the shared outcome type, but each remains a focused
change with its own compatibility and failure evidence. This inventory is not a
claim that those paths already meet the contract.

## Proportionate evidence

Provide an ordinary success case and the failures material to the changed
behavior. Mutation coverage includes unchanged state on refusal and evidence
that a successful operation actually applied its intended changes. Test a
post-write failure when it is a credible path, including truthful recovery.

Contract tests exercise the served dispatch as well as direct internal paths
where applicable. Use KiCad-authored fixtures for design parsing and mutation.
Expected results must be independent of the implementation under test: call
readback and check relevant external semantics, not just the writer's calculation.

Follow GOVERNANCE.md's negative-control rule for new guards: temporarily
neutralize a guard and demonstrate that its regression test fails, then restore
it. Record the distinguishing evidence concisely; an exhaustive narrative or
every operating system on every PR is not required. Hosted CI and material
safety evidence remain mandatory. Name secondary environments not observed and
use the existing validation-debt process.

## Adoption and enforcement

These are review requirements for new and changed behavior. Existing defects
remain visible in issues; fixing one tool does not require migrating every other
handler first. Keep fixes focused and expose one review-ready step per overlapping
dependency chain under the existing branch workflow.

The initial implementation tracked by [#549](https://github.com/mixelpixx/Konnect/issues/549)
landed through the following PRs. These are implementation references, not an
open implementation queue or a claim of universal handler adoption:

| Merged PR | Implemented baseline |
|---|---|
| [#551](https://github.com/mixelpixx/Konnect/pull/551) | Compiled/cached Draft 2020-12 validation for domain and meta-tools, catalogue conformance, checked unit handling, and positive sheet dimensions; coordinated with #546/#547/#543 |
| [#552](https://github.com/mixelpixx/Konnect/pull/552) | Shared, lifecycle-owned `inproc://` IPC test fixture; migrates the five racy mocks from #544 and covers readiness, concurrency, repeated use, and cleanup after success or responder failure |
| [#553](https://github.com/mixelpixx/Konnect/pull/553) | Shared outcomes and initial handler/observer migration |
| [#554](https://github.com/mixelpixx/Konnect/pull/554) | Named CI gate for the adopted outcome catalogue, served-dispatch outcome preservation, and the initial three-entry legacy-debt ratchet (now two entries after #542) |

The dispatch compiles, caches, and enforces every advertised tool schema before
the handler runs. Handlers still own domain rules and checked direct-call paths.
The CI gate checks that the outcome pilot remains served through the catalogue,
that its envelope survives the JSON-RPC `tools/call` path, and that every legacy
entry names existing paths, rationale, tracking issues, and removal criteria.
The baseline ceiling is code-reviewed and must ratchet down when an entry is
removed; adding tolerated debt requires an explicit guard change and must never
be generated automatically.

Contributors may use any AI or none. Share the behavioral contract while retaining
independent toolchain instructions and reviews. This contract specifies outcomes,
not model selection.

## Maintainer references

- [Governance](https://github.com/mixelpixx/Konnect/blob/main/GOVERNANCE.md)
- [Contributing](https://github.com/mixelpixx/Konnect/blob/main/CONTRIBUTING.md)
- [Branch workflow](https://github.com/mixelpixx/Konnect/blob/main/docs/BRANCH_AND_PULL_REQUEST_WORKFLOW.md)

The canonical source is `docs/RELIABILITY_CONTRACT.md`. The installer embeds these
same bytes as the konnect skill's `references/reliability-contract.md`, so
installed guidance can read it offline. Repository links above refer to current
maintainer policy, which may be newer than an installed release.
