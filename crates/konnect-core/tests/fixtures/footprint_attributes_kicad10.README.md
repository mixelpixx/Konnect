# Footprint attribute fixtures

Two stock KiCad 10.0.6 footprints, used by `set_footprint_metadata`'s
reporting tests (#895):

| File | Source | `(attr …)` |
|---|---|---|
| `c_0603_1608metric_kicad10.kicad_mod` | already committed; stock `Capacitor_SMD.pretty/C_0603_1608Metric` | `smd` |
| `solder_jumper_open_kicad10.kicad_mod` | stock `Jumper.pretty/SolderJumper-2_P1.3mm_Open_Pad1.0x1.5mm`, copied unchanged from `/usr/share/kicad/footprints` (`(generator "pcbnew")`, `(generator_version "10.0")`) | `exclude_from_pos_files exclude_from_bom allow_soldermask_bridges` |

## The oracle: KiCad's reading of each result

Each call ran on a copy, driven over stdio. Each result was loaded with KiCad
10.0.6's own `pcbnew` Python, `pcbnew.FootprintLoad`. The type comes from
`GetAttributes()` (`FP_SMD` / `FP_THROUGH_HOLE`, neither = unspecified), and
the solder-mask flag from `AllowSolderMaskBridges()`.

| Footprint | `attributes` sent | KiCad type | Solder-mask bridges | Warning |
|---|---|---|---|---|
| C_0603, stock | — | smd | False | — |
| C_0603 | `["exclude_from_bom"]` | **unspecified** | False | footprint type dropped |
| C_0603 | `["smd", "exclude_from_bom"]` | smd | False | none |
| solder jumper, stock | — | unspecified | True | — |
| solder jumper | `["exclude_from_pos_files", "exclude_from_bom"]` | unspecified | **False** | `allow_soldermask_bridges` dropped (not accepted by `attributes`) |

`main` @ `9488e5f` wrote byte-identical files for every call and answered only
`{"success":true,"changed_fields":["attributes"],…}`. `allow_soldermask_bridges`
cannot be sent back, because the `attributes` enum refuses it (#902).
