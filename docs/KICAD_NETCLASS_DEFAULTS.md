# KiCad Netclass Defaults

Netclasses live in `.kicad_pro` under `net_settings.classes`. Konnect writes them
from `create_netclass` (`crates/konnect-core/src/tools/pcb_routing.rs`).

This file records what KiCad itself considers a complete netclass, and which
fields may safely be omitted. The values below are taken from KiCad 10 source and
cross-checked byte-for-byte against a `.kicad_pro` written by KiCad 10.0.5.

## Why Completeness Matters

A netclass that exists but omits `wire_width` breaks schematic editing outright.
KiCad does not default the key, so junction dot size resolves from nothing:
Eeschema silently refuses to place junctions anywhere in the project, strips
existing ones on every save, and connectivity degrades without an ERC violation
or any visible error. Wires also plot with `stroke:none`.

An explicit `"wire_width": 0` is fine — `0` is KiCad's "use the default"
sentinel, the same convention as `(width 0)` on a wire stroke. The failure comes
from the key being absent, not from a zero value.

The chain, in `common/project/net_settings.cpp` unless noted:

| Step | Location | Behaviour |
|---|---|---|
| 1 | `net_settings.cpp:67` | Constructor seeds `m_defaultNetClass` via `NETCLASS(Default, true)`, so a project with no `net_settings` at all is fine. |
| 2 | `net_settings.cpp:133` | `readNetClass` builds every JSON class with `NETCLASS(name, false)` — no defaults. |
| 3 | `net_settings.cpp:168` | `wire_width` is applied only `if` the key is present. |
| 4 | `net_settings.cpp:216` | A parsed class that `IsDefault()` **replaces** the seeded default outright. |
| 4a | `netclass.h:96` | `IsDefault()` reads `m_isDefault`, which `SetName` sets only on an exact `aName == "Default"`. |
| 5 | `net_settings.cpp:1048` | `addMissingDefaults()` backfills other classes *from* the Default. Nothing backfills the Default itself. |

## It Is The Name, Not The Position

`saveNetclass` writes `m_defaultNetClass` first and the rest after
(`net_settings.cpp:194`), so in every `.kicad_pro` KiCad has written the Default
*is* `classes[0]`. That is an output convention, not a read rule. The loader
(`net_settings.cpp:209`) is a plain range-for with no index and no
first-iteration branch, and the only thing that marks a class as the default is
the exact name match in `SetName`. Confirmed empirically against KiCad 10.0.5: a
sparse class at `classes[0]` with a complete `Default` behind it plots
correctly, and a sparse class that is the sole entry with no `Default` at all
plots correctly too, because the seeded default is never replaced.

A writer must therefore key completeness off the name. Completing whichever
class comes first fixes the common case and leaves the bug live.

## Only The Default Class Must Be Complete

`addMissingDefaults()` fills the fields a netclass omits from the Default class, so
a non-Default class legitimately omitting fields simply inherits. In KiCad 10.0.6
it never fills two of the twelve below: `diff_pair_via_gap`, whose block is commented out
(`net_settings.cpp:1108-1114`), and `line_style`, which has no block. A class
without its own via gap has none, and one without a line style reads as solid
(`netclass.h:234`). The completeness requirement applies **only to the class named
`Default`**, because it is the root of that inheritance.

A writer therefore needs to emit a full field set for `Default`, but may leave a
named class partial. A reader must not treat absence on a non-Default class as
malformed — it means "inherits".

## Defaults

From `common/netclass.cpp:36-50`. Units are not uniform: the serialiser
(`net_settings.cpp:71`) writes schematic fields in **mils** and PCB fields in
**mm**.

