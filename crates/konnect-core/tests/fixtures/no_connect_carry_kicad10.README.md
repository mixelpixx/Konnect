# No-connect carry fixture

`no_connect_carry_kicad10.kicad_sch` carries the three ways a placement change
can meet a no-connect marker, so the shared carry contract can be checked
against an answer that is not its own (#626).

## Provenance

Built through Konnect against KiCad's stock `Device` library —
`create_schematic`, `add_wire`, `add_schematic_net_label`,
`add_schematic_component`, `add_no_connect` — then parsed and force-resaved by
KiCad 10.0.6 on Fedora:

```text
kicad-cli sch upgrade --force no_connect_carry_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`, KiCad's own library
records, field positions and UUIDs.

Nothing in the file is hand-placed. Every marker was written by
`add_no_connect` at a coordinate the tools had already put a pin on.

## Cases

`Device:R` puts its pins at `y ∓ 3.81` when it stands at 0°, and at `x ∓ 3.81`
once it is turned to 90°, pin 1 first.

| Item | Where | Role |
|---|---|---|
| the NETB wire | `(144.78, 160.02) -> (156.21, 160.02)` | the net a protected pin can be wired into |
| `R4` | `(144.78, 163.83)`, pin 1 on the wire's **end** | anchors NETB so it survives in the netlist |
| `R2` | `(158.75, 160.02)`, pin 1 at `(158.75, 156.21)`, clear of the wire | the pin the marker protects |
| marker `1a251b9a` | `(158.75, 156.21)` | on `R2` pin 1 |
| marker `493fbe64` | `(180.34, 120.65)` | on nothing — already dangling |
| `R3` / `R5` | `(114.3, 101.6)` / `(114.3, 109.22)` | `R3` pin 2 and `R5` pin 1 stack on `(114.3, 105.41)` |
| marker `3627efa6` | `(114.3, 105.41)` | under two pins, so it belongs to neither alone |

| Change | What moves | Expected |
|---|---|---|
| `R2` -> `(154.94, 163.83)` | pin 1 lands at `(154.94, 160.02)`, mid-span on the NETB wire | the marker follows to `(154.94, 160.02)`; no dot is added |
| `R2` -> 90° | pin 1 lands at `(154.94, 160.02)`, the same mid-span point | the marker follows there too |
| `R2` shifted by `(-3.81, +3.81)` | the same arrival, reached through the bulk shift | the marker follows there too |
| `R3` or `R5` moved | one of the two stacked pins leaves `(114.3, 105.41)` | refused before any write: the marker maps to two arrival points |
| `R4` -> `(139.7, 175.26)` | pin 1 leaves the wire end; no marker is on it | nothing is carried, and `493fbe64` stays at `(180.34, 120.65)` |

The three placement tools reach the same arrival point by different routes —
`move_schematic_component` through the typed model, `rotate_schematic_component`
through a rotation delta, `bulk_move_schematic_components` through S-expression
edits — so one geometry checks all three against the same oracle rows.

`R4` sits on a wire *end*, which needs no dot — it is there so NETB keeps a pin
whatever happens to `R2`, and so a placement change that carries nothing is
distinguishable from one that carries something.

## KiCad's own answer

The net each pin reaches, and the type KiCad gives that pin, are KiCad's — read
off `kicad-cli sch export netlist` run on the file as it stands after the named
change. `no fix` is the same change with the carry bypassed, which is the
behavior this fixture exists to rule out:

```text
kicad-cli sch export netlist --output out.net no_connect_carry_kicad10.kicad_sch
```

| File | `/NETB` | `R2` pin 1 `pintype` |
|---|---|---|
| the fixture | R4.1 | `passive+no_connect` |
| `R2` moved | R4.1 | `passive+no_connect` |
| `R2` moved, no fix | **R2.1, R4.1** | `passive` |
| `R2` -> 90° | R4.1 | `passive+no_connect` |
| `R2` -> 90°, no fix | R4.1 | `passive` |
| `R2` shifted | R4.1 | `passive+no_connect` |
| `R2` shifted, no fix | **R2.1, R4.1** | `passive` |

The load-bearing rows are the third and the last. Without the carry the marker is stranded at
`(158.75, 156.21)`, the junction pass sees an unprotected pin mid-span on a
wire and writes a dot, and KiCad puts the pin the user declared unconnected on
`/NETB`. A wrong implementation cannot produce both that row and the one above
it.

The `pintype` column is the other half, and it is what says the marker is
attached to the pin rather than merely somewhere on the sheet: KiCad writes
`passive+no_connect` only while a marker sits on that pin's endpoint.

## ERC

`kicad-cli sch erc --severity-all` finds 8 violations on the fixture, all of
them the cost of a sheet built out of deliberate stubs: 4 `pin_not_connected`,
1 `unconnected_wire_endpoint`, 1 `isolated_pin_label` (NETB reaches one pin),
1 `no_connect_dangling` (marker `493fbe64`, which is dangling on purpose), and
1 `no_connect_connected` (marker `3627efa6`, which is the stacked-pin case).

| File | violations |
|---|---|
| `R2` moved | 8 — unchanged: the marker travels with its pin |
| `R2` moved, no fix | 8, but **2** `no_connect_dangling` and no `isolated_pin_label` — the marker is stranded and the pin has joined NETB |
| `R2` -> 90° | 8 — unchanged |
| `R2` -> 90°, no fix | 10, with **2** `no_connect_dangling` and a fifth `pin_not_connected` |
| `R2` shifted | 8 — unchanged |
| `R2` shifted, no fix | 8, with the same **2** `no_connect_dangling` as the moved-no-fix row |

ERC reports the stranded marker, which is how #626 was noticed. What it never
reports is the other half — that the pin the marker used to protect has joined
a net — which is why the netlist table above is the primary oracle.
