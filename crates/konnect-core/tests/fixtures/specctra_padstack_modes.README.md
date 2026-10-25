# Specctra per-layer padstack fixtures

Three two-layer boards for #842, each with two stock
`PinHeader_1x02_P2.54mm_Vertical` footprints (`J1` at (100, 50), `J2` at
(110, 50), both at 0°). They differ only in J1 pad 1, a 1.7 mm square
through-hole pad:

| fixture | J1 pad 1 padstack |
|---|---|
| `specctra_padstack_normal.kicad_pcb` | normal (KiCad writes no `(padstack ...)` block) |
| `specctra_padstack_front_inner_back.kicad_pcb` | `front_inner_back`, a 2 mm circle on `B.Cu` |
| `specctra_padstack_custom.kicad_pcb` | `custom`, a 2 mm circle on `B.Cu` |

KiCad 10.0.6's `pcbnew` Python API built and saved each board, so every byte is
KiCad's serialization:

```text
python3 -I build.py specctra_padstack_<mode>.kicad_pcb <mode>
```

```python
import sys, pcbnew
mode_name = sys.argv[2]
b = pcbnew.BOARD()
b.SetCopperLayerCount(2)
mm = pcbnew.FromMM
lib = "/usr/share/kicad/footprints/"
nets = {}
for n in ["VCC", "GND"]:
    ni = pcbnew.NETINFO_ITEM(b, n); b.Add(ni); nets[n] = ni
for ref, x in [("J1", 100), ("J2", 110)]:
    fp = pcbnew.FootprintLoad(lib + "Connector_PinHeader_2.54mm.pretty", "PinHeader_1x02_P2.54mm_Vertical")
    fp.SetReference(ref)
    fp.SetPosition(pcbnew.VECTOR2I(mm(x), mm(50)))
    b.Add(fp)
    for pad, net in zip(sorted(fp.Pads(), key=lambda p: p.GetNumber()), ("VCC", "GND")):
        pad.SetNet(nets[net])
j1 = b.FindFootprintByReference("J1")
pad = sorted(j1.Pads(), key=lambda p: p.GetNumber())[0]
ps = pad.Padstack()
if mode_name == "front_inner_back":
    ps.SetMode(pcbnew.PADSTACK.MODE_FRONT_INNER_BACK)
elif mode_name == "custom":
    ps.SetMode(pcbnew.PADSTACK.MODE_CUSTOM)
if mode_name != "normal":
    pad.SetShape(pcbnew.B_Cu, pcbnew.PAD_SHAPE_CIRCLE)
    pad.SetSize(pcbnew.B_Cu, pcbnew.VECTOR2I(mm(2), mm(2)))
pts = [(90, 40), (120, 40), (120, 60), (90, 60)]
for i in range(4):
    s = pcbnew.PCB_SHAPE(b); s.SetShape(pcbnew.SHAPE_T_SEGMENT)
    s.SetStart(pcbnew.VECTOR2I(mm(pts[i][0]), mm(pts[i][1])))
    s.SetEnd(pcbnew.VECTOR2I(mm(pts[(i + 1) % 4][0]), mm(pts[(i + 1) % 4][1])))
    s.SetLayer(pcbnew.Edge_Cuts); s.SetWidth(mm(0.05)); b.Add(s)
pcbnew.SaveBoard(sys.argv[1], b)
```

## Locked via

`specctra_padstack_locked_via.kicad_pcb` is `specctra_two_resistors_locked.kicad_pcb`
with its locked via set to `front_inner_back` and given a 1 mm `B.Cu` size
under its 0.6 mm front. KiCad saved it with:

```python
import sys, pcbnew
b = pcbnew.LoadBoard(sys.argv[1])
via = [t for t in b.GetTracks() if t.GetClass() == "PCB_VIA"][0]
via.Padstack().SetMode(pcbnew.PADSTACK.MODE_FRONT_INNER_BACK)
via.Padstack().SetSize(pcbnew.VECTOR2I(pcbnew.FromMM(1), pcbnew.FromMM(1)), pcbnew.B_Cu)
pcbnew.SaveBoard(sys.argv[2], b)
```

Only the via's `(padstack ...)` block differs from the source board:

```text
(padstack
  (mode front_inner_back)
  (layer "Inner" (size 0.6))
  (layer "B.Cu" (size 1)))
```

## Oracle

The saved `front_inner_back` board holds J1 pad 1 as:

```text
(pad "1" thru_hole rect
  (size 1.7 1.7)
  ...
  (padstack
    (mode front_inner_back)
    (layer "Inner" (shape rect) (size 1.7 1.7) (zone_connect -1))
    (layer "B.Cu" (shape circle) (size 2 2))))
```

The `custom` board holds the same pad with only the `B.Cu` layer entry. Its
top-level `shape` and `size` describe `F.Cu` alone, so an exporter that reads
only them writes a 1.7 mm square on `B.Cu`, where KiCad has a 2 mm circle.

KiCad's own Specctra exporter has the same flaw. `pcbnew.ExportSpecctraDSN` on
the `front_inner_back` and `custom` boards writes the pad as
`Rect[A]Pad_1700.000000x1700.000000_um`, with
`(shape (rect B.Cu -850 -850 850 850))`. So a native DSN cannot stand in for
the refused export either.
