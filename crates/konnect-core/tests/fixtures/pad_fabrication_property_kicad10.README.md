# Pad fabrication-property footprints

Five versions of the same footprint, each written by KiCad, used to check that
`edit_footprint_pad`'s `fabrication_property` argument writes a pad exactly as
KiCad does (#961).

The footprint is
`Connector_USB.pretty/USB_C_Receptacle_GCT_USB4125-xx-x_6P_TopMnt_Horizontal`
from the KiCad 10.0.6 standard library. It has six SMD pads (`A5` … `B12`)
and four plated shield pads that all share the number `SH`, each
`thru_hole oval` with `(drill oval 0.6 1.2)` and `(property pad_prop_mechanical)`.
No stock footprint carries `pad_prop_castellated`, so KiCad writes that case
from this one, on PTH pads, where KiCad accepts it.

## Provenance

All five files were written by KiCad 10.0.6's own footprint writer on Fedora
44, through its Python bindings (`pcbnew.PCB_IO_KICAD_SEXPR`):
`FootprintLoad(library, name, True)` to keep UUIDs, one change through
`PAD.SetProperty`, then `FootprintSave`. Nothing was edited by hand.

| Fixture | Written from | Change made in KiCad | SHA-256 (first 16) |
|---|---|---|---|
| `pad_fabrication_property_mechanical_kicad10.kicad_mod` | the library file | none (load and save) | `5188d44f9a0c69af` |
| `pad_fabrication_property_castellated_kicad10.kicad_mod` | `…_mechanical_kicad10` | every `SH` pad to `PAD_PROP_CASTELLATED` | `37afab8c62e3dfb6` |
| `pad_fabrication_property_first_castellated_kicad10.kicad_mod` | `…_mechanical_kicad10` | first `SH` pad to `PAD_PROP_CASTELLATED` | `40d395590be96040` |
| `pad_fabrication_property_none_kicad10.kicad_mod` | `…_mechanical_kicad10` | every `SH` pad to `PAD_PROP_NONE` | `43c77b38ff19689a` |
| `pad_fabrication_property_testpoint_kicad10.kicad_mod` | `…_mechanical_kicad10` | pad `A5` to `PAD_PROP_TESTPOINT` | `d071cd9bdcdfb6f4` |

## What they show

- KiCad writes `(property pad_prop_<name>)` directly after the pad's
  `(drill …)` and before `(layers …)`, and writes nothing for `PAD_PROP_NONE`.
- A pad that had no property gets one in the same place.
- As with `pad_zone_connect_kicad10.README.md`, KiCad gives the four mandatory
  fields new UUIDs on each save, so tests compare the pads with KiCad's and
  require everything else to be unchanged from the input.

## KiCad's padstack check

Each fixture was placed alone on a board and checked with
`kicad-cli pcb drc --severity-all` (KiCad 10.0.6). None reports a `padstack`
violation. As a control, the same footprint with `PAD_PROP_CASTELLATED` on
SMD pad `A5` reports one warning, `Padstack is questionable ('castellated'
pads are normally PTH)`, which is the text `edit_footprint_pad` and
`create_footprint` return in `warnings` for that case (`PAD::CheckPad`,
`pcbnew/pad.cpp` at 10.0.6-rc2).
