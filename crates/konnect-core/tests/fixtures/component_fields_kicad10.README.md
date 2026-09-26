# Component properties fixture

`component_fields_kicad10.kicad_sch` carries custom `LCSC` and `MPN` properties
on a single-unit and a multi-unit component, so `get_schematic_component` and
`list_schematic_components` can be checked for returning them (#679).

## Provenance

Built through Konnect against KiCad's stock `Device` and
`Amplifier_Operational` libraries — `create_project`,
`add_schematic_component`, `edit_schematic_component` — then parsed and
force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force component_fields_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`, KiCad's own library
records, field positions and UUIDs. The property values are placeholders built
from the issue number, shaped like an LCSC ID and an MPN; they are not meant to
identify a part.

## Cases

| Component | Units | `LCSC` | `MPN` | Why |
|---|---|---|---|---|
| `R1` `Device:R` | 1 | `C679001` | `MPN-679-R1` | the filed repro |
| `U1` `Amplifier_Operational:LM358` | 1, 2, 3 | `C679002` | `MPN-679-U1` | every unit carries its own copy |

KiCad's save orders the symbols by UUID, which puts `U1` unit 2 first in the
file and unit 1 last. The component is still unit 1.

## KiCad's own answer

```text
kicad-cli sch export netlist --output component_fields.net component_fields_kicad10.kicad_sch
```

| `comp` | `LCSC` | `MPN` |
|---|---|---|
| `R1` | `C679001` | `MPN-679-R1` |
| `U1` | `C679002` | `MPN-679-U1` |

The values are KiCad's reading of the file, not this crate's. The netlist lists
`U1` once; the three per-unit copies are in the file itself.

## ERC

`kicad-cli sch erc --severity-all` finds 16 violations, all of them the cost of
parts placed with nothing wired: 10 `pin_not_connected`, 4 `pin_not_driven`
and 2 `power_pin_not_driven`.
