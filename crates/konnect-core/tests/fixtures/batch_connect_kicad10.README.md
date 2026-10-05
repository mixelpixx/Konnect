# Batch connect fixture

`batch_connect_kicad10.kicad_sch` has pins facing all four ways, one of them on
a mirrored symbol, and a stub that ends mid-span on a wire. It checks
`batch_connect_to_net`'s placement options against what `main` wrote before
the change (#701).

## Provenance

Built through Konnect against KiCad's stock `MCU_Microchip_ATtiny` and `Device`
libraries with `create_schematic`, `add_schematic_component` and `add_wire`,
then force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force batch_connect_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`.

| Item | Placement | Pins used |
|---|---|---|
| `U1` `ATtiny85-20P` | `(101.6, 101.6)` | 5 (PB0) faces right, 8 (VCC) up, 4 (GND) down |
| `U2` `ATtiny85-20P` | `(177.8, 101.6)`, mirrored about Y | 5 (PB0) faces **left** |
| `R1` `Device:R` | `(63.5, 63.5)`, rotated 90° | 1 faces left |
| wire | `(119.38, 88.9)` → `(119.38, 99.06)` | U1.5's 2.54 mm stub ends in its middle |

## The oracle: `main` before the change

Every expected position was written by `main` @ `b37476b`, built from source
and driven over stdio on a copy of this file. The net is `N` and the pins are
taken in the order above.

`batch_connect_to_net` with only `net_name` puts a label on each pin endpoint
and draws no wire:

| Pin | Label at | Rotation |
|---|---|---|
| U1.5 | (116.84, 93.98) | 0 |
| U1.8 | (101.6, 86.36) | 0 |
| U1.4 | (101.6, 116.84) | 0 |
| U2.5 | (162.56, 93.98) | 180 |
| R1.1 | (59.69, 63.5) | 180 |

`connect_to_net`, one call per pin, gives the stub ends below. With
`label_type: "global_label"` the positions are the same. The 2.54 mm runs add
one junction, at `(119.38, 93.98)`, and the upward run adds none.

| Pin | default (`auto`, 2.54 mm) | `direction: "up"`, 5.08 mm |
|---|---|---|
| U1.5 | right → (119.38, 93.98), rot 0 | (116.84, 88.9), rot 0 |
| U1.8 | up → (101.6, 83.82), rot 0 | (101.6, 81.28), rot 0 |
| U1.4 | down → (101.6, 119.38), rot 0 | (101.6, 111.76), rot 0 |
| U2.5 | left → (160.02, 93.98), rot 180 | (162.56, 88.9), rot 0 |
| R1.1 | left → (57.15, 63.5), rot 180 | (59.69, 58.42), rot 0 |

With UUIDs masked, the new `batch_connect_to_net` writes byte-identical files
to all of these:

| New batch call | Compared with `main`'s | Result |
|---|---|---|
| `net_name` only | `batch_connect_to_net` | identical; same response |
| explicit `stub_length: 0`, `direction: "auto"`, `label_type: "net_label"` | `batch_connect_to_net` | identical; same response |
| `stub_length: 2.54` | `connect_to_net` × 5 | identical |
| `stub_length: 2.54`, `label_type: "global_label"` | `connect_to_net` × 5 | identical |
| `stub_length: 5.08`, `direction: "up"` | `connect_to_net` × 5 | identical |

## KiCad's own answer

`kicad-cli sch export netlist` on each result puts all five pins on one net.
It is the global `N` only where global labels were placed, and the sheet-local
`/N` everywhere else:

| File | Net | Pins |
|---|---|---|
| `main` batch; new batch, defaults | `/N` | R1.1 U1.4 U1.5 U1.8 U2.5 |
| `main` `connect_to_net`; new batch, `stub_length: 2.54` | `/N` | R1.1 U1.4 U1.5 U1.8 U2.5 |
| `main` `connect_to_net` global; new batch global | `N` | R1.1 U1.4 U1.5 U1.8 U2.5 |
| `main` `connect_to_net` up; new batch up | `/N` | R1.1 U1.4 U1.5 U1.8 U2.5 |

The fixture itself reports 18 `pin_not_connected`, 4 `power_pin_not_driven`,
1 `wire_dangling` and 2 `unconnected_wire_endpoint` from
`kicad-cli sch erc --severity-all`. No pin is wired and the wire touches
nothing, so this is expected.

## Coordinates in the response (#747)

`kicad-cli sch erc --format json --severity-all --units mm` (KiCad 10.0.6)
reports each unconnected pin's position, in units of 100 mm. Scaled to mm, it
places the five pins where the label table above has them:

| Pin | ERC `pos` | mm |
|---|---|---|
| U1.5 | (1.1684, 0.9398) | (116.84, 93.98) |
| U1.8 | (1.016, 0.8636) | (101.6, 86.36) |
| U1.4 | (1.016, 1.1684) | (101.6, 116.84) |
| U2.5 | (1.6256, 0.9398) | (162.56, 93.98) |
| R1.1 | (0.5969, 0.635) | (59.69, 63.5) |

`served_responses_report_kicads_coordinates` asserts these exact values from
`batch_get_schematic_pin_locations`, and the 2.54 mm stub ends from the table
above in `batch_connect_to_net`'s `wire` and `label`. Before the fix the
responses said `116.83999999999999` and `93.97999999999999`.
