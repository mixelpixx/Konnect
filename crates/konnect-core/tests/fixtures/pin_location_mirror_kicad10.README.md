# Pin-location mirror fixture

`pin_location_mirror_kicad10.kicad_sch` places two LM358s whose units carry
different mirrors, so `get_schematic_pin_locations` and
`batch_get_schematic_pin_locations` can be checked to report each placement's
own `mirror_x`/`mirror_y` beside its pins (#855).

## Provenance

Built through Konnect against KiCad's stock `Amplifier_Operational` library:
`create_schematic`, then four `add_schematic_component` calls with `reference`,
`unit` and `mirror` (#485). Then parsed and force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force pin_location_mirror_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`. KiCad kept every placement
and mirror as Konnect wrote it.

## Cases

| Ref | Unit | Body | Mirror | Role |
|---|---|---|---|---|
| `U1` | 1 | (101.6, 76.2) 0° | — | a plain anchor over a mirrored unit |
| `U1` | 2 | (139.7, 76.2) 0° | `y` | the unit's own flag, not the anchor's |
| `U2` | 1 | (101.6, 127) 0° | `x` | a mirrored anchor over a plain unit |
| `U2` | 2 | (139.7, 127) 0° | — | the unit's own flag, not the anchor's |

A tool that reports the anchor's mirror on every unit, or none at all, fails
on one unit of each.

## Oracle

The mirrors are the `(mirror …)` tokens eeschema wrote. The pin positions are
KiCad's own: `kicad-cli sch erc --severity-all` names every unconnected pin at
its position.

| Ref | Pin | KiCad's position | Ref | Pin | KiCad's position |
|---|---|---|---|---|---|
| `U1` | 1 | (109.22, 76.20) | `U2` | 1 | (109.22, 127.00) |
| `U1` | 2 | (93.98, 78.74) | `U2` | 2 | (93.98, 124.46) |
| `U1` | 3 | (93.98, 73.66) | `U2` | 3 | (93.98, 129.54) |
| `U1` | 5 | (147.32, 73.66) | `U2` | 5 | (132.08, 124.46) |
| `U1` | 6 | (147.32, 78.74) | `U2` | 6 | (132.08, 129.54) |
| `U1` | 7 | (132.08, 76.20) | `U2` | 7 | (147.32, 127.00) |

U1's mirrored unit 2 has its output on the left of the body, where U2's plain
unit 2 has it on the right. U2's mirrored unit 1 has its inputs swapped
vertically against U1's plain unit 1.

## ERC

`kicad-cli sch erc --severity-all` reports 24 violations: 12
`pin_not_connected`, 8 `pin_not_driven`, and one `missing_power_pin` and one
`missing_unit` per LM358 for the unplaced power unit C. Nothing is wired, so
this is the expected state.
