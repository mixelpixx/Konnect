# Label move fixture

`label_move_kicad10.kicad_sch` carries the two labels `move_labels_by_offset`
used to get wrong (#803): a plain label whose name reads `(at 1 2)`, and a
global label `PWR` with the `Intersheetrefs` field KiCad adds to every global
label.

## Provenance

Built through Konnect against KiCad's stock `Device` library —
`create_schematic`, `add_schematic_component`, `add_wire`,
`add_schematic_net_label` — then force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force label_move_kicad10.kicad_sch
```

KiCad wrote the `Intersheetrefs` property on top of `PWR`'s anchor, at
(114.3, 105.41). A field on its anchor cannot tell "move the field by the
offset" from "put the field on the new anchor". So that one `(at …)` was
edited to (124.46, 101.6), a position a drag in Eeschema leaves, and the sheet
was resaved again. KiCad kept that position and changed no other byte. What is committed
is that second Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`.

## Cases

| Item | Position | Role |
|---|---|---|
| `R1` | `Device:R` at (101.6, 101.6) | pin 1 at (101.6, 97.79), pin 2 at (101.6, 105.41) |
| wire | (101.6, 97.79) → (127, 97.79) | carries the `(at 1 2)` label |
| wire | (101.6, 105.41) → (127, 105.41) | carries `PWR` |
| `label "(at 1 2)"` | (114.3, 97.79) | a name that contains `(at ` |
| `global_label "PWR"` | (114.3, 105.41) | anchor, with its `Intersheetrefs` field away from it at (124.46, 101.6) |

## Oracle

`label_move_kicad10.moved.kicad_sch` is the sheet after
`move_labels_by_offset` moved each label 2.54mm along its wire (`dx = 2.54`,
`dy = 0`), as KiCad resaved it. The resave changed no byte, and
`kicad-cli sch export netlist` gives:

| | net on `R1.1` | net on `R1.2` |
|---|---|---|
| as committed | `/(at 1 2)` | `PWR` |
| moved, before the fix | `/(at 116.84 97.79 0)` | `PWR` |
| moved, after the fix | `/(at 1 2)` | `PWR` |

Before the fix, the label named `(at 1 2)` stayed at (114.3, 97.79) and its
name became `(at 116.84 97.79 0)`. `PWR`'s anchor moved and its
`Intersheetrefs` field stayed at (124.46, 101.6). The `.moved` file has both
labels at x = 116.84 and the field at (127, 101.6), the same 2.54mm from where
it was.

## ERC

`kicad-cli sch erc --severity-all` reports 4 warnings on both files: 2
`isolated_pin_label` (each label reaches only one pin) and 2
`unconnected_wire_endpoint` (the wires' free ends). The sheet exists to be
moved, not wired, so that is expected.
