# Single-pin net fixture

`single_pin_nets.kicad_sch` and its child `single_pin_nets_child.kicad_sch` are
a parent/child pair carrying one net of every shape `find_single_pin_nets` has
to tell apart, including the two that cross the hierarchy boundary.

## Provenance

The pair was built through Konnect against KiCad's stock `Device:R` and
`power:` libraries — `batch_place_components`, `batch_add_wire`,
`add_schematic_net_label`, `add_power_symbol`, `add_hierarchical_sheet`,
`import_sheet_pins` — and then parsed and force-resaved by KiCad 10.0.5:

```text
kicad-cli sch upgrade --force single_pin_nets.kicad_sch
kicad-cli sch upgrade --force single_pin_nets_child.kicad_sch
```

What is committed here is that Eeschema serialization: `(generator
"eeschema")`, `(version 20260306)`, KiCad's own library records, field
positions and UUIDs. The predecessor of this fixture was hand-authored, with
placeholder UUIDs and abbreviated symbol definitions, and KiCad refused to open
it — a parser accepting its own approximation of the format proves nothing.

The `#FLG01` reference is the one deliberate edit made after placement:
`add_power_symbol` numbers every power symbol in the `#PWR` sequence, and
eeschema annotates `PWR_FLAG` as `#FLG`. The fixture carries what KiCad writes,
because `#FLG…` being excluded alongside `#PWR…` is exactly what one of the
tests asserts.

## KiCad's own answer

The expected pin count of every net is KiCad's, not this crate's. Reload-checked
with the netlist exporter, which is also the oracle the tests assert against:

```text
kicad-cli sch export netlist --output single_pin_nets.net single_pin_nets.kicad_sch
```

| Net | Pins KiCad resolves | Shape under test |
|---|---|---|
| `/TWO_PIN` | 2 — R1.2, R2.1 | one label, two pins: the false positive |
| `/LONE` | 1 — R3.2 | two labels on two disconnected roots, one pin |
| `/SPLIT` | 2 — R4.2, R5.1 | the same two-root shape with a pin on each |
| `VCC` | 1 — R6.2 | a rail reaching one pin; `#PWR001` is not a pin |
| `/FLAGGED` | 1 — R7.2 | `#FLG01` is not a pin either |
| `/SHEET_NET` | 2 — R8.2, R10.2 | across the sheet boundary |
| `MIXED` | 1 — R9.2 | one net named by a local *and* a global label |
| `GND` | 9 | the rail that is not a defect |
| `/Child/CHILD_LOCAL` | 2 — R11.2, R12.1 | a local net answered by the child alone |
| `STUB` | — absent | reaches no pin, so KiCad emits no net at all |

`/SPLIT` is the load-bearing row. KiCad resolves two same-named labels on wire
segments that never touch into one 2-pin net, which is why the tool pools the
roots a name sits on rather than counting each separately.

`/SHEET_NET` is the limitation the response documents. KiCad sees two pins;
neither sheet alone can. The parent counts R8.2 and the sheet pin, the child
counts R10.2 and reports `cross_sheet_unverified`.

`STUB` is the case no netlist can corroborate, which is the point of the tool:
a label whose net reaches nothing leaves no net for KiCad to export.

## ERC

`kicad-cli sch erc` independently agrees with the fixture's intent — three
errors and eight warnings, none of them structural:

- `isolated_pin_label` on both `LONE` labels and on both `MIXED` labels — the
  nets this tool must report;
- `label_dangling` on `STUB`;
- no violation for `SPLIT`, `TWO_PIN`, `SHEET_NET` or `GND`;
- `power_pin_not_driven` on `VCC` and the child `GND`, and
  `unconnected_wire_endpoint` on the deliberately floating stubs. Those are the
  cost of a sheet built to hold defects, not defects in the fixture.
