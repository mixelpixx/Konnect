# ERC coordinate scale fixtures (issue #541, kicad#25582)

Five files, for the two sides of one upstream defect and the one build the
version boundary cannot see:

| File | What it is |
|---|---|
| `erc_coordinate_scale_kicad10_0_6.json` | KiCad 10.0.6's ERC JSON report — every coordinate 100× too small |
| `erc_coordinate_scale_kicad10_0_6.rpt` | KiCad 10.0.6's ERC *text* report of the same run — the coordinates are correct, so this is the oracle |
| `erc_coordinate_scale_kicad10_0_7.json` | The fixed JSON writer's own report, captured from a build of the 10.0 branch carrying the fix |
| `erc_coordinate_scale_kicad10_0_7.rpt` | That same run's text report — a second, independent oracle |
| `erc_coordinate_scale_kicad10_branch.json` | The same branch build unmodified: true coordinates, stamped `10.0.6` — the known limitation |

Every one of the five is kicad-cli's own output. Nothing is edited, derived or
hand-written.

## The defect

`ERC_REPORT::WriteJsonReport` built its `UNITS_PROVIDER` with `pcbIUScale`
(1e6 IU/mm) while the text writer used `schIUScale` (1e4 IU/mm), so the JSON
report divided every schematic coordinate by 100. Present since the JSON
report was added — `eeschema/erc_report.cpp` at tags `8.0.0` and `9.0.0` and
at branch `10.0` all carry the `pcbIUScale` line — and fixed by the one-line
[`407f6293`](https://gitlab.com/kicad/code/kicad/-/commit/407f6293b31e975b0f4284555c0ad46094ab69e4)
on master, cherry-picked to the 10.0 branch as `6d8e1fe` on 2026-09-21.
[kicad#25582](https://gitlab.com/kicad/code/kicad/-/issues/25582) closed the
same day, `status::fix-committed`, milestone **10.0.7**.

## Provenance of the 10.0.6 pair

Both files are kicad-cli's own output, written back to back on KiCad 10.0.6
(`kicad-cli --version` → `10.0.6`), Fedora Linux, from the committed
`single_pin_nets.kicad_sch` fixture and its child sheet (see
`single_pin_nets.README.md` for how that sheet was built):

```text
kicad-cli sch erc --units mm --format json   --severity-all -o erc_coordinate_scale_kicad10_0_6.json single_pin_nets.kicad_sch
kicad-cli sch erc --units mm --format report --severity-all -o erc_coordinate_scale_kicad10_0_6.rpt  single_pin_nets.kicad_sch
```

Nothing in either file is edited. `source` therefore reads
`single_pin_nets.kicad_sch`, and the two `date` stamps are five seconds apart.
That sheet is a hierarchy, so the pair also covers a child sheet and a
two-item violation.

## Oracle table

Every item, in report order. The text report is the oracle; the JSON report is
what Konnect is handed.

| # | Sheet | Item | JSON `pos` | Text report | Ratio |
|---|---|---|---|---|---|
| 1 | `/` | Label 'STUB' | `0.6985, 1.8034` | `69.85, 180.34` | 100 |
| 2 | `/` | Symbol #PWR001 Pin 1 | `0.7366, 1.0033` | `73.66, 100.33` | 100 |
| 3 | `/` | Horizontal Wire | `0.635, 1.8034` | `63.50, 180.34` | 100 |
| 4 | `/` | Label 'MIXED' | `0.6985, 1.6002` | `69.85, 160.02` | 100 |
| 5 | `/` | Label 'LONE' | `0.762, 0.5969` | `76.20, 59.69` | 100 |
| 6 | `/` | Global Label 'MIXED' | `0.762, 1.6002` | `76.20, 160.02` | 100 |
| 7 | `/` | Global Label 'MIXED' | `0.762, 1.6002` | `76.20, 160.02` | 100 |
| 8 | `/` | Label 'MIXED' | `0.6985, 1.6002` | `69.85, 160.02` | 100 |
| 9 | `/` | Horizontal Wire | `0.635, 1.8034` | `63.50, 180.34` | 100 |
| 10 | `/` | Label 'LONE' | `0.9525, 0.5969` | `95.25, 59.69` | 100 |
| 11 | `/` | Horizontal Wire | `0.9525, 0.5969` | `95.25, 59.69` | 100 |
| 12 | `/Child/` | Symbol #PWR001 Pin 1 | `0.508, 0.6731` | `50.80, 67.31` | 100 |

Items 6 and 7 are the two items of one `same_local_global_label` violation.
Item 12 is on the child sheet.

## The measurement inside a description is scaled too

The same units provider formats the item *text*, so on an affected version a
description carrying a dimension is wrong by the same factor: rows 3, 9 and 11
read `Horizontal Wire, length 0.1270 mm` in the JSON against
`Horizontal Wire, length 12.70 mm` in the text report. Konnect corrects
coordinates and leaves KiCad's prose alone, so this divergence survives in the
response — the `coordinates.reason` string says so.

## Provenance of the 10.0.7 pair

KiCad 10.0.7 is unreleased (milestone due 2026-09-30), so there is no released
binary to capture. The 10.0 branch already carries the fix, cherry-picked as
`6d8e1fe`, so the pair was captured from a build of that branch:

```text
git clone https://gitlab.com/kicad/code/kicad.git && git checkout origin/10.0
# ffdb16575ff6214bfd262efc0d5a592b27692780, which has 6d8e1fe as an ancestor
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DKICAD_BUILD_QA_TESTS=OFF -DKICAD_SCRIPTING_WXPYTHON=OFF \
      -DCMAKE_INSTALL_PREFIX=../install
ninja -C build && ninja -C build install
```

Fedora Linux, gcc, against the same committed `single_pin_nets.kicad_sch` and
the stock KiCad 10 symbol libraries (`KICAD10_SYMBOL_DIR=/usr/share/kicad/symbols`,
so that the 11 violations are the schematic's own and not library-resolution
noise):

```text
kicad-cli sch erc --units mm --format json   --severity-all -o erc_coordinate_scale_kicad10_0_7.json single_pin_nets.kicad_sch
kicad-cli sch erc --units mm --format report --severity-all -o erc_coordinate_scale_kicad10_0_7.rpt  single_pin_nets.kicad_sch
```

### The one thing that had to be set, and why

A build of the 10.0 branch stamps its own reports **`10.0.6`**, not `10.0.7`.
`ERC_REPORT::WriteJsonReport` fills `kicad_version` from
`GetMajorMinorPatchVersion()`, which resolves to `KICAD_MAJOR_MINOR_PATCH_VERSION`;
`cmake/BuildSteps/WriteVersionHeader.cmake` parses that out of
`KICAD_SEMANTIC_VERSION` in `cmake/KiCadVersion.cmake`, which upstream leaves at
the *last released* version — `10.0.6-unknown` — until the release commit bumps
it. `git describe` does not feed it; it only reaches `KICAD_VERSION_FULL`.

So the branch build was captured twice:

| Build | `KICAD_SEMANTIC_VERSION` | Reported `kicad_version` | sha256 of the JSON |
|---|---|---|---|
| unmodified branch | `10.0.6-unknown` (upstream's value) | `10.0.6` | `b778a2d684403ba137d47d42b7136e2af46518a440be69a77a2f39d2cfc91c28` |
| release-stamped | `10.0.7` | `10.0.7` | `68e494eb4a420f08bbb6d367194cd5d464ded40ce95b7860334c0c32cd5cf473` |

Both are committed: the second as
`erc_coordinate_scale_kicad10_0_7.json`, the first as
`erc_coordinate_scale_kicad10_branch.json`. The two reports are **byte-identical apart
from `date` and `kicad_version`** — every coordinate, description and violation
is the same computation, so the one-line version bump (exactly what upstream's
release commit does) changed the stamp and nothing else.

That is also a fact about the wild, not only about this fixture: anyone running
a self-built 10.0-branch KiCad between 10.0.6 and 10.0.7 gets *correct*
coordinates reported as `10.0.6`, and Konnect will scale them by 100. The
report carries nothing that separates such a build from the affected release,
so this is a limit of the version boundary rather than something the
implementation can detect. Released builds are unaffected.

Such builds are outside the supported contract: #541 covers released KiCad
builds only, by maintainer decision on that issue. The unmodified capture is
committed so that `an_unreleased_branch_build_stamped_as_the_affected_release_is_scaled`
pins the resulting behaviour: `corrected`, and every coordinate 100× the true
one. Its oracle is the 10.0.6 and 10.0.7 text reports, whose coordinates its
own match. No text report is committed beside it: the one left from that build
came from an earlier run without `KICAD10_SYMBOL_DIR`, so it is not the same
run as the JSON.

## The 10.0.7 pair against both oracles

Checked on capture, all twelve items:

| Check | Result |
|---|---|
| 10.0.7 JSON coordinates vs the **10.0.6 text report** | identical |
| 10.0.7 JSON coordinates vs its **own 10.0.7 text report** | identical |
| 10.0.7 JSON coordinates vs the unmodified branch build | identical |
| 10.0.7 JSON descriptions vs its own text report | identical |
| 10.0.7 JSON vs 10.0.6 JSON, coordinate by coordinate | exactly 100× |

The first row is the strongest: the 10.0.6 text report was written by a
different binary, months earlier, by a writer the fix did not touch, and the
fixed JSON writer independently agrees with it on every number.

Replace this pair with a capture from the released 10.0.7 once it ships; the
tests read both files by name and need no other change.
