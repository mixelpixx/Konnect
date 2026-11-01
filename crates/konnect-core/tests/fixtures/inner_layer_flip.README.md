# `inner_layer_flip_*` — inner copper through a side flip (#831)

Three boards and one library footprint, all written by KiCad 10.0.6's own
`pcbnew` Python (`python3 -I build.py <out>`, script below). Nothing is
hand-authored: KiCad's parser read the stock `.kicad_mod` files, and KiCad's
writer wrote every byte.

| File | What it is |
|---|---|
| `inner_layer_flip_front.kicad_pcb` | 8 copper layers. `NT1` (stock `NetTie-3_SMD_Pad0.5mm`, its three pads and copper fill moved to `In2.Cu`) on F.Cu at 100,50 rotated -90°, and an unrelated stock `R1` (`R_0402_1005Metric`) on F.Cu. |
| `inner_layer_flip_back.kicad_pcb` | The same board after KiCad's own `FOOTPRINT::Flip` of `NT1`: the oracle for `flip_component`. |
| `inner_layer_flip_missing.kicad_pcb` | The back board after `SetCopperLayerCount(4)`. KiCad keeps `NT1`'s pads and fill on `In5.Cu`, which a 4-layer board does not have. |
| `inner_layer_refresh_r0402.kicad_mod` | Stock `R_0402_1005Metric` with both pads moved to `In2.Cu` and an `IN2` text added on `In2.Cu`, saved with `PCB_IO_KICAD_SEXPR().FootprintSave`. The net tie can't be used here, because library refresh refuses `net_tie_pad_groups` (#775). |
| `inner_layer_flip_padstack.kicad_pcb` | 8 copper layers. Stock `PinHeader_1x01_P2.54mm_Vertical` as `J1`, its pad set to a custom padstack with a 2.2 mm `In2.Cu` entry. |

The front and back boards come from one run; later runs differ only in UUIDs.
`inner_layer_flip_missing` was saved from the committed back board, and the
refresh footprint from a later run of the same script.

## Oracle

KiCad's flip of `NT1`, read from the front and back boards:

| Item | front | KiCad back |
|---|---|---|
| root `(layer …)` | `F.Cu` | `B.Cu` |
| root `(at …)` | `100 50 -90` | `100 50 90` |
| `fp_poly` fill | `In2.Cu` | `In5.Cu` |
| pads 1, 2, 3 | `In2.Cu` | `In5.Cu` |

KiCad's flip of the refresh footprint on the same 8-layer board, printed by
the script: pads `['In5.Cu', 'In5.Cu']`, text `In5.Cu mirrored`.

KiCad's flip of `J1` moves the padstack's 2.2 mm entry from `In2.Cu` to
`In5.Cu`. Konnect's file flip does not rewrite padstacks, so it refuses them.
A pad's `zone_layer_connections "In2.Cu"` stays on `In2.Cu` through KiCad's
flip (checked the same way with `SetZoneLayerOverride`), so Konnect leaves it
too.

`pcbnew.FlipLayer(layer, N)` for the rows `konnect-sexp`'s
`inner_copper_mirrors_as_kicads_flip_layer` asserts:

| N | In-range rows | Out of range |
|---|---|---|
| 2 | — | `In<k>` left unchanged |
| 4 | In1↔In2 | clamped to `In1.Cu` |
| 6 | In1↔In4, In2↔In3 | clamped to `In1.Cu` |
| 8 | In1↔In6, In2↔In5, In3↔In4 | clamped to `In1.Cu` |
| 32 | In1↔In30, In8↔In23 | — |

Konnect refuses where KiCad clamps, or keeps the layer, because neither result
is the footprint's mirror.

## Tests

| Test | Asserts |
|---|---|
| `flip_mirrors_inner_copper_through_the_board_stack` | served `flip_component` to B.Cu gives `NT1` the per-UUID layers of KiCad's back board, leaves `R1` byte-identical, and flipping back to F.Cu restores the front board's tree |
| `flip_refuses_an_inner_layer_the_board_lacks` | flipping the missing board returns `plan_blocked` naming `NT1`, `In5.Cu` and the 4-layer stack, and leaves the file byte-identical |
| `back_side_refresh_mirrors_inner_copper_through_the_board_stack` | refresh on 8 layers puts the pads and text on `In2.Cu` (front) or `In5.Cu` (back, text mirrored), and refuses on a 2-layer stack |
| `flip_refuses_a_per_layer_padstack` | flipping `J1` refuses and leaves the file byte-identical |

## `build.py`

