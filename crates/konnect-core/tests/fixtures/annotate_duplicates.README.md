# annotate_duplicates fixtures (#454)

Three saves of one schematic, all written by KiCad 10.0.5, used to pin
`annotate_schematic` to eeschema's own behaviour rather than to an assumption.

- `annotate_duplicates.kicad_sch` — built through Konnect's `create_project`
  and `add_schematic_component` (three `Device:R` all named `R1`: 1k at
  x=101.6, 2k at x=50.8, 3k at x=127.0; two `Device:C` named `C?`: 100n at
  x=139.7, 1u at x=63.5), then re-serialised by `kicad-cli sch upgrade` so
  the bytes are KiCad's. A hand-written version of the same schematic loaded
  in KiCad but netlisted zero components, which is why this one was not
  written by hand.
- `annotate_duplicates.kicad_keep_existing.kicad_sch` — the file above after
  eeschema's Tools → Annotate Schematic with its defaults (entire schematic,
  sort by X, keep existing annotations, first free number after 0). eeschema
  reported `Annotated 1u as C1.`, `Annotated 100n as C2.`, and twice
  `Error: Duplicate items R1`. Both the `Reference` property and the
  `(instances …)` block were written for the C's; every `R1` was left alone.
- `annotate_duplicates.kicad_reset.kicad_sch` — the keep-existing file after
  eeschema's "Reset existing annotations": `Updated 1k from R1 to R2.`,
  `Updated 3k from R1 to R3.`, `Annotation complete.` The leftmost `R1` (2k)
  kept its number.

`multichannel_channel_strip.kicad_sch` is KiCad's own `multichannel` demo
child sheet, verbatim: its parent instantiates it four times, so every symbol
carries four `(instances …)` paths. It pins per-sheet-instance numbering.
`check_schematic_overlaps` also finds one overlap in it, C12 (at
`(208.28, 67.31)`) over IC2's unit 2 (at `(213.36, 73.66)`), which reports
7.62 × 0.762 mm and not `0.7620000000000005` (#747).

Byte evidence: this directory is `-text` in `.gitattributes`; never
eol-normalise these files.

`multichannel_mixer.kicad_sch` and `multichannel_mixer.kicad_pro` are the
root sheet and project file of the same KiCad 10.0.5 `multichannel` demo,
verbatim. Together with the child sheet they let a test prove that a child
annotated on its own reserves the numbers the root already owns (`R1`..`R3`)
through the project's sheet tree.
