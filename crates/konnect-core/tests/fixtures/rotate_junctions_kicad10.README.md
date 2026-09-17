# Rotation junction fixture

`rotate_junctions_kicad10.kicad_sch` carries the two directions a turn can
change junction state, so `rotate_schematic_component` can be asked to
reconcile them and checked against an answer that is not its own (#615).

## Provenance

Built through Konnect against KiCad's stock `Device` library —
`create_schematic`, `add_wire`, `add_schematic_component`,
`move_schematic_component`, `add_schematic_net_label` — then parsed and
force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force rotate_junctions_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`, KiCad's own library
records, field positions and UUIDs.

The dot at `(127, 97.79)` is not hand-placed. `R1` was added off to the side at
`(160.02, 101.6)` and then *moved* onto the wire, and the move path added the
dot itself, reporting `junctions_added_count: 1` — it is the reconciliation the
turn used to skip, left in the file by the tool that does it.

The dot at `(190.5, 88.9)` is KiCad-style too: `add_wire` inserts one when a
branch lands mid-span on an existing wire.

## Cases

`Device:R` puts its pins at `y ∓ 3.81` when it stands at 0°, and at `x ∓ 3.81`
once it is turned to 90°, pin 1 first.

| Turn | What moves | Expected |
|---|---|---|
| `R1` → 90° | pin 1 leaves `(127, 97.79)`, mid-span on the NETA wire | the dot there is pruned |
| `R2` → 90° | pin 1 arrives at `(154.94, 160.02)`, mid-span on the NETB wire | a dot is added |
| `R0` → 90° | pin 1 leaves `(114.3, 97.79)`, the NETA wire's **end** | nothing added, nothing pruned |
| `R4` | untouched by the turns above | anchors NETB so the net survives in the netlist |
| the `(190.5, 88.9)` wire T | untouched by every turn above | its dot stays |

`R0` and `R4` sit on wire *ends*, which need no dot — they are there so each net
keeps a pin after the turn, and so a pin leaving an end is distinguishable from
a pin leaving an interior.

## KiCad's own answer

The pins reached by each net are KiCad's, not this crate's. Each row is
`kicad-cli sch export netlist` run on the file as it stands after the named
turn, with `no fix` meaning the same turn with the reconciliation bypassed —
the behavior this fixture exists to rule out:

```text
kicad-cli sch export netlist --output rotate.net rotate_junctions_kicad10.kicad_sch
```

| File | `/NETA` | `/NETB` |
|---|---|---|
| the fixture | R0.1, R1.1 | R4.1 |
| `R1` → 90° | R0.1 | R4.1 |
| `R1` → 90°, no fix | R0.1 | R4.1 |
| `R2` → 90° | R0.1, R1.1 | **R2.1, R4.1** |
| `R2` → 90°, no fix | R0.1, R1.1 | R4.1 |

The load-bearing row is the last pair. KiCad puts the turned pin on `/NETB`
only when the dot is there; without it the same pin, at the same coordinate on
the same wire, is reported as `unconnected-(R2-Pad1)`. That is the half of
#615 that changes the design rather than the picture: a wrong implementation
cannot produce both rows.

The `R1` rows are the converse and agree with each other — a stranded dot costs
no connectivity, which is why it needs a fixture to catch it at all.

## ERC

`kicad-cli sch erc --severity-all` finds 12 violations on the fixture, all of
them the cost of a sheet built out of deliberate stubs rather than defects in
it: 5 `pin_not_connected`, 5 `unconnected_wire_endpoint`, 1 `label_dangling`
(NETC labels a wire T with no pin on it), and 1 `isolated_pin_label` on NETB.

ERC is also where the asymmetry shows:

| File | violations |
|---|---|
| `R1` → 90° | 14 |
| `R1` → 90°, no fix | 14 — the stranded dot is invisible to ERC |
| `R2` → 90° | 10 |
| `R2` → 90°, no fix | 12 — R2 pin 1 stays `pin_not_connected`, NETB stays `isolated_pin_label` |
