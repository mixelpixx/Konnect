# Two-name net fixture

`two_name_nets.kicad_sch` carries one net of every shape that separates a net's
*identity* from its *name*: rails split across stubs and joined only by a name,
nets carrying two or three names at once, and an alias sitting on a power
symbol's own pin.

## Provenance

Built through Konnect against KiCad's stock `Device:`, `Memory_EEPROM:`,
`Connector:` and `power:` libraries — `batch_place_components`,
`batch_add_wire`, `add_schematic_net_label`, `add_power_symbol`,
`batch_add_no_connect` — and then parsed and force-resaved by KiCad 10.0.5:

```text
kicad-cli sch upgrade --force two_name_nets.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, KiCad's own library records, field positions and UUIDs.

One shape had to change to match what KiCad actually honours. Two label objects
placed at the *exact same coordinate* are not both kept: with `AAA` and `ZZZ`
stacked at `(55.88, 119.38)`, KiCad left `TP2`'s pin unconnected and netted only
`TP3`. Moving the second label 2.54 mm along the same wire nets both. The
co-located case that KiCad *does* honour is a label on a power symbol's own pin,
which is what `ALT` at `(59.69, 88.9)` is — and it is the one that matters here,
because Konnect synthesises a power symbol's name as a pseudo-label at exactly
that point.

## KiCad's own answer

Every expectation below is KiCad's, not this crate's:

```text
kicad-cli sch export netlist --output two_name_nets.net two_name_nets.kicad_sch
kicad-cli sch erc --severity-all --format json -o erc.json two_name_nets.kicad_sch
```

| Net | Pins KiCad resolves | Shape under test |
|---|---|---|
| `+3V3` | 5 — U1.8, C1.1, C2.1, TP1.1, TP7.1 | one rail over three stubs: the IC's, the capacitors' + test point, and `TP7` joined only by the `ALT` alias on a power symbol's pin |
| `RETURN` | 4 — U1.4, C1.2, C2.2, C3.2 | ground named by a global label that outranks its `GND` symbols |
| `SYS` | 4 — R1.1, R2.1, C3.1, TP6.1 | the pull-up rail, named by a global label over both a `+5V` symbol and a `PULLUP` local label |
| `/SDA` | 2 — U1.5, R1.2 | pin and pull-up on separate segments, joined by the name |
| `/SCL` | 2 — U1.6, R2.2 | the same |
| `/AAA` | 2 — TP2.1, TP3.1 | two locals on one segment plus a disconnected `ZZZ` segment, merged transitively |
| `/MIX` | 1 — TP4.1 | a local label beside a hierarchical one |
| `Net-(TP8-Pad1)` | 2 — TP8.1, TP9.1 | a net with no label at all, which KiCad names after a pin |

ERC states the precedence outcomes directly, and they are the table Konnect's
`driver_priority` encodes — global label > power symbol > local label >
hierarchical label, ties on the name ascending:

```text
Both +3V3 and ALT are attached to the same items; +3V3 will be used in the netlist
Both RETURN and GND are attached to the same items; RETURN will be used in the netlist
Both SYS and PULLUP are attached to the same items; SYS will be used in the netlist
Both AAA and ZZZ are attached to the same items; AAA will be used in the netlist
Both MIX and MIX_H are attached to the same items; MIX will be used in the netlist
```

The `SYS` net carries three names — the `SYS` global label, the `+5V` power
symbol and the `PULLUP` local label — and KiCad names it `SYS`, so global
outranks both of the others on one net.

## What each shape is for

- **Decoupling across stubs.** `U1`'s `VCC` pin and the capacitors `C1`
  (100 nF) and `C2` (10 µF) share no wire; they share the `+3V3` rail. An audit
  comparing wire-connected roots alone reports this correctly decoupled IC as
  undecoupled.
- **One rail, not three.** Two `+3V3` power symbols and the `VCC`/`ALT` labels
  name one net. A rail keyed by name, or by unmerged root, is reported two or
  three times, and every capacitor lands under one of them.
- **Ground under another name.** `RETURN` wins the name, so a ground skip
  reading only the winning name reports ground as an undecoupled power rail.
- **A pull-up hidden by a global.** `SYS` wins the name over `+5V`, so a
  classifier reading only the winning name does not see a rail there and reports
  `U1`'s `SDA`/`SCL` pins as missing their pull-ups.
- **An alias on a power symbol's pin.** `ALT` names the same point as the second
  `+3V3` symbol. A graph that keeps one name per point drops it and `TP7` never
  joins the rail.
- **Deterministic naming.** Five nets carry more than one name; each has exactly
  one answer, and KiCad's ERC above says which.
- **A net with no name.** `TP8`–`TP9` carry no label, so a reader collecting
  nets by name has nothing to collect and drops the wire and both pins.

The three `A0`/`A1`/`A2` pins and `WP` carry no-connect flags; the remaining ERC
violations are the ones this sheet exists to have — `multiple_net_names` on the
five two-name nets, `power_pin_not_driven` where no `PWR_FLAG` is placed, and
`isolated_pin_label` on the labels of `/AAA` and the deliberately one-pin
`/MIX`.
