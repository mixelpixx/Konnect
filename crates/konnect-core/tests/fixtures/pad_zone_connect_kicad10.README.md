# Pad zone-connection footprints

Four versions of the same footprint, each written by KiCad, used to check that
`edit_footprint_pad`'s `zone_connect` argument writes a pad exactly as KiCad
does (#453).

The footprint is `MountingHole.pretty/MountingHole_3.2mm_M3_Pad_TopBottom`
from the KiCad 10.0.6 standard library: three pads that all share the number
`1` (one `thru_hole`, two `connect`), each carrying `(zone_connect 2)`.

## Provenance

All four files were written by KiCad 10.0.6's own footprint writer on Windows
11, through the Python bindings it ships (`pcbnew.PCB_IO_KICAD_SEXPR`):
`FootprintLoad(library, name, True)` to keep UUIDs, one change through
`PAD.SetLocalZoneConnection`, then `FootprintSave`. Nothing was edited by hand,
and the repository stores the directory `-text`, so the bytes (including
KiCad's CRLF line endings) are KiCad's.

| Fixture | Written from | Change made in KiCad | SHA-256 (first 16) |
|---|---|---|---|
| `pad_zone_connect_kicad10.kicad_mod` | the library file | none (load and save) | `07c8b7321230d7e6` |
| `pad_zone_connect_thermal_kicad10.kicad_mod` | `pad_zone_connect_kicad10` | every pad to `ZONE_CONNECTION_THERMAL` | `ea2c71bbbd3e20f9` |
| `pad_zone_connect_inherited_kicad10.kicad_mod` | `pad_zone_connect_kicad10` | every pad to `ZONE_CONNECTION_INHERITED` | `2d79ad0db57fde3c` |
| `pad_zone_connect_first_none_kicad10.kicad_mod` | `pad_zone_connect_inherited_kicad10` | first pad to `ZONE_CONNECTION_NONE` | `0e151812231fc5b5` |

## What they show

- KiCad writes `(zone_connect 0)` for none, `1` for thermal reliefs and `2` for
  solid, after `(layers …)`/`(remove_unused_layers …)` and before the pad's
  `(uuid …)`.
- An inherited pad has no `zone_connect` token at all.
- Pads, graphics and the `KiLib_Generator` property keep their UUIDs across
  these saves, but KiCad gives the four mandatory fields (Reference, Value,
  Datasheet, Description) new UUIDs on each save even when asked to keep them.
  Tests therefore compare the pads with KiCad's and require everything else to
  be unchanged from the input, rather than comparing whole files.