```python
import sys, os, pcbnew
out = sys.argv[1]
LIB = "/usr/share/kicad/footprints"

def board():
    b = pcbnew.BOARD()
    b.SetCopperLayerCount(8)
    for name in ("TIE_A", "TIE_B"):
        b.Add(pcbnew.NETINFO_ITEM(b, name))
    pts = [(80, 30), (130, 30), (130, 70), (80, 70)]
    for i in range(4):
        s = pcbnew.PCB_SHAPE(b)
        s.SetShape(pcbnew.SHAPE_T_SEGMENT)
        s.SetLayer(pcbnew.Edge_Cuts)
        a, c = pts[i], pts[(i + 1) % 4]
        s.SetStart(pcbnew.VECTOR2I(pcbnew.FromMM(a[0]), pcbnew.FromMM(a[1])))
        s.SetEnd(pcbnew.VECTOR2I(pcbnew.FromMM(c[0]), pcbnew.FromMM(c[1])))
        b.Add(s)
    return b

def inner_tie(b):
    fp = pcbnew.FootprintLoad(LIB + "/NetTie.pretty", "NetTie-3_SMD_Pad0.5mm")
    for pad in fp.Pads():
        ls = pcbnew.LSET()
        ls.AddLayer(pcbnew.In2_Cu)
        pad.SetLayerSet(ls)
    for item in fp.GraphicalItems():
        if item.GetLayer() == pcbnew.F_Cu:
            item.SetLayer(pcbnew.In2_Cu)
    return fp

b = board()
fp = inner_tie(b)
lib = os.path.join(out, "Konnect_InnerTie.pretty")
io = pcbnew.PCB_IO_KICAD_SEXPR()
io.CreateLibrary(lib)
fp.SetFPID(pcbnew.LIB_ID("Konnect_InnerTie", "NetTie-3_SMD_Inner_Pad"))
io.FootprintSave(lib, fp)

fp.SetReference("NT1")
fp.SetPosition(pcbnew.VECTOR2I(pcbnew.FromMM(100), pcbnew.FromMM(50)))
fp.SetOrientationDegrees(-90)
b.Add(fp)
nets = b.GetNetsByName()
for pad in fp.Pads():
    pad.SetNet(nets["TIE_A"] if pad.GetNumber() != "3" else nets["TIE_B"])

r = pcbnew.FootprintLoad(LIB + "/Resistor_SMD.pretty", "R_0402_1005Metric")
r.SetReference("R1")
r.SetPosition(pcbnew.VECTOR2I(pcbnew.FromMM(115), pcbnew.FromMM(50)))
b.Add(r)
pcbnew.SaveBoard(os.path.join(out, "inner_layer_flip_front.kicad_pcb"), b)

fp.Flip(fp.GetPosition(), pcbnew.FLIP_DIRECTION_TOP_BOTTOM)
pcbnew.SaveBoard(os.path.join(out, "inner_layer_flip_back.kicad_pcb"), b)

# Library refresh fixture: the net tie is refused by refresh (#775), so use a
# stock resistor with its pads moved to In2.Cu, and record KiCad's own flip.
r_inner = pcbnew.FootprintLoad(LIB + "/Resistor_SMD.pretty", "R_0402_1005Metric")
for pad in r_inner.Pads():
    ls = pcbnew.LSET()
    ls.AddLayer(pcbnew.In2_Cu)
    pad.SetLayerSet(ls)
text = pcbnew.PCB_TEXT(r_inner)
text.SetText("IN2")
text.SetLayer(pcbnew.In2_Cu)
text.SetTextSize(pcbnew.VECTOR2I(pcbnew.FromMM(1), pcbnew.FromMM(1)))
text.SetTextThickness(pcbnew.FromMM(0.15))
text.SetPosition(pcbnew.VECTOR2I(0, pcbnew.FromMM(0.8)))
r_inner.Add(text)
r_inner.SetFPID(pcbnew.LIB_ID("Konnect_InnerTie", "R_0402_Inner_Pad"))
io.FootprintSave(lib, r_inner)
b2 = board()
b2.Add(r_inner)
r_inner.Flip(r_inner.GetPosition(), pcbnew.FLIP_DIRECTION_TOP_BOTTOM)
print("KiCad flip of R_0402_Inner_Pad pads:",
      [b2.GetLayerName(p.GetLayerSet().Seq()[0]) for p in r_inner.Pads()])
for item in r_inner.GraphicalItems():
    if isinstance(item, pcbnew.PCB_TEXT) and item.GetText() == "IN2":
        print("KiCad flip of its In2.Cu text:", b2.GetLayerName(item.GetLayer()),
              "mirrored" if item.IsMirrored() else "not mirrored")
```

`inner_layer_flip_missing.kicad_pcb`, from the committed back board:

```python
import sys, pcbnew
b = pcbnew.LoadBoard(sys.argv[1] + "/inner_layer_flip_back.kicad_pcb")
b.SetCopperLayerCount(4)
pcbnew.SaveBoard(sys.argv[1] + "/inner_layer_flip_missing.kicad_pcb", b)
```

`SaveBoard` also writes `.kicad_pro`/`.kicad_prl` siblings; they are not
committed.

`inner_layer_flip_padstack.kicad_pcb`:

```python
import sys, pcbnew
b = pcbnew.BOARD()
b.SetCopperLayerCount(8)
fp = pcbnew.FootprintLoad("/usr/share/kicad/footprints/Connector_PinHeader_2.54mm.pretty",
                          "PinHeader_1x01_P2.54mm_Vertical")
fp.SetReference("J1")
fp.SetPosition(pcbnew.VECTOR2I(pcbnew.FromMM(100), pcbnew.FromMM(50)))
b.Add(fp)
pad = list(fp.Pads())[0]
pad.Padstack().SetMode(pcbnew.PADSTACK.MODE_CUSTOM)
pad.SetSize(pcbnew.In2_Cu, pcbnew.VECTOR2I(pcbnew.FromMM(2.2), pcbnew.FromMM(2.2)))
pcbnew.SaveBoard(sys.argv[1] + "/inner_layer_flip_padstack.kicad_pcb", b)
```
