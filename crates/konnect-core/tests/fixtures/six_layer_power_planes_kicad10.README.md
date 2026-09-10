# Six-layer board with power and mixed planes (issue #461)

`six_layer_power_planes_kicad10.kicad_pcb` is the board `estimate_cost` and
`validate_for_manufacturing` are tested against for their copper-layer count.

## Provenance

A minimal board was written by hand with this `(layers …)` table — six copper
layers of which only three are typed `signal`:

```
(0 "F.Cu" signal)
(4 "In1.Cu" power)
(6 "In2.Cu" signal)
(8 "In3.Cu" mixed)
(10 "In4.Cu" power)
(2 "B.Cu" signal)
```

plus the standard technical layers and a 30 × 20 mm rectangular outline. It was
then re-serialized by KiCad 10.0 on Windows with:

```text
kicad-cli pcb upgrade --force six_layer_power_planes_kicad10.kicad_pcb
```

so the committed bytes are pcbnew's own layout (`(generator "pcbnew")`,
`(version 20260206)`, tab-indented, CRLF, `legacy_teardrops`, `embedded_fonts`
and the rest of what KiCad adds). Nothing was edited after the resave.

## Why the kinds matter

The pre-#461 manufacturing tools counted copper by finding the substring
`signal)` in the file text. On this board that gives **3**; the `(layers …)`
table declares **6**. `get_board_info` already counted structurally and said 6,
so the three tools disagreed about the same file. The Jetson AGX Thor demo
shipped with KiCad 10 reproduces the same class with a single `jumper`-typed
layer (10 declared, 9 by substring); this fixture makes the gap unmissable.

## Tests using this fixture

`crates/konnect-core/src/tools/manufacturing.rs`, module
`copper_layer_count_tests`.
