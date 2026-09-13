# Interactive architecture diagrams

These five externally published diagrams provide focused visual explanations of
Konnect's architecture and safety boundaries:

- [Diagram landing page](https://neusse.github.io/konnect-archify-tool/)
  provides one index for the complete published set.

- [Runtime architecture](https://neusse.github.io/konnect-archify-tool/konnect-runtime.html)
  follows an MCP request through dispatch, on-demand tool loading, KiCad
  mutation backends, and independent command-line checks.
- [Guarded PCB mutation](https://neusse.github.io/konnect-archify-tool/konnect-guarded-pcb-mutation.html)
  shows the live IPC path, the bounded closed-board file fallback, and the
  fail-closed refusal path.
- [Live PCB tool-call sequence](https://neusse.github.io/konnect-archify-tool/konnect-live-pcb-tool-call.html)
  traces one request from the MCP client through active-board verification,
  commit, readback, and return.
- [Manufacturing evidence flow](https://neusse.github.io/konnect-archify-tool/konnect-manufacturing-evidence.html)
  separates design sources, ERC/DRC/BOM evidence, the release verdict, package
  generation, and fabrication-house model review.
- [Schematic transaction lifecycle](https://neusse.github.io/konnect-archify-tool/konnect-schematic-transaction.html)
  explains journal-before-write, interruption recovery, divergence, and
  explicit abandonment.

The links open rendered HTML pages hosted by GitHub Pages. The viewer provides
light/dark themes, guided views, search, focus, pan/zoom, presentation mode, and
image export. It may request the JetBrains Mono font from Google Fonts; the
diagrams remain usable with a fallback font if that request is unavailable.

## Provenance

The five-diagram set was refreshed for the pinned Konnect revision in
[`neusse/konnect-archify-tool`](https://github.com/neusse/konnect-archify-tool)
at commit
[`25c43ad8e9543b99875b3a4bb6708577ecf6cc88`](https://github.com/neusse/konnect-archify-tool/commit/25c43ad8e9543b99875b3a4bb6708577ecf6cc88)
after reviewing Konnect commit
[`f0f5ad045c97f02f51d1f54efd3965a1aa4e4215`](https://github.com/mixelpixx/Konnect/commit/f0f5ad045c97f02f51d1f54efd3965a1aa4e4215).
GitHub Pages publication was introduced at
[`f931a8243f2dc0276bf364f56c007d9bd81972fc`](https://github.com/neusse/konnect-archify-tool/commit/f931a8243f2dc0276bf364f56c007d9bd81972fc).
The currently published artifact set, landing page, canonical upstream source
links, and current Pages workflow are recorded at
[`8e82e36e45bd28b064dc7aa634640ef53832c63d`](https://github.com/neusse/konnect-archify-tool/commit/8e82e36e45bd28b064dc7aa634640ef53832c63d).

The standalone repository contains the typed sources, pinned Archify v2.16.0
tooling, validation and delivery receipts, visual evidence, generated HTML, and
the reusable `konnect-archify-refresh` Codex skill. None of those generated or
tooling files are copied into Konnect.

The runtime architecture passes Archify's strict browser containment check at
all tested desktop sizes. The four detailed diagrams retain their raw
vertical-overflow findings and use a documented readable-scroll baseline; their
readability, viewer controls, and screenshot captures pass. This is a
disclosure, not a claim that those raw visual checks passed.

## Maintenance

Regeneration and publication happen in the standalone tool repository. Its
refresh skill compares the previously pinned Konnect revision with the current
target, reviews behavior relevant to the five stable reader questions, updates
the affected typed sources, validates and delivers every artifact, checks
hashes, and captures light/dark browser evidence. A successful push of delivered
HTML to its `main` branch publishes the pages through its GitHub Pages workflow.

When updating these links:

1. Run the refresh in `konnect-archify-tool` against the intended Konnect
   commit.
2. Review every changed diagram and its visual evidence.
3. Commit and publish the delivered HTML in that standalone repository.
4. Verify every public URL returns the refreshed page.
5. Update the two source revisions above when their content changes.

## Verification boundary

The diagrams document software behavior; they do not replace KiCad rendering,
ERC, DRC, connectivity inspection, PCB layout-physics review, or manufacturing
acceptance. A structurally complete package and clean automated checks do not by
themselves validate fabrication-house component models or physical placement.
