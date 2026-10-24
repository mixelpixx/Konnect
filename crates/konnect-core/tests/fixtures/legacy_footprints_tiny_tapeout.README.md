# `testpoint_keystone_5019_kicad6.kicad_mod`, `cui_ujc_hp_3_kicad7.kicad_mod`

Two footprints in KiCad's older formats, copied unmodified from the Tiny
Tapeout demo library that KiCad 10.0.6 ships
(`/usr/share/kicad/demos/tiny_tapeout/tinytapeout-kicad-libs/footprints`,
Fedora 44 `kicad-10.0.6-1.fc44`). That library is Apache-2.0, per the demo's
`LICENSE.txt`.

| Fixture | Source | Format |
|---------|--------|--------|
| `testpoint_keystone_5019_kicad6.kicad_mod` | `ttlib.pretty/TestPoint_Keystone_5019_Minature.kicad_mod` | KiCad 6, `(version 20211014)` |
| `cui_ujc_hp_3_kicad7.kicad_mod` | `TinyTapeout.pretty/CUI_UJC-HP-3-SMT-TR.kicad_mod` | KiCad 7, `(version 20221018)` |

`pcb_components`' tests read their graphics for #970. They are the forms the
current format writes differently:

- the test point's 24 `fp_line`s carry a bare `(width …)` and no stroke type;
- the jack's one user text is unlocked inside its position,
  `(at -2.8 3.2 unlocked)`, where KiCad 8 and later write `(unlocked yes)`.

## Oracle

KiCad 10.0.6 itself: both footprints loaded with `pcbnew.FootprintLoad`, added
to an empty board and written with `pcbnew.SaveBoard`.

| Footprint | What KiCad saved |
|-----------|------------------|
| Test point | 24 `(type solid)`, no other stroke type; its user text has no `(unlocked yes)` |
| Jack | 13 `(type solid)`; its user text `(unlocked yes)` |
