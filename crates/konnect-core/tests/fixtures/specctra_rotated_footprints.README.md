# Rotated-footprint Specctra fixtures

`specctra_rotated_footprints.kicad_pcb` and `specctra_rotated_footprints_45.kicad_pcb`
were built from `specctra_two_resistors.kicad_pcb` with KiCad 10.0.6's `pcbnew`
Python, then saved through `pcbnew.SaveBoard`. Each `.native-kicad-10.dsn` is
KiCad's `pcbnew.ExportSpecctraDSN` of the saved board. Only the absolute output
path in the root `(pcb ...)` atom was normalized to `board.dsn`.

```bash
python3 -I make840.py specctra_two_resistors.kicad_pcb <out_dir>
```

```python
# Usage: python3 -I make840.py <specctra_two_resistors.kicad_pcb> <out_dir>
import os, sys, pcbnew
src, out = sys.argv[1:3]
b = pcbnew.LoadBoard(src)
fps = {fp.GetReference(): fp for fp in b.GetFootprints()}
nets = {n: b.FindNet(n) for n in ("VCC", "GND")}

def clone(ref, x, y, degrees, pad_nets):
    fp = pcbnew.Cast_to_FOOTPRINT(fps["R2"].Duplicate(False))
    b.Add(fp)
    fp.SetReference(ref)
    fp.SetPosition(pcbnew.VECTOR2I_MM(x, y))
    fp.SetOrientationDegrees(degrees)
    for pad in fp.Pads():
        pad.SetNet(nets[pad_nets[pad.GetNumber()]])
    return fp

def save(stem):
    pcb = os.path.join(out, stem + ".kicad_pcb")
    pcbnew.SaveBoard(pcb, b)
    assert pcbnew.ExportSpecctraDSN(pcbnew.LoadBoard(pcb), os.path.join(out, stem + ".native-kicad-10.dsn"))

fps["R1"].SetOrientationDegrees(90)
# KiCad writes a pin's rotate with %.6g: 12.3456789 becomes 12.3457.
for pad in fps["R1"].Pads():
    pad.SetFPRelativeOrientation(pcbnew.EDA_ANGLE(12.3456789, pcbnew.DEGREES_T))
r4 = clone("R4", 120, 60, 90, {"1": "GND", "2": "VCC"})
for pad in r4.Pads():
    turn = {"1": 90, "2": -90}[pad.GetNumber()]
    pad.SetFPRelativeOrientation(pcbnew.EDA_ANGLE(turn, pcbnew.DEGREES_T))
save("specctra_rotated_footprints")
clone("R3", 120, 40, 45, {"1": "VCC", "2": "GND"})
save("specctra_rotated_footprints_45")
```

A pad's `(at x y angle)` angle in a `.kicad_pcb` includes its footprint's
rotation. KiCad's DSN writes each pin's `(rotate ...)` as the pad's angle within
the footprint, normalized to [0, 360) and printed with `%.6g`, and omits it
when 0 (#840):

| Ref | Footprint | Pad angles in the file | KiCad's pin `(rotate ...)` |
|-----|-----------|------------------------|----------------------------|
| R1 | 90° | 102.3456789, 102.3456789 | 12.3457, 12.3457 |
| R2 | 0° | 0, 0 | none, none |
| R3 (`_45` only) | 45° | 45, 45 | none, none |
| R4 | 90° | 180, 0 | 90, 270 |

KiCad stores R3's pads at ±0.499999 mm, and its DSN writes them at ±499.999 µm.
`adopt_native_dsn` compares whole-micrometre pin positions, so it refuses that
board (#841). The adoption tests therefore use the board without R3. The `_45`
board checks pin rotations only.
