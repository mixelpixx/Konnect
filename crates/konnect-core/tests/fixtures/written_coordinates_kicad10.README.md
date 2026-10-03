# Written-coordinates fixture

`written_coordinates_kicad10.kicad_sch` holds points whose coordinates come out
of `f64` sums carrying noise, so the tests can check that a tool writes them the
way KiCad does (#766).

## Provenance

Built through Konnect against KiCad's stock `Device` library with
`create_schematic`, `add_schematic_component` and `add_schematic_net_label`.
The two wires were then added as text, each ending 0.02 mm short of a pin, since
`add_wire` snaps to the grid and would close the gap. The file was then
force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force written_coordinates_kicad10.kicad_sch
```

What is committed is that Eeschema serialization.

## Cases

| Item | Where | The sum | `f64` result | KiCad writes |
|---|---|---|---|---|
| `R1` pin 1 | `R1` at `(139.7, 101.6)`, rotation 0 | `101.6 - 3.81` | `97.78999999999999` | `97.79` |
| `R2` pin 2 | `R2` at `(120.65, 96.52)`, rotation 90 | `120.65 + 3.81` | `124.46000000000001` | `124.46` |
| Wire from label `A` | `(139.7, 88.9)` to `(139.7, 97.81)` | snap onto `R1` pin 1 | | |
| Wire from label `B` | `(132.08, 96.52)` to `(124.48, 96.52)` | snap onto `R2` pin 2 | | |
| Label `SIG` | `(139.7, 167.64)` | moved by `(2.54, -5.08)` | `(142.23999999999998, 162.55999999999997)` | `(142.24, 162.56)` |

## KiCad's own answer

Netlists from `kicad-cli sch export netlist`:

| Net | As committed | After `fix_connectivity` |
|---|---|---|
| `R1` pin 1 | `unconnected-(R1-Pad1)` | `/A` |
| `R2` pin 2 | `unconnected-(R2-Pad2)` | `/B` |
| `R1` pin 2, `R2` pin 1 | unconnected | unconnected |

The "KiCad writes" column is what `kicad-cli sch upgrade --force` writes for the
same items after `fix_connectivity` and `move_labels_by_offset` run. Before the
fix, a KiCad resave changed exactly those three lines. After it, the resave
leaves the file byte-identical.
