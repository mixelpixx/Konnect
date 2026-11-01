# Wire-end junction fixture

`wire_end_junctions_kicad10.kicad_sch` is a sheet with no wires. It holds one
of each item a junction count takes as an exit besides wires. The
`wire_end_junction_tests` in `sch_wiring.rs` draw wires onto it and check
which junction dots the wire tools write (#897).

## Provenance

Built through Konnect, against KiCad's stock `Device` and `74xx` libraries:

| Step | Arguments |
|---|---|
| `create_schematic` | |
| `add_schematic_component` | `Device:R`, `R1`, `10k`, at `(152.4, 101.6)` |
| `add_schematic_component` | `74xx:74LS00`, `U1`, unit 1, at `(190.5, 101.6)` |
| `add_hierarchical_sheet` | `Child`, file `wire_end_junctions_child.kicad_sch`, at `(50.8, 139.7)`, size `30.48 × 20.32` |
| `add_sheet_pin` | `SIG`, input, right side, at `(81.28, 147.32)` |
| `add_bus` | `(203.2, 149.86)→(241.3, 149.86)` |
| `add_bus_entry` | wire side `(213.36, 147.32)`, `down_right` |

KiCad 10.0.6 then parsed and force-resaved both files:

```text
kicad-cli sch upgrade --force wire_end_junctions_kicad10.kicad_sch
kicad-cli sch upgrade --force wire_end_junctions_child.kicad_sch
```

The committed files are that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`. The parent has no wires
and no junctions. The child sheet is empty; it is committed only so the parent
opens as KiCad saved it.

| Point | What ends there |
|---|---|
| `(152.4, 97.79)` | `R1` pin 1 |
| `(198.12, 101.6)` | `U1` pin 3, the output. `74LS00` is a De Morgan symbol, and both body styles (`74LS00_1_1`, `74LS00_1_2`) put pin 3 at library `(7.62, 0)` |
| `(81.28, 147.32)` | the `SIG` sheet pin |
| `(213.36, 147.32)` | the bus entry's wire side |

## The rule and its oracle

eeschema's `JUNCTION_HELPERS::AnalyzePoint` (`eeschema/junction_helpers.cpp`)
counts the distinct wire *exit angles* at a point. A wire that ends there
gives one exit, a wire that passes through gives two once something ends
there, and each connected symbol, sheet or bus entry gives one unique exit.
So two pins one symbol stacks on a point count once. The point is a junction at
three or more exits, and `SCH_SCREEN::IsExplicitJunctionNeeded` asks for a dot
there.

KiCad's own demo corpus agrees. Across every `.kicad_sch` under `demos/` in
the KiCad 10.0 source tree, the scan below found 3408 points where three or
more wire ends meet with no wire passing through. All 3408 carry a dot, and
none is missing one:

```text
python3 -I corpus.py <kicad-10.0>/demos
{('1', False): 21724, ('3+', True): 3408, ('2', False): 2508, ('2', True): 659, ('1', True): 181}
```

The keys are (distinct wire-end exits, has a dot). The scan parses `(wire (pts
...))` and `(junction (at ...))`, keys points at 0.01 mm, takes each wire end's
exit angle toward the wire's other end, and skips any point that lies mid-span on
another wire, since `find_t_junctions` already handles those.

`corpus.py`:

```python
import re, sys, glob, math, collections
wire_re = re.compile(r'\(wire\s*\(pts\s*\(xy ([-\d.]+) ([-\d.]+)\)\s*\(xy ([-\d.]+) ([-\d.]+)\)')
junc_re = re.compile(r'\(junction\s*\(at ([-\d.]+) ([-\d.]+)\)')
k=lambda x,y:(round(float(x)*100),round(float(y)*100))
tot=collections.Counter()
for f in sorted(glob.glob(sys.argv[1]+'/**/*.kicad_sch', recursive=True)):
    s=open(f).read()
    ends=collections.defaultdict(set)
    segs=[]
    for m in wire_re.finditer(s):
        a=k(m[1],m[2]); b=k(m[3],m[4])
        if a==b: continue
        segs.append((a,b))
        ends[a].add(round(math.degrees(math.atan2(b[1]-a[1],b[0]-a[0]))))
        ends[b].add(round(math.degrees(math.atan2(a[1]-b[1],a[0]-b[0]))))
    juncs={k(m[1],m[2]) for m in junc_re.finditer(s)}
    def midspan(p):
        for a,b in segs:
            if p in (a,b): continue
            cross=(b[0]-a[0])*(p[1]-a[1])-(b[1]-a[1])*(p[0]-a[0])
            if cross==0 and min(a[0],b[0])<=p[0]<=max(a[0],b[0]) and min(a[1],b[1])<=p[1]<=max(a[1],b[1]): return True
        return False
    for p,angs in ends.items():
        if midspan(p): continue   # only end-to-end meetings
        n=len(angs)
        key=('3+' if n>=3 else str(n), p in juncs)
        tot[key]+=1
        if n>=3 and p not in juncs: print('MISSING', f, p)
print(dict(tot))
```

## Cases

Each case starts from a fresh copy of the fixture.

| Case | Wires | Dots |
|---|---|---|
| three wire ends at a T | `(101.6,101.6)→(114.3,101.6)`, `(114.3,101.6)→(127,101.6)`, `(114.3,101.6)→(114.3,88.9)` | `(114.3, 101.6)` |
| a branch landing mid-span (control) | `(101.6,101.6)→(127,101.6)`, `(114.3,101.6)→(114.3,88.9)` | `(114.3, 101.6)` |
| a wire laid across an existing wire end | `(114.3,101.6)→(114.3,88.9)`, then `(101.6,101.6)→(127,101.6)` | `(114.3, 101.6)` |
| four wire ends at a cross | the T above plus `(114.3,114.3)→(114.3,101.6)` | `(114.3, 101.6)` |
| two wire ends at a corner | `(101.6,101.6)→(114.3,101.6)`, `(114.3,101.6)→(114.3,88.9)` | none |
| a duplicated segment | `(101.6,101.6)→(114.3,101.6)` and `(114.3,101.6)→(127,101.6)` twice | none |
| two wire ends at a pin tip | `(139.7,97.79)→(152.4,97.79)`, `(152.4,97.79)→(152.4,88.9)` | `(152.4, 97.79)` |
| one wire end at a pin tip | `(139.7,97.79)→(152.4,97.79)` | none |
| a collinear overlap ending mid-span | `(101.6,101.6)→(127,101.6)`, `(114.3,101.6)→(120.65,101.6)` | none |
| one wire end at a pin both body styles put there | `(198.12,101.6)→(210.82,101.6)` | none |
| two wire ends at a sheet pin | `(81.28,147.32)→(93.98,147.32)`, `(81.28,147.32)→(81.28,134.62)` | `(81.28, 147.32)` |
| two wire ends at a bus entry | `(200.66,147.32)→(213.36,147.32)`, `(213.36,147.32)→(213.36,134.62)` | `(213.36, 147.32)` |

The duplicated segment has three wire ends at `(114.3, 101.6)` but only two
exit angles, so a count of ends draws a dot there and KiCad's count of exits
does not. The pin rows show that a pin tip counts as an exit. The collinear
overlap has two exits at each of its inner ends; before #897 Konnect dotted
both, because `find_t_junctions` saw a wire end landing mid-span. The gate row
gives one wire exit plus one symbol. Counting pins rather than symbols makes it
three, because Konnect's pin list carries pin 3 once per body style.
