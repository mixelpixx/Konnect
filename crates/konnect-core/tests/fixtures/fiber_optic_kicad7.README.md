# KiCad 7 symbol library fixture

`fiber_optic_kicad7.kicad_sym` is KiCad's stock `Fiber_Optic` library from
kicad-symbols tag `7.0.10`, unedited:

```text
curl -O https://gitlab.com/kicad/libraries/kicad-symbols/-/raw/7.0.10/Fiber_Optic.kicad_sym
sha256 52d86587d04bf71e5d9ebeffcbb259c0de8d5d7dce114f0f2a6b7cbcb7b9d0ab
```

It is in the KiCad 6/7 format (`version 20220914`), which has no
`Description` field. The description lives in a `ki_description` property, so
`set_symbol_properties` has to edit that property for a new description to
reach KiCad (#892).

`fiber_optic_kicad7.edited.kicad_sym` is what `set_symbol_properties` wrote for
this call:

```text
set_symbol_properties AFBR-1624Z  description "Versatile Link transmitter, 650 nm",
                                  footprint OptoDevice:Broadcom_AFBR-16xxZ_Horizontal,
                                  fields {MPN: AFBR-1624Z}
```

`fiber_optic_kicad7.edited.resaved.kicad_sym` is KiCad 10.0.6's reading of
that output:

```text
kicad-cli sym upgrade --force fiber_optic_kicad7.edited.kicad_sym \
  -o fiber_optic_kicad7.edited.resaved.kicad_sym
```

## Oracle

The resave lists these fields for `AFBR-1624Z`:

| Field | Stock library | KiCad's reading after the edit |
|---|---|---|
| `Footprint` | empty | `OptoDevice:Broadcom_AFBR-16xxZ_Horizontal` |
| `Description` (`ki_description` in the stock file) | `Versatile Link Fiber Optic Transmitter` | `Versatile Link transmitter, 650 nm` |
| `MPN` | absent | `AFBR-1624Z` |
| `ki_keywords` | `Fiber optic transmitter` | unchanged |
| `ki_fp_filters` | `Broadcom*AFBR*16xxZ*` | unchanged |

Before the fix, the tool added a separate `Description` property. KiCad loaded
it, then overwrote it with the older `ki_description`, so the resave kept
`Versatile Link Fiber Optic Transmitter`.
