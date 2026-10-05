# Symbol fields fixture

`symbol_fields_kicad10.net` is a netlist of the kind `update_pcb_from_schematic`
reads, exported by KiCad 10.0.6. Both components carry custom `LCSC` and `MPN`
fields, so the sync can be checked for copying symbol fields onto footprints
(#788).

## Provenance

The schematic was built through Konnect against KiCad's stock `Device` library
(`create_project`, `add_schematic_component`, `edit_schematic_component`). It
was then parsed and force-resaved by KiCad 10.0.6, and the netlist exported:

```text
kicad-cli sch upgrade --force symbol_fields_kicad10.kicad_sch
kicad-cli sch export netlist --format kicadsexpr -o symbol_fields_kicad10.net symbol_fields_kicad10.kicad_sch
```

The schematic is not committed: nothing reads it, and the cases below rebuild it.

Three sections of the export are removed: `(design …)`, which names the exporting
machine's path and the export time, `(libparts …)` and `(libraries …)`. What
remains is KiCad's output unchanged: `(version …)`, `(components …)`,
`(groups)`, `(variants)` and `(nets …)`.

The field values are placeholders built from the issue number, shaped like an
LCSC ID and an MPN. They are not meant to identify a part.

## Cases

| Component | Footprint | `LCSC` | `MPN` | Why |
|---|---|---|---|---|
| `C1` `Device:C`, value `100n` | `Capacitor_SMD:C_0603_1608Metric` | `C788001` | `MPN-788-C1` | the issue's `LCSC` field, and a second custom field |
| `R1` `Device:R`, value `10k` | `Resistor_SMD:R_0603_1608Metric` | `C788002` | empty | KiCad exports an empty field as `(field (name "MPN"))`, and Update PCB copies it all the same |

Both symbols leave Datasheet empty, which KiCad also writes as a field with no
value, and both carry their library symbol's Description.
