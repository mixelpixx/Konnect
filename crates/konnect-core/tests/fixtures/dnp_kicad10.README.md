# Native DNP fixture

`dnp_kicad10.kicad_sch` carries KiCad's native do-not-populate attribute,
`(dnp yes|no)`, on a single-unit and a multi-unit component, a KiCad 10
design-variant override beside it, and the custom `DNP` property that #415
reports in place of the attribute. `edit_schematic_component`,
`batch_edit_schematic_components`, `get_schematic_component` and
`list_schematic_components` are checked against KiCad's own reading of all
three.

## Provenance

1. Built through Konnect against KiCad's stock `Device` and
   `Amplifier_Operational` libraries (`create_project`,
   `add_schematic_component`).
2. Two edits by hand: `C1`'s `(dnp no)` became `(dnp yes)`, and a `Lite`
   variant clause carrying `(dnp yes)` was added to `R1`'s instance path.
3. Parsed and force-resaved by KiCad 10.0.6:

   ```text
   kicad-cli sch upgrade --force dnp_kicad10.kicad_sch
   ```

4. `R1` was given a custom property `DNP` = `yes` the way #415 reports it:
   `edit_schematic_component` with `fields: {"DNP": "yes"}` on Konnect
   0.13.0, before this change. Then force-resaved by KiCad 10.0.6 again.

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`. KiCad kept every edit,
wrote the variant clause in its own multi-line form, and gave the property
its own `(show_name no)` and `(do_not_autoplace no)` (Konnect 0.13.0 wrote
only `(hide yes)`). Repeating step 4 on the step-3 file reproduces the
committed bytes exactly.

## Cases

| Component | Units | Native `dnp` | Custom `DNP` property | Variant `Lite` | Why |
|---|---|---|---|---|---|
| `R1` `Device:R` | 1 | `no` | `yes` | `(dnp yes)` | single unit; the inert property and a per-variant override, neither of them the attribute |
| `C1` `Device:C` | 1 | `yes` | — | — | KiCad-saved DNP, readable before anything is written |
| `U1` `Amplifier_Operational:LM358` | 1, 2, 3 | `no` on each | — | — | every placed unit carries its own token |

KiCad writes the token after `(in_pos_files …)` and before `(uuid …)`.

## KiCad's own answer

`kicad-cli sch export bom` on the fixture as committed, with the default fields
`Reference,Value,Footprint,QUANTITY,DNP`. The bracket is each row's `DNP`
column:

| Options | Rows |
|---|---|
| default | `C1` [DNP], `R1` [yes], `U1` [] |
| `--exclude-dnp` | `R1` [yes], `U1` [] |
| `--variant Lite` | `C1` [DNP], `R1` [yes], `U1` [] |
| `--variant Lite --exclude-dnp` | `U1` [] |

**The custom property is worse than inert.** It fills `R1`'s `DNP` column with
`yes`, with or without the variant, so the BOM reads as if `R1` were
unpopulated, while `--exclude-dnp` keeps `R1`: it is still fitted (only the
`Lite` variant's own override drops it). Only the attribute decides exclusion,
so the checks below read exclusion, not that column.

KiCad's plot agrees with exclusion. `kicad-cli sch export svg` draws a symbol
whose attribute is `(dnp yes)` greyed (`#9B9B98` where a fitted part's body is
`#840000`) with a red cross over it. `C1` is drawn that way; `R1`, with only
the property, is drawn as a fitted part until its attribute is set.

A package whose units disagree gets two answers. With one `U1` unit set to
`(dnp yes)` (unit 1 or unit 2), the default BOM lists `U1` once, marked `DNP`,
while `--exclude-dnp` still lists it as fitted. That is why the tools write
every placed unit.

## After the tools

The built server was driven over stdio on a scratch copy, with the BOM
exported by KiCad after each step:

| Step | `--exclude-dnp` keeps |
|---|---|
| fixture | `R1`, `U1` |
| `edit_schematic_component` `U1` `dnp: true` | `R1` |
| `batch_edit_schematic_components` `R1` `true`, `C1` `false`, `U1` `false` | `C1`, `U1` |
| `edit_schematic_component` `R1` `false`, then `C1` `true` | the fixture, byte for byte |

After the first step, `kicad-cli sch upgrade --force` (run on a copy beside
the project file) leaves the file unchanged. After the second it removes one
thing: `R1`'s `Lite` clause. KiCad 10.0.6 writes a variant's `dnp` only when it
differs from the symbol's own, and `R1` is now DNP itself. The tools leave
variant clauses alone, and the four BOM exports (default and `Lite`, each with
and without `--exclude-dnp`) are identical before and after that re-save.

A sheet from before KiCad 7 has no `(dnp …)`. On a KiCad 6 sheet (format
`20211123`; not committed), setting `dnp: true` on a capacitor wrote
`(in_bom yes) (on_board yes) (dnp yes) (fields_autoplaced)`. The BOM then
listed the part as DNP, and `--exclude-dnp` dropped it. KiCad 10.0.6 then
re-saved the sheet and kept the attribute, after `(on_board yes)`.
