# Netclass differential-pair settings, explicit and inherited

A project whose netclasses carry the differential-pair settings three ways —
complete on the Default, all three set on `USB`, none set on `Power` — in the
bytes KiCad wrote. Used to check that `get_netclasses` reports them as KiCad
resolves them and that `create_netclass` changes only what it is asked to
(#777).

## Provenance

1. Konnect's `create_project` laid out `netclass_diff_pair_kicad10`, and its
   schematic was removed. pcbnew 10.0.6, with the API server enabled, opened
   the board, and Konnect's `save_project` had it save through the API's
   `SaveDocument`. That save writes the board and, beside it, the project file
   (`pcbnew/api/api_handler_pcb.cpp:185-198`, `pcbnew/files.cpp:376-382`,
   `:999-1002`), so both files are KiCad's.
2. With pcbnew closed, `create_netclass` and `assign_net_to_class` from this
   change (#777) wrote:

   ```text
   create_netclass      name=USB   diff_pair_width=0.18 diff_pair_gap=0.15 diff_pair_via_gap=0.3
   create_netclass      name=Power clearance=0.3
   assign_net_to_class  net_name=USB_DP netclass=USB
   ```

3. pcbnew 10.0.6 opened the board again and saved it the same way. The
   committed files are that output; nothing was edited by hand.

Run on Linux, pcbnew under Xvfb.

## What they show

KiCad rebuilt the classes from its own model rather than passing them through:
it wrote the Default first and the others by name
(`common/project/net_settings.cpp:189-201`, a `std::map` at
`include/project/net_settings.h:236`), and it added `pcb_color`,
`schematic_color` and `tuning_profile` to `USB` and `Power`, which it always
writes (`:76-80`). Every value Konnect wrote came back
unchanged, the Default and `netclass_patterns` included. A differential-pair
key is written only when the class holds one (`:116-123`):

| Class | `diff_pair_width` | `diff_pair_gap` | `diff_pair_via_gap` |
|---|---|---|---|
| `Default` | 0.2 | 0.25 | 0.25 |
| `USB` | 0.18 | 0.15 | 0.3 |
| `Power` | — | — | — |

So KiCad held all three of `USB`'s values, a named class's via gap included,
and invented none for `Power`. When it resolves `Power`, KiCad takes the width
and gap from the Default and no via gap, whose fallback is commented out
(`:1108-1114`).

The classes are KiCad's in their serialization; their values were set by
`create_netclass`. Board Setup was not opened. Its grid has width and gap
columns and none for a via gap (`common/dialogs/panel_setup_netclasses.cpp:52-75`),
so it could author `Power` and `USB`'s width and gap, but not `USB`'s via gap.

A second open and save left the project file byte-identical and its timestamp
unchanged (`common/settings/json_settings.cpp:520-547`), and each save wrote the
board with the same bytes. The board has no nets, so the `USB_DP` pattern fits
none; `schematic.top_level_sheets` names a schematic that is not part of the
fixture, and no test reads it.

| File | SHA-256 |
|---|---|
| `netclass_diff_pair_kicad10.kicad_pro` | `df212b2dda4689fd488b67bd65c504cc074e8504e2c92d4231c94e3353335b9d` |
| `netclass_diff_pair_kicad10.kicad_pcb` | `2128152b386b0d7807bccbee5cb2f0272e74509fac06381410389483d3ee1584` |

KiCad source references are to tag 10.0.6.

## Tests using this fixture

`netclass_tests` in `crates/konnect-core/src/tools/pcb_routing.rs`:
`a_kicad_written_project_reads_explicit_and_inherited_differential_pair_settings`
and `updating_a_kicad_written_project_changes_only_the_value_named`.

## Regenerating

Repeat the three steps with KiCad 10.0.6, removing the `.kicad_prl`, the
`.history` directory and any `~*.lck` KiCad leaves beside the files, then check
`jq '.net_settings.classes'` against the table above.
