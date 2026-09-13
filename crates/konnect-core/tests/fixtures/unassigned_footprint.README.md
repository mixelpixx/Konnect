# `unassigned_footprint.net`

A real `kicad-cli sch export netlist --format kicadsexpr` export (Eeschema
10.0.0-rc2, 2026-09-11) of a one-sheet schematic with three components:

| Reference | Symbol | Footprint field | `(footprint …)` node in the export |
|---|---|---|---|
| `C1` | `Device:C` | `Capacitor_SMD:C_0603_1608Metric` | present |
| `R1` | `Device:R` | *(empty — never assigned)* | **absent** |
| `R2` | `Device:R` | `Resistor_SMD:R_0603_1608Metric` | present |

`R1` pin 1 is wired to `C1` pin 1, so the unassigned component also appears
in the `nets` section (`Net-(C1-Pad1)`), which is the case #507 is about: a
sync must keep the other components and report `R1`, not fail on it.

Recipe: `create_project`, three `add_schematic_component` calls,
`edit_schematic_component … footprint=` for `C1` and `R2` only, one
`add_wire`, then the kicad-cli export above. The only edit to the export is
the `(source …)` path, replaced by the bare file name. Line endings are the
export's own (CRLF); the directory is `-text` in `.gitattributes`.
