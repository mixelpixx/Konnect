# Route obstacles fixture

`route_obstacles_kicad10.kicad_sch` and its child sheet
`route_obstacles_sub_kicad10.kicad_sch` put another net's pin or sheet pin on the
L-bend that `connect_pins`, `batch_connect_pins` and `add_schematic_connection`
draw between two pins (#887). They also hold a T with no junction, which every
route used to dot wherever it was on the sheet.

## Provenance

Built through Konnect (`main` @ `9488e5f`) against KiCad's stock `Device`
library with `create_schematic`, `add_schematic_component`,
`add_schematic_net_label`, `add_hierarchical_sheet` and `add_sheet_pin`. The
three wires of the `tee` group were then appended as text, because every
Konnect wire tool dots a T. Then both files were force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force route_obstacles_kicad10.kicad_sch
kicad-cli sch upgrade --force route_obstacles_sub_kicad10.kicad_sch
```

Both files are that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`. Every part is `Device:R` at rotation 0, so pin 1 is at
`y − 3.81` and pin 2 at `y + 3.81`.

| Group | Parts | Route | In the way |
|---|---|---|---|
| around | R1 (101.6, 101.6), R2 (127, 111.76), R3 (114.3, 109.22) | R1.2 → R2.1 | R3.1 at (114.3, 105.41), on the horizontal-first leg. The issue's reproduction. |
| blocked | R11 (101.6, 50.8), R12 (127, 60.96), R13 (114.3, 58.42), R14 (120.65, 60.96); label `OTHER` on R13.1 | R11.2 → R12.1 | R13.1 (net `OTHER`) on the horizontal-first leg, R14.1 on the vertical-first leg |
| clear | R21 (101.6, 152.4), R22 (127, 162.56) | R21.2 → R22.1 | nothing |
| sheet | R41 (177.8, 101.6), R42 (203.2, 111.76); sheet `SUB` at (203.2, 96.52), 19.05 × 10.16, input pin `IN` at (203.2, 105.41) | R41.2 → R42.1 | sheet pin `IN` at the horizontal-first corner |
| tee | R51 (177.8, 152.4), R52 (203.2, 162.56), R53 (190.5, 139.7); wires (177.8, 156.21)→(203.2, 156.21)→(203.2, 158.75) and (190.5, 143.51)→(190.5, 156.21), no junction | none | — |
| tap | R54 (184.15, 139.7) | `add_schematic_connection` (184.15, 143.51) → (184.15, 156.21), mid-span on R51's wire | — |

`kicad-cli sch export netlist` on the fixture names `/OTHER` (R13.1) and
`Net-(R51-Pad2)` (R51.2 R52.1). R53.2 is not on that net: KiCad does not join a
wire end that lands mid-span without a junction. Every other pin is
`unconnected-…`. `kicad-cli sch erc --severity-all` reports 28
`pin_not_connected`, 1 `isolated_pin_label` (`OTHER`), 1 `label_dangling` (the
child's `IN`) and 1 `unconnected_wire_endpoint` (R53's wire at the T).

## The oracle: KiCad's netlist of each result

Each tool ran on a copy of the fixture, driven over stdio. `batch_connect_pins`
took the four routes in the order above, and `connect_pins` took them one call
each, both followed by the `tap` call. Both give the same wires and the same
netlist. Pins left unconnected are omitted.

| Group | `main` @ `9488e5f` | This change |
|---|---|---|
| around | `Net-(R1-Pad2)` R1.2 R2.1 **R3.1**; junction at (114.3, 105.41) | `Net-(R1-Pad2)` R1.2 R2.1; wires (101.6, 105.41)→(101.6, 107.95)→(127, 107.95), no junction |
| blocked | `/OTHER` **R11.2 R12.1** R13.1; junction at (114.3, 54.61) | refused, nothing written; `/OTHER` R13.1 |
| clear | `Net-(R21-Pad2)` R21.2 R22.1 | the same wires, (101.6, 156.21)→(127, 156.21)→(127, 158.75) |
| sheet | `/SUB/IN` **R41.2 R42.1** | `Net-(R41-Pad2)` R41.2 R42.1; wires (177.8, 105.41)→(177.8, 107.95)→(203.2, 107.95) |
| tee, tap | `Net-(R51-Pad2)` R51.2 R52.1 **R53.2** R54.2; junctions at (190.5, 156.21) and (184.15, 156.21) | `Net-(R51-Pad2)` R51.2 R52.1 R54.2; one junction, at (184.15, 156.21) |

The bold pins are the ones `main` joined without being asked. The batch reported
`{"connected_count":4,"errors":[]}` on `main`, and on this change
`connected_count` 3 with one error:

```text
R11.2 -> R12.1: the horizontal-first route would join R13 pin 1 and label OTHER at (114.3, 54.61) on net OTHER; the vertical-first route would join R14 pin 1 at (120.65, 57.15)
```

`connect_pins` refuses that pair with `plan_blocked`, one reason per bend.
