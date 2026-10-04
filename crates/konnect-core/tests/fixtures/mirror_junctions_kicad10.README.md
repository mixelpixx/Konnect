# Mirror junction fixture

`mirror_junctions_kicad10.kicad_sch` puts a wire where a reflected pin arrives,
so `mirror_schematic_component` can be asked to reconcile the junction there
and checked against an answer that is not its own (#450).

## Provenance

`mirror_fields_kicad10.kicad_sch`, plus a wire and a net label added through
Konnect (`add_wire` from (147.32, 49.53) to (147.32, 52.07), and
`add_schematic_net_label` `NETM` at (147.32, 52.07)), then parsed and
force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force mirror_junctions_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`.

## Case

`U1` (`Regulator_Linear:AP2112K-3.3`, at (139.7, 50.8), 0°, unmirrored) has
pin 3 (`EN`) at (132.08, 50.8). Reflected with `(mirror y)`, pin 3 arrives at
(147.32, 50.8), mid-span on the `NETM` wire, a point no pin occupied before.
Nothing else on the wire moves: pin 1 arrives at (147.32, 48.26), off the
wire's end at 49.53.

| Change | Expected |
|---|---|
| `U1` → `mirror y` | one dot added at (147.32, 50.8) |

## KiCad's own answer

`kicad-cli sch export netlist` on the file after the reflection, and on the
same file with the added dot deleted (the reflection without reconciliation):

| File | `U1` pin 3 |
|---|---|
| the fixture | not on `/NETM`, which has no pin and so is not listed |
| reflected through the tool | `/NETM` |
| reflected, dot deleted | `unconnected-(U1-EN-Pad3)` |
