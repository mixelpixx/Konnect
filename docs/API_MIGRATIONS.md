# Konnect MCP API migrations

Konnect's tool schemas are public API. This file records intentional argument
removals and the supported replacement workflow.

## Unreleased: an omitted reference keeps the library prefix (patch release)

`add_schematic_component` and `batch_place_components` wrote a bare `?` when
the caller passed no `reference`, dropping the library symbol's prefix.
`annotate_schematic` then numbered the symbol `1`, and a `#`-prefixed symbol
such as `power:PWR_FLAG` lost the `#` that keeps it off the board (#669).

The default is now the library `Reference` property plus `?` (`R?`, `#FLG?`),
as eeschema places a symbol, so annotation yields `R1` and `#FLG01`. It
appears in the written symbol, its `(instances …)` records and the response's
`fields.Reference`. A library symbol with no `Reference` property still gets
`?`. An explicit `reference` is unchanged, and so is `batch_place_components`'
failure entry, which still reports `"reference": "?"` for an entry that named
none.

## Unreleased: `run_erc` reports coordinates that can be found on the sheet (minor release)

KiCad's ERC **JSON** writer divided every violation coordinate by 100, from
the release that introduced it (8.0) through 10.0.6: it formatted schematic
internal units through `pcbIUScale` (1e6 IU/mm) where the text report used
`schIUScale` (1e4 IU/mm). `run_erc` reads that JSON, so a pin
`get_schematic_pin_locations` puts at `(100.33, 104.14)` came back from
`run_erc` at `(1.0033, 1.0414)` — a location that cannot be found on the sheet
(#541). Upstream fixed it in `6d8e1fe` for KiCad 10.0.7
([kicad#25582](https://gitlab.com/kicad/code/kicad/-/issues/25582)).

`run_erc` now checks the `kicad_version` the report states — the binary that
wrote those numbers — and says what it did in a new `coordinates` object:

- `status: "corrected"` — an affected version wrote the report; every `x`/`y`
  was multiplied by 100, and `scale_applied` says so. This is the change a
  caller sees: coordinates from KiCad 10.0.6 and earlier are now 100× larger,
  and match `get_schematic_pin_locations` for the same pin.
- `status: "verbatim"` — KiCad 10.0.7 or newer wrote it; coordinates are
  KiCad's own, untouched.
- `status: "withheld"` — the version could not be placed on either side of the
  fix (no `kicad_version`, an unrecognized one, or a development build, whose
  version cannot date it against the fix). No `x`/`y` is reported at all;
  KiCad's raw numbers ride along as `kicad_reported_x`/`kicad_reported_y` on
  the violation and on each item, deliberately under a name that is not a
  location. `coordinates.reason` says why.

`run_erc`'s optional `output` file changes with it: it used to hold the
filtered violation array alone, and now holds the whole response — the same
JSON the tool returns, with `violations` under that key beside `coordinates`.
That file is the copy handed to another tool or another person, so it must not
carry a coordinate without the block saying what the coordinate is worth. A
consumer of that file reads `<file>.violations` where it used to read the
top-level array.

One case the boundary cannot see: a KiCad built from the 10.0 branch between
the 10.0.6 release and the 10.0.7 one carries the fix but still stamps its
reports `10.0.6`, because `kicad_version` comes from `KICAD_SEMANTIC_VERSION`,
which upstream leaves at the last released version until the release commit
bumps it. Such a build reports correct coordinates and `run_erc` will multiply
them by 100. Nothing in the report distinguishes it from the affected release.
Released KiCad builds are unaffected, and only released builds are the
supported contract: an in-between branch build is outside it (maintainer
decision on #541). If you run a self-built 10.0-branch KiCad, check `coordinates.kicad_version` against your own build and treat a
`"corrected"` status from it as the one case where the correction is wrong.

`pcb drc` has its own report writer and never carried the defect, so
`run_drc`, `get_drc_violations` and every DRC coordinate are unchanged.

A caller that compensated for the scaling itself must stop: read
`coordinates.status` instead, and multiply only when something else says to. A
caller that reads `x`/`y` unconditionally must handle their absence when
`status` is `withheld` — where it previously received a number that could be
off by 100× with nothing saying so. KiCad scales the lengths quoted inside its
own violation *text* the same way ("Horizontal Wire, length 0.1270 mm" for a
12.70 mm wire); Konnect does not rewrite KiCad's prose, and
`coordinates.reason` states that.

## Unreleased: configuration tools refuse a file they cannot use (minor release)

`load_user_config`, `save_user_config`, `load_project_config`,
`save_project_config`, `get_effective_config`, `add_design_rule` and
`list_design_rules` treated a preferences file that failed to parse, or failed
to read for any reason, as if it did not exist. The load answered with
Konnect's defaults and said "User preferences loaded."; the next save then
wrote those defaults, plus the one key being set, over the user's file (#580).

A configuration file now has four states that are never merged:

- **absent** — the defaults, and the response says so: `source: "defaults"`
  (`"file"` otherwise). `load_user_config` still leaves the defaults on disk
  for the user to edit, and reports `persisted: true|false` with
  `persist_error` instead of discarding a failed write. `load_project_config`
  writes nothing, as before.
- **loaded** — unchanged.
- **malformed** (invalid JSON, or a root that is not an object) and
  **unreadable** (permissions, a directory at the path, invalid UTF-8) — a
  structured refusal, `error.kind: "invalid_configuration"`, carrying `path`
  and a `reason` starting `malformed_json:` or `unreadable:`. Nothing is
  written. Repair or move the file and retry.

The saves (`save_*_config`, `add_design_rule`) refuse in the same two states,
keep every other key, persist with an atomic conditional write against the
text that was read (an atomic no-clobber create when there was no file), and
answer from the file as read back. They add `path` and `created`. A
persistence error, including the conditional writer's `conflict`, is
`mutation_outcome_uncertain`: that writer reports a conflict both before a
replacement and after one whose readback differs, so the response does not
claim nothing was written — read the file before retrying. The one no-write
statement is a file that appears while Konnect is creating one, which is a
`conflict`. `get_effective_config` and `list_design_rules` add `sources:
{user, project}` (`"file"`, `"defaults"`, or `"not_configured"` when there is
no project directory) and refuse, naming which file, rather than merge
defaults in place of one that cannot be used.

No argument changed. A caller that relied on a broken preferences file being
silently reset must now repair or remove it.

## Unreleased: `add_power_symbol` snaps to the schematic grid (patch release)

`add_schematic_component` and `batch_place_components` snap the requested
position to KiCad's 1.27 mm schematic grid; `add_power_symbol` wrote it as
given (#662). Wires and labels are snapped too, so a power symbol left off the
grid could not be reached by them: ERC reported the endpoint off grid and the
power pin unconnected.

`add_power_symbol` now snaps like the other two placers. No argument or
response field changed. `x` and `y` in the response were already read back
from the committed file, so they now report the snapped position. A request
that was on the grid, which includes any pin endpoint of a placed component,
lands exactly where it did before; only an off-grid request moves, by at most
0.635 mm on each axis.

## Unreleased: `update_pcb_from_schematic` names every footprint it cannot place (minor release)

One library footprint the typed placement path cannot carry (custom-shape pads
today) turned the whole dry run into a `conflict` with a single diagnostic that
named neither the footprint nor a part: `reference: null`, no footprint id, and
preparation stopped at the first failure, so a second unusable footprint stayed
hidden until the first was replaced. Separately, a connected pad the footprint
does not have was checked only during apply, so a dry run could say `ready` for a
plan whose apply then failed with `footprint J601 has no pad 3`.

Both are now found while planning, and named. Status is still `conflict` and
nothing is planned or applied.

- Every diagnostic gains `references`, the list of every part it concerns, and
  `footprint_id`, the library footprint it is about or `null`. `reference` keeps
  its meaning: the one part concerned, or `null` when there are several or none.
  For the existing single-part diagnostics `references` holds that one part.
  This includes the single `preflight_conflict` diagnostic of a refusal before
  any plan exists (saved hierarchy, netlist export, IPC preflight), which used
  to carry only `code` and `message` and now carries `reference: null`,
  `references: []` and `footprint_id: null` as well.
- There is one diagnostic per footprint that cannot be prepared, listing every
  part that needs it. Its `code` says which stage failed, in the vocabulary
  `update_footprints_from_library` already uses:
  `footprint_library_resolution_failed` (the library id does not resolve),
  `footprint_library_read_failed` (the file cannot be read) and
  `unsupported_library_footprint` (it holds something Konnect cannot place).
  **Changed value:** the custom-pad case was reported as
  `footprint_library_resolution_failed`, although the library resolved; it is
  now `unsupported_library_footprint`.
- New code `footprint_pad_missing`: the schematic connects a pad the library
  footprint (for an addition) or the live footprint (for an update) does not
  have. It carries `reference` and `footprint_id`, and the message names the pads.

Callers do not change their requests. A caller that matched
`footprint_library_resolution_failed` to detect custom-shape pads should match
`unsupported_library_footprint` instead. Custom-shape pads are still not
supported; the fix is that the refusal now says which footprint to substitute
and for which parts.

## Unreleased: `get_layer_list` and `get_netclasses` answer about the live board (minor release)

Both tools previously parsed the saved `.kicad_pcb` unconditionally. A board
open in KiCad with unsaved changes was therefore answered from the file on
disk, and nothing in `get_layer_list`'s response said so.

Both now take an optional `board_source` selector:

- `auto` (the default, and what an existing caller gets) uses the exact board
  KiCad holds open, and reads the saved file only when the response can state
  why no live observation was used;
- `live` returns a structured error unless the exact board is open in a
  reachable KiCad;
- `saved` inspects the last saved file and reports that unsaved editor changes
  are excluded.

Under `auto` and `live`, a query that fails after KiCad has positively
identified the board — a rejection, or an editor this session had reached that
is no longer reachable — is returned as an error rather than answered from the
saved file.

Under `live`, a reachable KiCad that holds some other board returns the
established `wrong_document` error naming the boards it does hold, rather than
a generic unavailability.

A KiCad running with no PCB editor open — the project manager alone, which
refuses `GetOpenDocuments` itself — is not a live board and never identified
one, so `auto` answers from the saved file and reports
`no_pcb_editor_at_endpoint` (or its `…_with_editor_lock` /
`…_with_uninspectable_lock` forms). `live` refuses. A board this server
observed live before that editor closed is the exception and still refuses, as
`unsafe_file_fallback`. Mutating tools treat this state exactly as they did
before it had a name.

An endpoint whose build does not implement `GetOpenDocuments` at all falls back
the same way, under its own reason `open_documents_unimplemented_at_endpoint`
(with the same two lock forms). It is reported separately because it is not
evidence about KiCad's editors: nothing was identified, and a board may still
be open with unsaved changes behind a command that was never answered.

Both responses gain a `sources` object naming the origin of each domain
(`ipc`, `saved_board`, `project_file`, `derived`, `unavailable`) and a
`source_evidence` object carrying `board_state`,
`excludes_unsaved_editor_state`, a machine-readable `reason` and its prose
`detail`. `get_layer_list` additionally reports `copper_layer_count` and a
per-layer `display_name`; its `id`, `type` and `user_name` remain file-backed
and are `null` for a layer the live editor has enabled that the saved file does
not carry. `get_netclasses` keeps its existing `nets_source` string, whose
wording now also covers the live case.

`get_layer_list` also refuses, rather than answering, when KiCad returns an
`AS_OK` with no enabled-layer set: no board has zero enabled layers, and
reporting that non-answer as a live stackup of nothing would be a confident
lie about a board whose real stackup is in the file. A layer whose *name*
KiCad declines is different — the enabled set it did give still stands, and
that layer reports its canonical name.

The `display_name` lookups are one IPC round trip per layer, and together they
get ten seconds of KiCad's time. That is a bound on the waiting, not only on
when the last lookup may start: the lookups stop once it is spent, and each one
that starts waits at most what is left of it rather than the client's usual
30-second reply timeout. A live read of a KiCad that slows to a crawl therefore
returns inside the bound, with `display_name: null` on the layers it did not
get to and the canonical `name` on every layer as always.

`error.kind: "unsafe_file_fallback"` can now be returned by a read. It carries
the same `reason` as the write path — the fact is the same one — but nothing
was going to be written: reporting the saved file as current is the unsafe act
it refuses. `board_source: "saved"` inspects that snapshot deliberately.

Callers that want the previous behaviour exactly should pass
`board_source: "saved"`.

## Unreleased: `add_power_symbol` spells designators as eeschema does (patch release)

`add_power_symbol` numbered its hidden designators `#PWR001`, `#PWR002`, and
past ninety-nine `#PWR100`. eeschema writes the prefix, a `0`, then the number:
`#PWR01`, `#PWR010`, `#PWR0100`. `annotate_schematic` already follows eeschema,
so one sheet could carry both spellings of the same series.

`add_power_symbol` now uses the same rule. The `reference` it reports, and
writes to the `Reference` property and to each instance record, changes for
numbers 1 to 9 (`#PWR001` becomes `#PWR01`) and from 100 upward (`#PWR100`
becomes `#PWR0100`). For 10 to 99 the two spellings are the same text. No field
is added, removed or renamed.

Designators already in a file are not rewritten. The older spellings still
count toward the free-number search, so a sheet holding `#PWR001` and `#PWR002`
is handed `#PWR03`, never a duplicate. A caller that matched the literal text
`#PWR001` should read `reference` from the response instead.

## Unreleased: force-directed refinement refuses unsafe plans

`refine_placement_force_directed` is deprecated as a recommended bulk-cleanup
workflow. Its global-net spring model can pull one footprint toward every
other footprint sharing a board-wide rail, and a non-converged plan previously
applied without proving that it improved placement.

Dry-run responses now include `plan_status`, `blocking_reasons`, and
`displacement_mm` for each planned move. A plan is blocked when it does not
converge, does not improve the shared placement score, retains a hard-fail
verdict, places a courtyard outside the board outline, exceeds the optional
positive `max_displacement_mm`, or omits that limit. Dry-run still returns a
blocked plan for diagnosis. Apply mode returns structured `plan_blocked` before
the first mutation, and therefore requires `max_displacement_mm` even though
the argument remains optional for diagnostic dry-runs.

Use bounded explicit `move_component` / `rotate_component` operations derived
from schematic function, with `score_placement` and KiCad DRC after each small
batch, instead of treating the deprecated heuristic as an automatic placer.

## Unreleased: hierarchical sheets open without KiCad repair (patch release)

`add_hierarchical_sheet` and `duplicate_sheet` previously omitted canonical
KiCad 10 sheet defaults and the root schematic's page-one instance/footer
records. CLI ERC and PDF export still succeeded, but opening the generated root
in Eeschema displayed an automatic-repair warning and changed the file on save.

New and duplicated sheets now carry the defaults observed in a real KiCad
10.0.6 repaired-save, and a hierarchy root gains `sheet_instances` and
`embedded_fonts` only when those records are absent. Existing footer records
are preserved. Callers do not need to change their requests.

## Unreleased: `place_decoupling_caps` requires exact references

`place_decoupling_caps` no longer discovers capacitors by looking for any net
shared with the target IC. A board-wide net such as GND made that inference
select unrelated parts and could generate a row far outside the board.

Callers must now supply a non-empty `capacitor_references` array containing the
exact existing footprints to move. Those references are trusted as the
caller's design intent; Konnect does not apply a reference-prefix or value
heuristic to second-guess them. Missing, duplicate, unknown, or geometrically
unplaceable references return structured invalid input before planning.

Dry runs now report `plan_status` (`applicable` or `blocked`) and
`blocking_reasons`. A target outside the board outline, a non-improving score,
or a newly introduced hard failure blocks the plan. Apply mode evaluates the
same plan and returns a structured `plan_blocked` error with the same reasons
before writing. Existing net-inference callers must choose the exact references
from schematic evidence and pass them explicitly.

## Unreleased: component replacement preserves pin junction and no-connect intent

`replace_component` previously changed a placed symbol's library identity and
reported success without re-evaluating the junction dots at its old and new
pin endpoints. A replacement whose new pin landed on a wire could therefore
look connected while remaining absent from KiCad's netlist; a pin that left a
wire could strand an unjustified dot.

Replacement now reconciles the whole old/new pin-endpoint difference and
commits the symbol, carried no-connect markers, and junction changes in one
conditional atomic write. A no-connect marker follows a protected pin only
when the old pin maps to exactly one new pin with the same placed-symbol UUID,
unit, and pin number. Local geometry is intentionally allowed to change;
removed, renumbered, or duplicated protected pins return a structured refusal
before writing instead of guessing correspondence.

The response gains `junctions_added_count`, `junctions_pruned_count`,
`no_connects_moved_count`, and `no_connects_moved`. Existing component fields
are now bound to the selected UUIDs and read back from the committed file. An
unprovable committed result returns `mutation_outcome_uncertain` rather than
echoing the requested library identifier as proof.

## Unreleased: region moves preserve pin junction and no-connect intent

`move_region` previously translated every selected symbol unit and reported a
successful placement without re-evaluating the junction dots at any moved pin.
A pin moved onto the interior of a wire therefore looked connected in
eeschema but remained unconnected in KiCad's netlist; a pin moved away left an
unjustified dot behind. No-connect markers were also left at their old
coordinates.

Region movement now treats the selected units as one placement change. It
carries no-connect markers by stable pin identity, reconciles the whole-sheet
symmetric pin-endpoint difference once, and commits symbols, markers, and
junctions in one conditional atomic write. Whole-region reconciliation is
important when selected pins swap coordinates: processing the symbols one at
a time could briefly invent or remove a dot that remains justified by the
complete move.

The response gains `junctions_added_count`, `junctions_pruned_count`,
`no_connects_moved_count`, and `no_connects_moved`. Returned placements are
read back from the committed file. If a marker cannot be followed one-to-one,
the tool returns a structured `ambiguous_target` or `stale_target` error and
leaves the schematic byte-identical. If committed readback cannot prove the
selected UUIDs landed at their requested positions, the tool reports
`mutation_outcome_uncertain` rather than echoing its plan as success.

The tool still moves symbols only; it does not stretch or reroute connected
wires. Re-route the affected nets after a successful region move.

## Unreleased: batch placement preserves pin-on-wire connectivity

`batch_place_components` previously placed a pin directly on the interior of
an existing wire without adding the junction KiCad requires at that point. The
sheet looked connected, the tool reported a complete placement, but KiCad's
netlister left the pin unconnected. Single-component placement already handled
the same geometry correctly.

Batch placement now reconciles every new pin endpoint once after planning the
whole batch and commits the symbols and required junctions in one conditional
atomic write. Its response gains `junctions_added_count` and
`junctions_pruned_count`; existing fields keep their meanings. A batch that
does not change junction state reports zero for both fields.

## Unreleased: `set_active_layer` refuses instead of corrupting KiCad 10 boards

`set_active_layer` previously inserted an `(active_layer "...")` entry into the
board's `(setup ...)` block. KiCad 10.0.6 does not support that document token:
active layer is editor-session state, and a board containing the inserted entry
cannot be loaded.

The tool now returns a structured `unsupported_capability` error and leaves the
board byte-identical. There is no supported replacement call in the bundled
stable KiCad IPC protocol. If v0.12.0 already changed a board, close it, make a
backup, remove only the injected `(active_layer "...")` line, and reopen it.
See [Troubleshooting](TROUBLESHOOTING.md#a-board-no-longer-opens-after-set_active_layer).

## Unreleased: a stale-target refusal is bounded, not one line per symbol (patch release)

Every mutating schematic tool preflights placed-symbol instance metadata. When
that check refused, `error.reason` carried one `"{reference}: observed [...],
expected [...]"` line per stale symbol. A sheet goes stale as a whole — copy a
`.kicad_sch` to a new filename stem and every symbol records the old project
name — so the answer repeated one fact once per symbol: 18 535 B of `reason` and
a 37 301 B response for a 46-symbol sheet, growing linearly with the sheet (#592).

`reason` now names the expected instance identity once, states how many of the
sheet's placed symbols are stale, and groups the symbols by their diagnosis,
naming at most three symbols and five distinct diagnoses before counting the
rest. Instance lists themselves are never sampled — an elided path is one the
caller cannot write back:

```text
placed-symbol instance metadata disagrees with project 'demo': 46 of 46 placed
symbols are stale; every symbol must record exactly [demo:/<root>/<sheet>];
RV201, C201, R203 and 43 more (46 symbols): observed [old:/<root>/<sheet>]
```

The same sheet now answers in 1 373 B. Symbols that disagree with each other
keep separate entries, so a mixed sheet still names each distinct failure.

The guard itself is unchanged: the same documents are refused, `error.kind`
stays `stale_target`, `error.target` is unchanged, and nothing is written. Only
the `reason` text — and the `message` that interpolates it — is shorter. Callers
matching on `error.kind` need no change; a caller parsing individual symbols out
of `reason` should read the sampled identities and counts instead, or inspect
the file. `reason` remains prose for a human or a model, not a parsed field.
## Unreleased: a placement change carries the no-connect on the pin it moves (minor release)

A no-connect marker is a sheet item at a coordinate, so
`move_schematic_component`, `rotate_schematic_component` and
`bulk_move_schematic_components` used to leave it where it was while the pin it
protected moved away. The junction pass then saw
an unprotected pin, and where that pin had landed mid-span on a wire it wrote a
dot — putting the pin the caller had explicitly declared unconnected onto that
net (#626). ERC reported the stranded marker; nothing reported the new
connection.

All three now treat the marker as intent attached to a pin. The marker is
followed by symbol instance UUID, unit, pin number and library pin geometry —
never by coordinate coincidence alone — and moves in the same write as the
symbol, so the junction pass sees it at the arrival point and adds no dot.

All three responses gain `no_connects_moved_count` and `no_connects_moved`, an array
of `{ uuid, from: { x, y }, to: { x, y } }` read back from the written file.
A placement change that carries nothing reports `0` and `[]`. No existing field
changes meaning, and `junctions_added_count` on a move or bulk shift can now be
`0` where it was `1`, which is the fix.

`move_region` and `replace_component` now inherit this contract together with
their whole-mutation junction reconciliation as documented above (#623,
#625). `batch_place_components` reconciles its newly placed pins as documented
above (#622).

A placement change that cannot follow a marker one-to-one now **refuses before
writing** instead of orphaning it:

- `ambiguous_target` when the pins under one marker land on different points —
  two pins stacked on it with only one of them moving — or when carrying it
  would stack two markers on one point. `target` names the marker and its
  position; `candidates` name the competing pins or markers.
- `stale_target` when the sheet cannot answer for the marker at all: a
  `lib_symbols` lookup that failed, a placed symbol with no UUID, or a marker
  with no UUID to follow.

A refusal leaves the file byte-identical. The remedy depends on which one it is:

- For `ambiguous_target`, delete or replace the competing marker with
  `delete_no_connect` / `add_no_connect`, then repeat the placement change.
- For `stale_target` naming unresolved pin geometry, repair the sheet rather
  than the marker: the refusal is sheet-wide, so **one** placed symbol whose
  `lib_symbols` entry is missing blocks every placement change on a sheet that
  has any no-connect, including changes nowhere near a marker. An unresolvable
  symbol means Konnect cannot tell which pins sit under a marker at all, and
  `delete_schematic_component` already refuses the same way for the same
  reason. Re-embed the definition (re-place the symbol, or restore the
  `lib_symbols` entry) and retry.
- For `stale_target` naming a marker with no UUID, delete and re-add that
  marker so KiCad's writer gives it one.

## Unreleased: explicit live-board synchronization for CLI DRC

`run_drc` and `get_drc_violations` accept `sync_live_board: false` and
`refill_zones: false` by default. Existing calls continue to check the saved
file without requiring IPC. To check recent editor changes, finish other
mutations first and call either tool with `sync_live_board: true`. Konnect binds
the exact requested open board, optionally refills and waits for KiCad to stop
returning `AS_BUSY`, saves that same document, verifies its native snapshot
against the saved file, and only then runs CLI DRC. A changed source during DRC
invalidates the result rather than being reported as synchronized evidence.

Both summaries add `source: "saved_file"`, `live_board_synced`,
`zones_refilled`, and `zone_refill_source` (`"ipc"`, `"kicad_cli"`, or null).
These do not turn CLI DRC into an unsaved in-memory check. Without synchronization,
CLI zone refill is analysis-only and does not save the open editor's board.
Standalone `refill_zones` now binds its requested board, waits for completion,
returns a structured refusal on failure, and reports `saved: false` on success.

Wrong targets and unavailable IPC stop before mutation/CLI. Once refill/save
has been attempted, failed or unproven work returns `mutation_outcome_uncertain`
with the requested path and operation. Inspect/reconcile the editor and saved
file before retrying; Konnect does not replay a possibly applied mutation or
fall back to a different board. See [DRC synchronization](DRC_SYNCHRONIZATION.md).

## Unreleased: `rotate_schematic_component` reconciles junctions (minor release)

A turn relocates a symbol's pin endpoints exactly as a move does, but only
`move_schematic_component` re-judged the junction dots at the points its pins
left and arrived at. A pin turned off a wire left its dot behind with nothing
to justify it, and — the half that changes the design rather than the picture —
a pin turned *onto* a wire mid-span got no dot, so KiCad left it off the net it
visibly touches (#615).

`rotate_schematic_component` now makes the same reconciliation call, with the
same rules: a dot is pruned when nothing is left to justify it, and one is
added only where a pin has landed mid-span on exactly one wire with no
no-connect flag at the point.

The response gains the two keys the move has always carried, always present:

- `junctions_added_count`
- `junctions_pruned_count`

Both are derived from the reconciliation that actually ran, not predicted. No
argument changed and no existing response key changed in name, shape or
content. A caller that reads only the old keys is unaffected; one that assumed
a turn never touches junction dots will now see the dots it used to have to
repair by hand.

Sheets a previous version left behind are not migrated by this change. A
stranded dot costs no connectivity and can be deleted at leisure; a *missing*
dot does cost connectivity, and the repair is `add_junction` at the pin's
coordinate, or re-running the turn.

## Unreleased: fixed tool arguments reject unknown keys (patch release)

Following #551's validator, #546 closes fixed tool argument records before
advertising and compiling their schemas. Unknown top-level arguments and unknown
fields in fixed nested records now return `invalid_argument` before a handler
runs. For example, use `query` rather than `part_name` in `search_symbols`, and
`rotation` rather than `rotaton` in `create_footprint.pads`. A missing required
field still takes precedence over other validation errors. Correct the named
field using the current `tools/list` schema, then retry; a refusal writes nothing.

Intentional caller-keyed maps remain extensible: schematic custom `fields`,
field-name keys in `field_placements`, routing `net_map`, and template
`net_mappings`. Fixed placement records inside `field_placements` are closed.
Arbitrary configuration `value` data remains unrestricted. The optional footprint
model `offset`, `scale`, and `rotate` records now declare numeric `x`, `y`, `z`
fields; omission keeps the existing coordinate defaults.

Previously ignored extension keys on fixed records are no longer accepted.
Tool authors must explicitly declare `additionalProperties: true` or a value
schema for intentional caller-keyed maps. Catalogue tests inventory those
exceptions. This changes refusals, not success-response shapes or tool counts.

## Unreleased: `annotate_schematic` reports what it did, and numbers the way eeschema does (minor release)

`annotate_schematic` returned the string "Annotation complete." whatever it
had done, including nothing: two symbols that already shared a designator were
never candidates, the file was left byte-identical, and KiCad's netlister then
merged them into one component (#454). It also rewrote only the
`(instances …)` reference of a `?` symbol and left its `(property "Reference"
…)` behind, so KiCad and Konnect's own readers disagreed about the designator.

The response is now JSON, with the shared `outcome` envelope:

- `assigned` — one entry per `(uuid, unit, project, path, from, to)` that was
  written;
- `project` — the project whose instance records were annotated; `paths` —
  its sheet instances in the file; `duplicates_before` and
  `unannotated_before` — what the file looked like; `outside_project` —
  symbols whose instance records name only other projects, left untouched;
- `unresolved` — designator groups the tool will not decide for you, each with
  its `project`, `paths`, `uuids` and a `reason`;
- `written`, `dry_run`, `resolve_duplicates`, `remaining_unannotated`;
- `outcome.status` is `complete` only when the committed file, read back, has
  no `?` and no duplicate in the project; `partial` when `unresolved` is
  non-empty; `failed` when the tool refused before writing; `uncertain` when
  the write or the readback could not be proven.

A caller matching "Annotation complete." must update. No argument was renamed
or removed. Persistence conflicts return `mutation_outcome_uncertain` with an
`uncertain` outcome and `inspect_target` retry scope, not a no-write refusal:
the conditional writer can detect a conflict either before replacement or
after replacement during readback, without distinguishing that timing in its
error. Reload and inspect the schematic before retrying; applied work is not
known on this path and the response does not claim that nothing was written.
Three optional arguments are added: `resolve_duplicates` (default
false) renumbers all but the first of each group of separate parts sharing a
designator, in ascending X, which is what eeschema's "Reset existing
annotations" produced on the fixture; `dry_run` (default false) returns the
plan without writing; `project` (string) names the project whose instance
records to annotate.

References are unique across a project (KiCad's flat list), so numbers are
reserved per project across every sheet instance the file carries: a reused
sheet's instances each get their own number, and a designator shared across
instances is a duplicate unless it is the units of one package. Exactly one
project's instance records are annotated — the schematic's owning project (its
`.kicad_pro`, resolved the way #189 resolves ownership), else the only project
the file names, else `project` — and anything ambiguous is refused with the
candidates (`invalid_argument`, field `project`). Other projects' records are
never edited. Multi-unit parts are recognised from the embedded library
definition: units with the same definition and value, distinct within the
declared unit count, are one package. Unannotated units of one package get one
shared number; a shared designator that could be a package (a unit repeats, is
out of range, the values differ, or the definition is missing) is never
renumbered, even with `resolve_duplicates`, and is reported with a reason.
When the owning project is proven, the numbers used on its other sheets are
reserved by walking the sheet tree from the root (`other_sheets_consulted`
lists them; `other_sheets_unreadable` names any that could not be read), so a
child sheet annotated on its own never hands out a number the root owns.
Duplicates that already exist across sheets are not detected or renumbered
here; annotating a hierarchy from its root is tracked in #463.

Numbering now follows eeschema's Tools → Annotate defaults, measured on
eeschema 10.0.5 rather than assumed: ascending X per sheet instance, the first
free number for a prefix (a gap between `R1` and `R3` is filled; before, the
next number was always the maximum plus one), and `#`-prefixed designators
spelled `#PWR01`, `#PWR010`, `#PWR0100` as eeschema spells them (before:
`#PWR1`). Both places a designator lives are written.
## Unreleased: `rotate_schematic_component` carries field text round (patch release)

`rotate_schematic_component` wrote the symbol's new angle and nothing else.
A field's `(at …)` is an absolute sheet coordinate, not an offset from the
body, so Reference and Value stayed where the old orientation had put them:
a `Device:LED` placed horizontally and then turned to 90° kept its designator
2.54mm above the origin, which is the middle of the vertical body and the wire
into its anode (#612).

The turn now rotates each field's position about the symbol origin, so a
symbol placed unrotated and turned afterwards ends up field-for-field
identical to the same symbol placed at that angle outright — the placement
path has transformed its anchors since #101. A reflected body turns its fields
the opposite way, matching the rotate-then-mirror order a placement uses —
counting axes, not the token's presence, so `(mirror xy)` (two reflections, a
proper 180° turn) and `(mirror none)` turn the unreflected way.

A field's stored *angle* is unchanged, and deliberately so: KiCad adds the
symbol's rotation to it when drawing, so turning the angle here would draw the
text at twice the angle.

No argument and no response key changed. A caller that positioned a field by
hand keeps that offset, carried round with the body rather than reset — the
offset rotates, so its absolute coordinates change.
`reset_schematic_field_positions` remains the only tool that discards a manual
offset and puts fields back on their library anchors; it is also the repair
for sheets a previous version left behind, and it already accounted for
rotation.

## Unreleased: `trace_from_point` reports pins and junctions (minor release)

`trace_from_point`'s answer to "what is at this point" listed wires and labels
only. A component pin and a junction dot at the same coordinate were omitted,
and no `pins_here` or `junctions_here` key was present to be empty, so a caller
could not tell the two had been skipped (#539).

The response gains two arrays, always present:

- `pins_here` — every placed pin at the point, each carrying `reference`,
  `pin`, `pin_name`, `electrical_type`, `x` and `y`, spelled as
  `find_orphan_items` already spells them. Pins stacked on one point are all
  reported. Only the unit actually placed contributes, so a multi-unit symbol
  never answers with another unit's pins.
- `junctions_here` — every junction dot at the point, as `x`/`y`.

`x`, `y`, `net`, `wires_here` and `labels_here` are unchanged in name, shape
and content, and the `tolerance` argument governs the two new arrays exactly as
it governs the existing ones. A caller that reads only the old keys is
unaffected; one that treated the absence of `pins_here` as "no pin here" was
reading a key that never existed.

`tolerance` is now declared `exclusiveMinimum: 0` and refused with a structured
`invalid_argument` when it is zero, negative or not a number — previously any
value was accepted. `tolerance: -1` used to answer successfully with `net`
named and all four `*_here` arrays empty: the net comes from the shared net
graph, which resolves at its own fixed tolerance and never saw the argument, so
the response asserted a net at a point it also reported as bare. The argument
still does not reach `net`; what changes is that a value which can only produce
empty evidence is no longer answered. `find_orphan_items` already refused the
same input.

Hierarchical sheet pins and no-connect flags can also sit on a point and are
still not reported. The tool description now names the four kinds it does
report, and says so, rather than promising "what is at that point" in the
abstract.

## Unreleased: tool input schemas are enforced at dispatch (patch release)

Konnect now compiles and caches every advertised Draft 2020-12 tool-input
schema and validates calls before invoking either a domain handler or a
meta-tool. A malformed present option no longer looks like omission and cannot
silently select the option's default.

Calls newly return a structured `invalid_argument` naming the failing field
when they contain a wrong JSON type, a fractional value for an integer field,
a declared out-of-range value, or an unknown property inside an object that
explicitly declares `additionalProperties: false`. Nested fields use paths such
as `components[0].unit` and `graphics[0].colour`. Objects without a closed
schema remain open; this change does not invent new restrictions that the
served schema does not declare.

JSON numbers `2` and `2.0` both satisfy an integer schema; `2.7` does not.
Omitted optional arguments and their existing defaults remain compatible.
`add_schematic_component`, `batch_place_components`, and `replace_component`
now declare `unit >= 1`. `add_hierarchical_sheet` and `edit_sheet` now declare
and directly enforce `width > 0` and `height > 0`; refusals occur before a
schematic write or child-file creation. Correct the named field and retry.

Tool authors now get an immediate failure while constructing the catalogue if
an advertised schema cannot compile. Catalogue conformance tests cover schema
compilation, cached reuse, closed nested objects and unions, declared bounds,
wrong-typed options, integer-number compatibility, and no-write-on-refusal.

## Unreleased: placement preserves library Value and Footprint (minor release)

`add_schematic_component`, every entry of `batch_place_components`, and
`add_power_symbol` now copy the resolved library symbol's `Value` and
`Footprint` onto the placed instance (#506). Previously, placement derived
`Value` from the name after `:` in `lib_id` and always wrote an empty
`Footprint`, even when the embedded library definition already carried both.
KiCad's netlister reads the instance fields and does not fall back to that
embedded definition, so the old result could have the wrong value and no
`(footprint …)` node downstream.

The single and batch placement schemas gain optional `footprint` arguments.
Explicit `value` and `footprint` values win over the library defaults. An
explicit empty `footprint` therefore clears a library assignment. A library
whose Footprint is genuinely empty, such as the generic `Device:R`, remains
empty. If a malformed library omits Value, the historical symbol-name fallback
is retained.

The placed-file readback now binds and verifies both effective fields. The new
optional arguments are backward compatible. Existing response field names are
unchanged, but their `Value` and `Footprint` contents now match the library
rather than Konnect's discarded defaults.

## Unreleased: Windows discovers KiCad's IPC endpoint (patch release)

No tool, argument, or response field changed shape. What changes on **Windows**
is which path a hybrid tool takes, and therefore the value it reports in
`source`.

NNG maps `ipc://` to a named pipe there, which has no filesystem presence, and
discovery probed the candidate path as a file — so it never found anything.
A Konnect launched by an MCP client with no `ipc_address` and no
`KICAD_API_SOCKET` reported the transport unreachable even with KiCad running,
and every hybrid tool took its direct-file fallback while KiCad held the board
open (#529). Discovery now looks the same path up in the pipe namespace.

For a Windows caller this means:

- Tools that reported `source: "file"` with a `fallback_reason` of
  `transport_unreachable` now report `source: "ipc"` when KiCad is running with
  the API server enabled, and their edits go through KiCad rather than to the
  saved file.
- Live-only tools (`update_pcb_from_schematic`, `refill_zones`, the routing
  tools) stop refusing and start working.
- `get_installation_info` reports the discovered endpoint instead of a null one.

Nothing changes on Linux or macOS, where discovery already worked, and nothing
changes for any caller that set the address explicitly. Discovery still only
ever runs at startup, so a server launched before KiCad stays unresolved for its
lifetime — see `docs/TROUBLESHOOTING.md`.

## Unreleased: `check_clearance` adds explicit courtyard spacing (minor release)

`check_clearance` returned the straight-line distance between two footprints'
placement anchors under a description that said "physical clearance". Read as
copper-to-copper spacing, a `21.125` answer stood in for a courtyard gap of
about 3 mm (#410). Footprint size and shape were never considered.

Stage 1 changed nothing about the number and everything about what the response
says it is:

- `measurement: "anchor_to_anchor"` and `anchor_distance_mm` are added; the
  latter is the value `distance_mm` carried.
- `distance_mm` is kept, identical, as a **deprecated** alias, listed in a new
  `deprecated_fields` array so a consumer can see it without reading prose.
- `note` states in words that this is not pad, trace or courtyard clearance.
- The tool description and the directory row no longer claim clearance, and
  point to `run_drc` for the question the old description implied.

Stage 2 adds an optional `mode` argument. The compatibility default remains
`"anchor"`, so existing calls keep the same fields and number. The new
`"courtyard"` mode returns:

- `measurement: "courtyard_bbox_edge_to_edge"` and
  `courtyard_clearance_mm`, measured between the axis-aligned board-space hulls
  of the two transformed, authored courtyards;
- `geometry1` / `geometry2`, including each measured bbox and footprint
  rotation, plus `side1`, `side2`, and the actual `source` (`ipc` or
  `saved_file`);
- `overlaps: true` with a zero distance when the two same-side hulls overlap;
  touching hulls have zero distance and `overlaps: false`;
- a structured unavailable result when either authored courtyard is absent or
  unreadable. Pad and anchor fallbacks are deliberately not substituted;
- `reason_code: "opposite_board_sides"` and `applicable: false` for footprints
  on opposite sides, rather than presenting their projected bboxes as a
  placement collision.

Both modes read a read-only IPC serialization when the requested board is open
in KiCad and otherwise read the saved file. This remains a placement-spacing
tool, not an electrical-clearance oracle: run `run_drc` for copper clearance.
For non-cardinal footprint rotations, the reported bbox is the conservative
axis-aligned hull of the rotated courtyard artwork.

`distance_mm` remains available only in `anchor` mode as a deprecated alias;
this additive release does not remove it. This terminal stage closes #410.

## Unreleased: `update_pcb_from_schematic` reports an unassigned footprint (minor release)

`kicad-cli sch export netlist` writes no `(footprint …)` node for a symbol whose
`Footprint` property is empty, and the sync required one for every component.
One footprint-less symbol — a legitimate state for a generic `Device:R` whose
package has not been chosen yet, and until #506 every symbol Konnect itself
placed — failed the whole sync with *KiCad netlist node is missing footprint*,
naming no component and blocking every other one (#507).

Such a component is now **reported, not fatal**, the way eeschema's own Update
PCB dialog says "footprint not assigned" and continues:

- `coverage.unassigned_footprint` — a `{planned, applied}` count pair beside
  `skipped_by_flag`.
- top-level `unassigned_footprints` — one entry per component: `reference`,
  `value`, `lib_id` (from the export's `libsource`, when present), `symbol_path`,
  and `board_state`: `absent` (nothing is added; the part is not counted under
  `footprints_added`) or `kept` (a footprint with that identity or reference
  already exists on the board and is left exactly as it is — it is counted as
  matched, so it does not appear under `board_only_preserved`).
- A wired pin of such a component is dropped from the plan's net assignments,
  since there is no pad to carry it. Its saved `on_board` / `in_bom` flags
  still apply.

A schematic whose every component is unassigned is a `noop` that still names
them, and a genuine conflict still clears `changes` while keeping the list.

Both additions are additive; `status`, `changes`, `diagnostics` and every
existing count keep their names and meanings. No argument was renamed or
removed.

## Unreleased: IPC health responses say why KiCad did not answer (minor release)

`check_kicad_ui` and `open_project` gain an `ipc_failure` field (#532). It is
`{ "kind", "message" }` when a failure kind was established, where `kind` is
one of `not_configured`, `no_listener`, `access_denied`, `handshake_failed`,
`transport_error`, or `request_failed`. `request_failed` means the request
did not complete and may have reached the endpoint: an explicit KiCad error
status in `message` proves receipt, a receive timeout or malformed reply does
not. `access_denied` means the operating system refused this account; it does
not prove who owns the endpoint. Before this, every one of those
surfaced only as `ipc_responsive: false` or `ipc_available: false`, so a KiCad
that was listening but refused this account looked exactly like one that was
closed.

`ipc_failure: null` means **no failure kind was established**. That happens in
two cases: the Ping succeeded with `AS_OK`, or `check_kicad_ui`'s own
`timeout_seconds` deadline expired before the Ping finished. The second case
still reports `timed_out: true`. A listener that accepts but never negotiates
takes NNG's 10-second limit to report `handshake_failed`, longer than the
default `timeout_seconds` of 5.

No existing field was removed or renamed. Two existing values change wording:

- `open_project`'s `message` for a KiCad that did not answer now depends on the
  kind. Before, every unanswered call returned "KiCad IPC is not reachable.
  Start KiCad and enable the IPC API, or work in file-only mode." That message
  is kept for `not_configured`, `no_listener`, and `transport_error`. The other
  three kinds return:
  - `access_denied`: "The KiCad IPC endpoint refused this account, likely a
    different account or a restrictive ACL; run Konnect as the same
    operating-system user as KiCad. See ipc_failure."
  - `handshake_failed`: "A listener at the KiCad IPC address did not complete
    NNG's handshake, so it is probably not KiCad; see ipc_failure."
  - `request_failed`: "The KiCad IPC request did not complete and may have
    reached the endpoint; a KiCad status in ipc_failure proves receipt, and
    KiCad may still be starting."
- An IPC tool whose dial fails now says in its error text why the dial failed
  ("Nothing is listening there…", "…it refused this account…", "…did not
  complete NNG's handshake…"), instead of one sentence listing every possible
  cause. Callers classifying these errors by type are unaffected. Callers
  matching the old text must update.

## Unreleased: atomic validation for schematic edits (minor release)

`edit_schematic_component`, `add_component_annotation`, and
`group_components` now parse and semantically validate the exact prospective
command result before committing it. A missing UUID, wrong reference, unit,
library, hierarchy path, position, rotation, mirror, requested property, or
field-text placement returns `stale_target` while leaving the schematic
byte-for-byte unchanged. `edit_sheet`, `move_sheet`, `import_sheet_pins`,
`add_sheet_pin`, `edit_sheet_pin`, and `delete_sheet_pin` use the same contract:
their prospective document must contain exactly one sheet with the bound UUID
and its complete serialized state must match the edited intent. `delete_sheet`
must instead prove that the bound sheet UUID is absent. A mismatch refuses the
operation before writing.

Committed-file readback remains an independent backstop. If that second
observation cannot load the schematic or cannot prove the requested component
or hierarchy state, every tool listed above now returns the new structured
error kind `mutation_outcome_uncertain`, carrying `operation`, `path`, and
`reason`. The message explicitly says the file may have changed and must be
reloaded and inspected before retrying. It does not return `stale_target`,
because that kind is used for a pre-commit refusal whose no-write result has
been established.

No tool or argument was renamed or removed. The new error shape is an additive
public response change and is planned for the next minor release.

## Unreleased: mirrored schematic placement (minor release)

`add_schematic_component` and every entry of `batch_place_components` accept a
new optional `mirror`, and the schematic component responses gain two boolean
fields. Nothing is renamed or removed, and omitting `mirror` reproduces the
previous behaviour exactly: no `(mirror …)` token is written and the symbol is
placed upright.

`mirror` uses the file format's own vocabulary — `"x"`, `"y"` or `"none"`.
`"x"` negates screen-Y and `"y"` negates screen-X, which is eeschema's meaning
for the tokens it writes, and `"none"` is the explicit spelling of unmirrored,
which KiCad records by omitting the token rather than writing one. There is
deliberately no way to ask for both axes: KiCad stores at most one mirror flag
per symbol and reflecting both is rotation 180, so a pair of booleans would let
a caller request a state the format cannot hold. Mirroring is applied after
rotation, and it does not substitute for rotation 180 on a symbol whose pins
are not symmetric — 180 keeps every pin's coordinates correct but reverses
their visual order along the body.

Any other value is refused rather than dropped. `add_schematic_component`
returns `invalid_argument` naming `mirror` and writes nothing;
`batch_place_components` refuses only the offending entry, reporting it in
`errors` and placing the rest, as it already does for every other per-entry
problem. A caller who misspells the axis is asking for a reflection, so placing
the symbol upright and reporting success is the failure this argument exists to
end.

A placement's field anchors follow the mirror. `Reference` and `Value` are
positioned through the same transform as the symbol body, which previously
hardcoded an unmirrored frame, so a mirrored symbol's field text sat on its
unmirrored side (#101).

Responses add `mirror_x` and `mirror_y`, at the top level and in each `units[]`
entry, for every tool that shares the committed-file component readback:
`add_schematic_component`, `batch_place_components`, `add_power_symbol`,
`edit_schematic_component`, `move_schematic_component`,
`rotate_schematic_component` and `add_component_annotation`. Both are read from
the reparsed committed schematic beside `x`, `y`, `rotation`, `lib_id` and
`units[].field_placements` — never echoed from the request — and the requested
axis is bound as placement intent, so a reflection that did not reach the file
refuses with `stale_target` rather than reporting success. The mutations that
do not change a reflection bind the axis the file already carries, which makes
them assert they left an existing one alone; `add_power_symbol` binds none,
because a power symbol is placed upright.

No tool mirrors an already-placed symbol: changing a placement's reflection
still means deleting it and placing it again. This additive change is planned
for the next minor release; no tool or argument was renamed or removed.

## Unreleased: DRC checks schematic parity on every run (minor release)

`run_drc`, `get_drc_violations`, `run_design_review`, `validate_for_manufacturing`
and `export_manufacturing_package` all run `kicad-cli pcb drc` through one
path, and that path never passed `--schematic-parity`. KiCad gates the parity
test behind that flag; without it, KiCad 10 still writes the `schematic_parity`
key as an *empty* array, so the parser from #245 read "never asked" as
"checked, none found" and every board reported parity `0` (#516). The flag is
now always sent.

Two visible consequences:

- **`schematic_parity` becomes non-zero on boards that reported `0`**, and the
  review and readiness verdicts that fold DRC in change on them. A board whose
  footprints disagree with its schematic — KiCad's own `ecc83` demo included —
  now reports `footprint_symbol_mismatch` / `missing_footprint` /
  `extra_footprint` items under `schematic_parity` and is no longer `READY` /
  `LOOKS GOOD` on that evidence.
- **`null` keeps meaning "not checked", and gains a reason.** With the flag on
  and no root schematic for the board's project, kicad-cli exits 0, prints
  *Failed to fetch schematic netlist for parity tests* to stderr, and writes an
  empty array — the same silent zero, one layer down. Konnect reads that
  statement and reports `schematic_parity: null`, lists it under
  `categories_not_reported`, and adds `schematic_parity_diagnostic` quoting
  KiCad's statement and naming the root schematic the test reads: the one
  sharing the board's file stem, i.e. the board's own project's root. A
  project of a different name beside the board is not consulted, because KiCad
  does not consult it either. A non-empty parity array is always kept as
  KiCad's evidence, whatever that lookup says. Review and manufacturing
  diagnostics carry the same reason.

`schematic_parity_diagnostic` is additive and absent when parity was checked
or when the kicad-cli never reported the category at all. No tool, argument, or
existing field was renamed or removed.

## Unreleased: `add_mounting_hole` uses KiCad's shipped footprint names (minor release)

`add_mounting_hole` wrote `MountingHole:MountingHole_{drill:.1}mm`. KiCad 10
ships no plain `MountingHole_3.2mm` — 3.2 mm exists only as the M3 family — and
spells round sizes without a decimal (`MountingHole_3mm`), so the default call
and every integer size produced a name no stock KiCad resolves (#462).

The lib_id now comes from a table of the footprints KiCad 10 ships (`3.2` →
`MountingHole_3.2mm_M3`, `3` → `MountingHole_3mm`, `4.3` →
`MountingHole_4.3mm_M4`, …), and a drill with no shipped footprint is refused
with `invalid_argument` on `drill_diameter` listing the shipped sizes, before
anything is written. When `MountingHole.pretty` resolves from the machine
(project or global `fp-lib-table`, or a discovered install), the hole placed is
KiCad's own library footprint, as `place_component` would place it; otherwise
Konnect's unplated-hole geometry is written under the shipped name. That
geometry's pad now equals the drill (no `+0.5` annulus), matching KiCad's plain
`MountingHole_*` footprints.

Results add `geometry` (`"library"` or `"inline"`) and, for inline geometry,
`geometry_note`. The `footprint` field is now read back from the saved board
rather than repeated from the request. No tool or argument was renamed or
removed; `drill_diameter` keeps its default of 3.2.

## Unreleased: DRC item ownership (minor release)

`run_drc` and `get_drc_violations` now say what owns each item of each
violation. Every item in `violations`, `unconnected_items`, and
`schematic_parity` gains up to four additive fields:

- `ownership_status`: `resolved`, `uuid_missing`, `not_found`, `ambiguous`, or
  `unavailable`.
- `owner`: `{"kind":"board"}` or a footprint owner with its reference and UUID
  when resolved; `null` for every unresolved state.
- `item_kind`: the board node head when uniquely resolved.
- `layer`: the item's single layer when uniquely resolved.

Ownership is derived only from exact UUIDs in the saved `.kicad_pcb`; it is
never inferred from KiCad's prose. Duplicate UUIDs are `ambiguous` rather than
resolved according to file order. If the saved board cannot be reread or
parsed, every item is marked `unavailable` and the report adds
`ownership_diagnostic` with the reason, while retaining all original DRC
findings.

Footprint ownership does not make a finding false. Footprint-owned `Edge.Cuts`
is fabrication geometry; ownership identifies whether the board or footprint
definition is the likely repair location. KiCad's existing `description`,
`pos`, `uuid`, `severity`, and `rule` fields remain unchanged.

## Unreleased: `create_symbol` draws a symbol body (minor release)

`create_symbol` accepts `graphics`, an array of drawing primitives, at the top
level beside `pins` and per unit inside `units[]`. The vocabulary is
`set_footprint_graphics`'s — `line`, `arc`, `rect`, `circle`, `poly`, with points
as `{x, y}` and `stroke_width_mm` — generated for both tools by one function, so
the two domains cannot drift. Only `fill` differs: a symbol adds KiCad's pale
`background`, which is what the stock libraries use for a body box.
`set_footprint_graphics`'s own schema is unchanged.

**Whether the key is present is itself the contract, and `[]` is present.**
Omitting `graphics` keeps the existing behaviour exactly: the automatic body
rectangle is sized to the pin names, and pins are slid out to the edge it
computes. Supplying `graphics` suppresses that body for that unit — including
`graphics: []`, which asks for a symbol with no body at all and cannot be
expressed any other way. Supplying it also means pin `x`/`y` are written exactly
as given rather than moved, which is the reason the feature exists.

Two consequences follow for callers who combine `graphics` with older arguments:

- A `glyph` on a unit that also supplies `graphics` is not drawn; the response
  carries a warning saying so, rather than discarding the request silently.
- A triangular `glyph` (op-amp, buffer, inverter, schmitt) carrying power pins
  normally moves them to a generated rectangular power unit, because the
  triangle's apex has no room for their names. **With `graphics` supplied that
  split no longer happens**, since a body the caller drew has whatever room they
  gave it, and moving the pins would overwrite the coordinates they supplied. A
  caller relying on the split must omit `graphics` for that unit.

`units[].body` reports `"graphics"` when geometry was supplied, alongside the
existing `"rectangle"` and glyph names. No tool, argument, or existing response
field was renamed or removed.

**Four request shapes that previously returned success now fail**, because each
wrote something other than what was asked for:

| request | before | now |
|---|---|---|
| `rect`, `circle` or `poly` with no `fill` (schema-required) | `(fill (type none))` written | `invalid_argument` naming `graphics[i]` |
| any primitive carrying a key outside its schema | key ignored, the rest drawn | `invalid_argument` naming `graphics[i].<key>` |
| a pin with no numeric `x`/`y` in a unit supplying `graphics` | pin written at `(0 0)` | `invalid_argument` naming `units[i].pins[j].x` |
| `graphics` present but not an array | read as absent; automatic body and success | `invalid_argument` |

Nothing is written to the library file in any of those cases. Callers that
depended on the defaults must now send `fill`, drop the extra key, or supply the
pin coordinates. This additive argument and these refusals are planned for the
next minor release.

## Unreleased: `estimate_cost` and `validate_for_manufacturing` count copper structurally (minor release)

Both tools counted copper layers by finding the substring `signal)` in the
board text, which misses every `power`, `mixed` and `jumper` copper layer and
quoted a six-layer board as two-layer (#461). Both now read the `(layers …)`
table through the same function `get_board_info` uses, so the three tools
report one number for one file.

`estimate_cost` keeps its optional `layers` argument as the count to quote at.
Two additive response fields make an override visible instead of silent:
`board.board_copper_layers` (what the file declares) beside the existing
`board.copper_layers` (what was priced), and a top-level `warnings` array that
names the discrepancy when they differ, or states that the file declares no
copper layers at all. Omitting `layers` now prices at the board's declared
count rather than a substring count clamped to a minimum of two; a board with
no `(layers …)` table reports `0` and a warning rather than an invented `2`.
`validate_for_manufacturing`'s `board_info.copper_layers` changes value on any
board with non-`signal` copper; its shape is unchanged.
## Unreleased: schematic nets resolve by identity (minor release)

One electrical net can carry several names — a rail named by a `+3V3` power
symbol that also has a `VCC` label, a local label on a net that also carries a
global one — and KiCad nets a sheet by name as well as by wire, so two segments
each carrying a `SIG` label are one net. The shared net graph now joins
same-named points, and every audit compares net *identity* rather than a net
name. Where a name is reported it is the one KiCad's netlister would choose:
global label, then power symbol, then local label, then hierarchical label,
ties broken on the name ascending.

Two response shapes change meaning:

- `audit_power_rails.power_nets` lists one entry per **net**, named the way
  KiCad names it. It previously listed one entry per name, so a rail named by
  both a power symbol and a label appeared twice, as did a rail named by two
  power symbols on separate stubs. `summary`'s rail count follows. A consumer
  counting rails gets a smaller, truer number; one matching a specific string
  should match the KiCad name, since an alias may no longer appear.
- `get_connected_items.nets` is sorted and deduplicated, where it was in
  `HashSet` order. Its `labels` array now carries every label on the queried
  component's nets rather than only those spelling a net its winning way, and
  its `wires`/`connected_components` now include items on a net that carries no
  label at all, which were omitted entirely.

Two more tools change what they report, through the shared graph rather than
through any code of their own:

- `find_shorted_nets` keys off the same label-to-root relation, so a net
  carrying more than one name is now reported as a short. On a sheet where a
  rail is named by a power symbol and labelled for readability, that is a new
  finding per such net — five on this change's own fixture (`+3V3`/`ALT`/`VCC`,
  `RETURN`/`GND`, `SYS`/`+5V`/`PULLUP`, `AAA`/`ZZZ`, `MIX`/`MIX_H`), reported as
  one group per net rather than one per pair. It is the same condition KiCad's
  own ERC reports as `multiple_net_names`, and the tool description now says so.
- `points_on_net(name)` resolves the whole merged net, so `get_net_components`,
  `get_net_connections` and `count_net_connections` return the complete net for
  any name on it. Querying an alias — `VCC` on a rail KiCad calls `+3V3` —
  returns everything on that rail rather than the segment the alias sits on.

Findings change with them, in the direction of fewer false positives:
`audit_decoupling`, `audit_power_rails` and `audit_connections` no longer report
a decoupled rail as undecoupled, a rail twice, or a fitted pull-up as missing
when the capacitor or resistor sits on another segment of the same net. The
ground skips read every name on a rail, so a `GND` net that a global label
renames is skipped rather than reported as an undecoupled power rail.

`net_at`-backed reporting — `get_pin_net`, `get_component_nets`,
`trace_from_point`, `export_netlist_summary` — returns a stable name across
processes. For a net with one name the answer is unchanged; for a net with
several, callers that happened to see an alias now always see the KiCad name.

No tool, argument, or response field was renamed or removed. These changes are
planned for the next minor release.

## Unreleased: `find_single_pin_nets` counts pins, not labels (minor release)

`find_single_pin_nets` counted label instances per net name, so an ordinary net
— one label on a wire reaching two or more pins — was reported, and a genuine
single-pin net disappeared as soon as it carried a second label. Membership now
comes from the shared net graph: a net is reported when it reaches **at most one
pin**, zero included, since a label whose net reaches nothing is the orphan label
and the deleted-component stub the tool is sent looking for. Hierarchical sheet
pins count as pins; a power symbol's own pin does not, or the rail reaching
exactly one component pin would be hidden.

Every existing field remains with its existing meaning: `single_pin_net_count`,
and per net `net`, `x`, `y`, and `type`. `type` is still the kind of the *first*
label found, so a consumer matching on it is unaffected.

Results add five fields:

- `pin_count` — pins the net reaches, `0` or `1` for a reported net.
- `label_count` — label instances naming it, the value the old membership rule
  used. A net with no label at all is a different smell, so the count is kept.
- `pins` — the reached pin, as a one-or-zero-element array of the same
  `component_pin` / `sheet_pin` objects the other connectivity tools return.
- `label_types` — every distinct label kind naming the net, sorted. Local labels
  are extracted first, so a net carrying both a local and a global label reports
  `type: "NetLabel"` and says nothing about the global one; this field does.
- `cross_sheet_unverified` — true when any label naming the net can carry it off
  this sheet (global, hierarchical, or a power symbol). The answer is per sheet,
  and a flagged net is a lead rather than a finding. Such nets are still
  reported: a rail reaching one pin on this sheet is worth showing.

**`single_pin_net_count` changes meaning**: it now counts nets reaching at most
one actual pin, not nets named by exactly one label instance. A consumer reading
it as a defect count gets a smaller, truer number and needs no migration; one
that had learned to ignore this tool's noise can stop. No consumer can keep the
old set, since it was wrong in both directions.

No tool, argument, or existing response field was renamed or removed.

## Unreleased: guarded PCB file fallback reports its observed reason

Hybrid PCB mutation tools may use their existing direct-file fallback when
KiCad IPC is unreachable or when a reachable KiCad positively reports that the
requested board is not open. A successful file-path result now includes
`fallback_reason.kind` (`transport_unreachable` or `board_not_open`) and
`fallback_reason.message`; its warning is derived from that same observation.
Existing `source: "file"` and operation-specific fields remain unchanged.

If KiCad answers but any relevant open PCB document identity is empty, bare,
malformed, unresolved, duplicated, or otherwise prevents a complete comparison,
the tool fails closed with structured error kind `ambiguous_open_board` and the
requested `path`. No IPC mutation or file mutation is attempted. Callers should
make KiCad's open documents identifiable, then retry; they must not treat this
error as permission to edit the saved file directly.

## Unreleased: `score_placement` reports interface filter caps (minor release)

`score_placement`'s decoupling deduction no longer fires on a capacitor placed on
a connector's own pins for cable/EMI filtering (#411). A cap beyond its value
family's limit from the nearest shared-net IC is exempted only when a `J*`
connector carries **every** net that cap carries and its courtyard edge is within
that same limit of the cap's center. The cap must be on the connector's face,
unless the connector's pads carrying those nets reach both copper faces — a
plated through-hole pin, not an SMD one.

Results add `interface_filter_caps`: one entry per exempted cap, with
`reference`, `value`, `connector`, `connector_distance_mm`, and `limit_mm`. It is
evidence rather than a score — an exempted cap deducts nothing, and a waiver that
vanished silently would be indistinguishable from a check that never ran. The
array is empty on boards without such caps.

No tool, argument, or existing response field was renamed or removed. This
additive response field is planned for the next minor release.

## Unreleased: type-safe trace deletion (minor release)

`delete_trace` now accepts only a UUID observed in the requested live board's
trace-segment inventory. Via, zone, graphic, footprint, missing, and stale UUIDs
return `stale_target` before `DeleteItems` is sent. A successful call reads the
same board again and refuses success if the segment remains.

The existing `deleted_uuid` field remains, but it is now derived from the
observed segment rather than echoed from the request. Results add
`deleted_type: "trace_segment"`, the observed net/layer/width/endpoints under
`preimage`, and `postcondition: "absent_from_trace_readback"`. No argument or
tool was renamed or removed.

## Unreleased: schematic field text placement (minor release)

`edit_schematic_component` accepts two new optional arguments and returns one
new response field. Nothing is renamed or removed, and omitting both arguments
reproduces the previous behaviour exactly.

`field_placements` is an object keyed by field name — `Reference`, `Value`,
`Footprint`, or any custom property — whose entries each set any of `x`, `y`,
`rotation` and `hide`. An omitted member leaves that aspect as the committed
file holds it, so moving a field cannot change its visibility and hiding one
cannot move it. Coordinates are absolute schematic millimetres as KiCad stores
them, not offsets from the symbol body, and they are not snapped to the 1.27 mm
grid that component placement applies.

Because a field position is absolute it belongs to one placement, so `unit`
names which placed unit `field_placements` applies to. It is required when `x`
or `y` is given and the component has more than one placed unit; that request
is refused rather than writing one coordinate to every unit, which would stack
a multi-unit part's field text on a single point. `hide` and `rotation` without
a coordinate apply to every placed unit when `unit` is omitted.

Visibility is read in both forms KiCad writes: `(hide yes)` as a direct child
of the property, which is what `lib_symbols` definitions carry, and nested
inside the property's `(effects …)`, which is what KiCad writes on a placement.
Writes use the direct-child form; KiCad 10.0.6 treats the two identically.

`units[].field_placements` reports each field's `x`, `y`, `rotation` and `hide`
**as observed in the committed file**, not as requested, and is present for
every property carrying an `(at …)`. Every requested placement is first
compared against the prospective command result; a mismatch returns
`stale_target` without writing. The committed file is checked again before
success is reported. Failure of that second observation returns
`mutation_outcome_uncertain`, explicitly naming the file that may have changed.

A malformed existing placement is refused before anything is written.
`(at …)` is parsed positionally and must carry finite numeric x and y, and a
finite rotation when a third value is present; a placement with more values
than that is refused too. Previously an unparseable token was dropped and the
remaining values shifted left, so a rotation-only edit could commit a position
the file never held.

This additive change is planned for the next minor release; no tool or argument
was renamed or removed.

## Unreleased: committed schematic component mutation readback (minor release)

Schematic component placement, batch placement, field edits and renames, moves,
rotations, annotations, and grouping now bind the selected symbol UUIDs before
writing and build their success responses from one reload of the committed
schematic. Existing response fields remain. Results add observed identity and
placement evidence including `schematic`, `reference`, `uuid`, `lib_id`,
`unit_count`, `units`, `fields`, coordinates, rotation, and instance paths or
references where applicable. Grouping returns the same evidence per component.

Success additionally requires every bound unit's observed unit number, library
ID, project/hierarchy paths, x/y, rotation, and property values to match the
preselected target plus the intended mutation. Placement compares requested
Reference/Value, library and unit, and its tool's coordinate rules (component
placement snaps to 1.27 mm; power placement retains requested coordinates).
Edits preserve the other bound values; moves preserve relative unit positions,
and rotations preserve relative unit angles. Coordinate/angle comparisons allow
only serialization rounding below 0.000001 mm/degrees.

Missing, malformed, stale-revision, or wrong-document identities refuse with
`stale_target`, including mismatched intended values, before edits,
annotations, or grouping are committed. Component-target
resolution and committed readback reject duplicate UUID, reference/unit,
property, or instance identities and conflicting project, instance-unit,
or cross-unit hierarchy records
with the new `ambiguous_target` kind and include their candidates whenever
Konnect cannot prove one top-level symbol per bound UUID and one logical
reference across its units. A post-commit verification failure from edits,
annotations, or grouping returns `mutation_outcome_uncertain`, so inspect and
reload the named schematic before retrying. A move commits the symbol placement
before a separate junction-reconciliation write; if that second write or final
readback refuses, the move can already be durable. This additive response
change is planned for the next minor release; no tool or argument was renamed
or removed.

## Unreleased: connectivity-safe component deletion (minor release)

`delete_schematic_component`, `batch_delete`, and
`batch_delete_schematic_components` now resolve a complete logical component
before writing, remove only no-connect markers owned exclusively by deleted
pins, and reconcile junctions only at affected pin endpoints. Wires and labels
remain, matching KiCad's plain-delete behavior. Selecting one placed-unit UUID
through `batch_delete` deletes every placed unit of that reference.

The existing single-delete fields (`deleted`, `deleted_units`) and batch fields
(`deleted_count`, `deleted`, `errors`) remain. Single-delete results add
`deleted_unit_uuids`, plus count-and-UUID evidence for removed no-connects and
added or pruned junctions. Batch results add `deleted_components` (including
each reference's observed unit count and UUIDs), `deleted_item_uuids`, and the
same connectivity evidence fields. These values come from reloading the
committed schematic rather than echoing requested selectors.

Missing, protected, malformed, stale, wrong-document, or editor-locked targets
refuse with `stale_target` before a write. Duplicate UUID or reference/unit
identities refuse with `ambiguous_target` when Konnect cannot prove a unique
safe deletion. A post-write readback
that still observes a selected reference or UUID also returns `stale_target`;
inspect and reload the saved schematic before retrying because that refusal can
follow a committed write. This additive response change is planned for the next
minor release; no tool or argument was renamed or removed.

## Unreleased: complete schematic placement instances (minor release)

`add_schematic_component`, `batch_place_components`, and `add_power_symbol`
preserve every instance path when the saved root reuses a child schematic.
Existing inputs and response fields remain. Placement results now include
`schematic`, `project`, `instance_paths`, and observed symbol fields (`uuid`,
`added`, `reference`, `value`, `x`, `y`, `rotation`, `unit`). Batch results put
these fields in each `placed` entry; power placement retains `added_power` and
`junctions_added`. Values come from reloading the committed file.

Missing, foreign, duplicate, malformed, or obsolete saved instance paths,
references, or units return `stale_target` with `target` and `reason` before
placement writes. Repair the saved hierarchy and its complete symbol instance
metadata before retrying. Ambiguous
project ownership continues to use the existing `conflict` kind from #189.
If post-write readback cannot verify the target or symbol, `stale_target` may
follow a committed write: inspect/reload the file before retrying to avoid a
duplicate placement. This observes saved files only and does not claim an
atomic snapshot of the complete hierarchy or unsaved editor state.

See [Schematic project ownership](PROJECT_OWNERSHIP.md#placement-instance-validation)
for the acceptance matrix and limits. This additive response change is planned
for the next minor release; no tool or argument was renamed or removed.

## Unreleased: schematic ownership conflicts (minor release)

Symbol-loading operations and ERC root detection now refuse unproven or ambiguous
ancestor project ownership with the existing `conflict` kind. Previously, some
of these cases silently inherited unrelated libraries or treated the schematic
as projectless. `error.paths` names the schematic directory and every candidate
root. Restore the saved hierarchy or separate the independent document from the
unrelated project before retrying. Loose schematics with no candidate project
and adjacent library-table authority remain supported. See
[Schematic project ownership](PROJECT_OWNERSHIP.md) for the behavior and limits.

## Unreleased: Rust Specctra export is the default

`export_specctra_dsn.native_bridge_mode` now defaults to `disable`, so an
omitted value always selects the Rust/IPC exporter. This keeps the default path
free of Python and SWIG and makes its KiCad 11 direction explicit.

KiCad 10 users who deliberately want the authenticated ActionPlugin bridge can
pass `prefer` (use the native export when available, otherwise Rust) or
`require` (refuse when the native bridge is unavailable). No tool or argument
was removed.

## Unreleased: JLCPCB manufacturing files use vendor-ready names and schema

`export_manufacturing_package(fab_house="jlcpcb", include_assembly=true)` now
publishes `BOM-<project>.csv` and `CPL-<project>.csv` instead of `bom.csv` and
`positions.csv`. The CPL contains JLCPCB's documented `Designator`, `Mid X`,
`Mid Y`, `Layer`, and `Rotation` columns rather than KiCad's native position
headers. The existing `files_generated.type="pick_and_place"` discriminator is
unchanged.

JLCPCB assembly exports require `position_units="mm"`. Grouped BOM references
are individually enumerated and DNP parts are excluded from both the BOM and
CPL. A malformed CPL, compressed BOM range, or BOM/CPL designator mismatch
returns an incomplete/error result instead of an upload instruction. Generic
and other-fabricator exports retain the existing `bom.csv`/`positions.csv`
names, inclusion policy, and KiCad-native position schema.

JLCPCB CPL rotation and position corrections are now applied after KiCad's
native geometry export. The optional `jlcpcb_cpl_corrections_path` input points
to a versioned project JSON policy; exact designator overrides take precedence
over the first matching project footprint prefix, which takes precedence over
Konnect's independently verified built-in rules. See
[JLCPCB CPL corrections](JLCPCB_CPL_CORRECTIONS.md) for the policy schema.

The response adds `placement_orientation` at the top level and on the
`pick_and_place` artifact. It records policy provenance, each applied rule with
before/after values, and every unmatched footprint. Its status is always
`PREVIEW_REQUIRED` and `physical_validation` is always `false`: a structurally
complete package is not evidence that JLCPCB's selected component models are
physically aligned. Inspect every part in Component Placements before ordering.

## Unreleased: remove inputs that never affected an operation

The following optional inputs were advertised but never read by their handlers.
Keeping them would let a client believe a request was honoured when the result was
identical without it.

| Removed input | Migration |
|---|---|
| `import_sheet_pins.project_name` | Omit it. Importing hierarchical labels as sheet pins does not modify project-instance paths. |
| `refill_zones.zones` | Omit it. KiCad IPC refills every zone on the active board and exposes no per-net selector. |
| `run_drc.tests` | Omit it. `kicad-cli pcb drc` runs the complete configured ruleset. Configure rules/waivers in KiCad; `severity` and `limit` only filter Konnect's returned report. |
| `audit_decoupling.board` and `audit_decoupling.max_distance_mm` | Run `audit_decoupling(schematic)` for net-connectivity coverage, then use PCB placement/clearance inspection for physical capacitor distance. The audit never measured PCB distance. |
| `export_manufacturing_package.quantity` | Omit it from export. Manufacturing files are quantity-independent; pass `quantity` to `estimate_cost` for pricing context. |
| `validate_for_manufacturing.schematic` | Run the board validator without it. Use `check_bom_health(schematic)` for the separate schematic/BOM review. |
| `estimate_cost.schematic` | Omit it. The estimator counts placed board footprints, which are the components relevant to assembly pricing. |
| `move_connected.*` (all parameters) | The tool now refuses unconditionally: it never implemented the connected move and silently delegated to a plain symbol move while reporting connections preserved (#315). Use `move_schematic_component`, then re-route the affected nets. The parameters return when the wire-carrying move is actually built. |

These removals narrow the schema to behavior Konnect can verify. They do not change
the generated files or analysis because the removed values had no implementation.

## Unreleased: `flip_component` on an open board uses KiCad 10.0.6's native `FlipItems`

`flip_component` previously refused unconditionally whenever KiCad was
reachable, because the vendored IPC protocol had no footprint-flip command
(#604). KiCad 10.0.6 added a native `FlipItems` command, so a board that is
open live in KiCad **10.0.6 or newer** now flips through that command inside
one KiCad undo transaction — the same transform the GUI's **F** key performs,
including the footprint's 3D-model offset/rotation — and reports
`"source": "ipc"`. Konnect never saves the board on this path.

A reachable KiCad older than 10.0.6 (or any endpoint without the handler)
answers `AS_UNHANDLED`; Konnect reports this as a structured
`unsupported_capability` error naming the observed KiCad version and the
10.0.6 requirement, rather than falling back to editing the file. A mutation
that appears to succeed but whose fresh post-flip readback cannot confirm the
result returns a distinct `mutation_outcome_uncertain` error instead of either
success or a plain failure — inspect the board in KiCad and reconcile before
retrying.

The **closed-board** file fallback is unchanged and only reachable when no
live KiCad holds the named board at all: it keeps refusing any footprint whose
3D model carries a non-zero `offset.y`, `rotate.x`, or `rotate.y`, since
reproducing KiCad's own 3D-model flip transform for that path remains out of
scope. `flip_component`'s tool description and `BoardAccess` classification
changed from "requires a closed board" to live-preferred-with-fallback to
reflect this; existing closed-board callers are unaffected.

## Unreleased: `add_net` is idempotent on legacy boards

`add_net` now returns an existing legacy net's observed numeric ID without
writing when the requested name is already declared. Its JSON response adds a
`created` boolean so callers can distinguish a new insertion from an idempotent
no-op. New declarations use the board's newline convention, canonical tab
indentation, escaped S-expression text, and an ID one greater than the highest
observed top-level declaration.

KiCad 10 boards still refuse the operation before writing because they have no
top-level numeric net table. That refusal is now the structured
`unsupported_capability` kind. Create a KiCad 10 net by naming it on a pad or
copper item instead.

## Unreleased: `score_placement` discloses whether it scored a live or saved board (patch release)

`score_placement` always read the saved `.kicad_pcb` file, even when KiCad held
that exact board open live with unsaved moves or edits. A hard courtyard-overlap
introduced by a live `move_component` could be entirely absent from the response
with no error and no indication the answer was stale (#595).

The tool now prefers the exact board's live IPC snapshot (via `SaveDocumentToString`,
the same read-only mechanism `run_drc`'s `sync_live_board` uses) when KiCad has it
open, falling back to the saved file otherwise. Scoring policy and every existing
field are unchanged; the response adds `source`, either `"ipc"` or `"saved_file"`
(matching `run_drc`'s own source-disclosure convention), so a caller can tell
which board state was judged.