| Key | Value | Unit | Constant |
|---|---|---|---|
| `wire_width` | `6` | mils | `DEFAULT_WIRE_WIDTH` |
| `bus_width` | `12` | mils | `DEFAULT_BUS_WIDTH` |
| `line_style` | `0` | enum (solid) | `DEFAULT_LINE_STYLE` |
| `clearance` | `0.2` | mm | `DEFAULT_CLEARANCE` |
| `track_width` | `0.2` | mm | `DEFAULT_TRACK_WIDTH` |
| `via_diameter` | `0.6` | mm | `DEFAULT_VIA_DIAMETER` |
| `via_drill` | `0.3` | mm | `DEFAULT_VIA_DRILL` |
| `microvia_diameter` | `0.3` | mm | `DEFAULT_UVIA_DIAMETER` |
| `microvia_drill` | `0.1` | mm | `DEFAULT_UVIA_DRILL` |
| `diff_pair_width` | `0.2` | mm | `DEFAULT_DIFF_PAIR_WIDTH` |
| `diff_pair_gap` | `0.25` | mm | `DEFAULT_DIFF_PAIR_GAP` |
| `diff_pair_via_gap` | `0.25` | mm | `DEFAULT_DIFF_PAIR_VIAGAP` |

These twelve are the inheritable set — `saveNetclass` writes each one
conditionally (`if( nc->HasWireWidth() )` and friends), so KiCad's own output
omits them on classes that do not set them.

## Always Written

`saveNetclass` emits these unconditionally, set or not:

| Key | Value |
|---|---|
| `name` | class name |
| `priority` | `2147483647` for `Default`, `-1` otherwise |
| `schematic_color` | `"rgba(0, 0, 0, 0.000)"` |
| `pcb_color` | `"rgba(0, 0, 0, 0.000)"` |
| `tuning_profile` | `""` |

`priority` comes from two places: `net_settings.cpp:69` sets
`std::numeric_limits<int>::max()` on the Default, and the NETCLASS constructor
(`netclass.cpp:57`) sets `-1` for every other class. KiCad treats the Default as
the lowest-priority fallback, so `2147483647` is not arbitrary.

## A Complete Default Class

```json
{
  "name": "Default",
  "priority": 2147483647,
  "schematic_color": "rgba(0, 0, 0, 0.000)",
  "pcb_color": "rgba(0, 0, 0, 0.000)",
  "tuning_profile": "",
  "wire_width": 6,
  "bus_width": 12,
  "line_style": 0,
  "clearance": 0.2,
  "track_width": 0.2,
  "via_diameter": 0.6,
  "via_drill": 0.3,
  "microvia_diameter": 0.3,
  "microvia_drill": 0.1,
  "diff_pair_width": 0.2,
  "diff_pair_gap": 0.25,
  "diff_pair_via_gap": 0.25
}
```

## Verifying Without KiCad

A project whose Default class lacks `wire_width` plots every wire with no
stroke, which is detectable headlessly:

```
kicad-cli sch export svg --output out --exclude-drawing-sheet <sheet>.kicad_sch
grep -c 'fill:none; stroke:none;' out/*.svg
```

`0` is healthy. `1` means every wire was plotted with no stroke.

Keep the `fill:none; ` prefix. It is what scopes the match to the wire group: a
junction dot is filled rather than stroked, so a perfectly healthy sheet
carries a bare `stroke:none` and grepping for that alone reports a passing plot
as a failing one.

The junction dot is the sharper signal, and the one that matches the reported
symptom. eeschema still emits the `<circle>`, it just collapses the radius:

| | Junction dot | Wire group |
|---|---|---|
| Healthy | `r="0.4572"` | `stroke:#009600; stroke-width:0.1524` |
| Incomplete Default | `r="0.0001"` | `fill:none; stroke:none;` |

0.4572 mm is 18 mils, three times the 6 mil wire, so the radius tracks the
resolved wire width and pinning it against a known-good baseline is a stronger
assertion than any literal. Both rows measured on KiCad 10.0.5.

## Source References

- `common/netclass.cpp:36-50` — the `DEFAULT_*` constants
- `common/netclass.cpp:52-80` — `NETCLASS::NETCLASS`, and what `aInitWithDefaults` gates
- `common/project/net_settings.cpp:71` — `saveNetclass`, including the unit split
- `common/project/net_settings.cpp:128` — `readNetClass`
- `common/project/net_settings.cpp:1048` — `addMissingDefaults`

Read from the GitHub mirror `KiCad/kicad-source-mirror`; upstream is GitLab at
`kicad/code/kicad`.
