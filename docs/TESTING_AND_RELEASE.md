# Testing And Release

The pull-request baseline is defined in `CONTRIBUTING.md`. Run the same commands
locally when the required platform dependencies are available:

```bash
cargo test --workspace --locked --lib --tests
cargo test --workspace --locked --doc
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo fmt --all -- --check
```

`protoc` and its well-known type includes are required by
`crates/konnect-ipc/build.rs`.

## Coverage Map

| Area | Source of tests |
|---|---|
| MCP protocol, CLI, and asset contracts | `crates/konnect/tests` and `crates/konnect/src` unit tests |
| Router and toolset invariants | `crates/konnect-core/src/router` tests |
| Dispatch and required-argument behavior | `crates/konnect-core/src/mcp/handler.rs` tests |
| Domain handlers and evidence rules | tests beside modules in `crates/konnect-core/src/tools` |
| S-expression parsing and atomic writes | `crates/konnect-sexp` tests |
| Typed schematic model | `crates/konnect-schematic-editor` tests |
| IPC builders, transport, and client behavior | `crates/konnect-ipc` tests |
| Real KiCad behavior | ignored live tests and `.github/workflows/e2e-kicad.yml` |
| Viewer | `crates/schematic-viewer` tests and its CI job |
| Python plugin | `plugin/tests` and the plugin CI job |
| PCM package | `packaging/validate-pcm.py` and packaging CI jobs |

The viewer is outside the Cargo workspace. Build or test it from
`crates/schematic-viewer`; workspace commands do not cover it.

## Evidence-Focused Regression Tests

When a tool returns a count, success state, or verdict, test the evidence behind
that field. The v0.7 reference cases include:

- complete DRC category parsing in `tools/cli.rs`;
- DRC-backed review/readiness decisions in `tools/design_review.rs` and
  `tools/manufacturing.rs`;
- footprint-type discrimination and post-commit read-back in
  `tools/pcb_sync.rs` and `konnect-ipc/src/builders.rs`;
- closed-board placement and flip refusal cases in `tools/pcb_components.rs`.

Use real KiCad-generated fixtures for formats KiCad owns. For an IPC path, unit
tests should prove request construction and failure classification; an ignored
live test or the end-to-end workflow should prove behavior that depends on a
running editor.

## CI And Live Validation

`.github/workflows/ci.yml` covers the Rust workspace, formatting, clippy,
documentation tests, viewer, plugin, Nix, and PCM validation. The live KiCad
workflow is separate because it requires an installed graphical application and
is not an ordinary per-PR gate.

In the PR description, list every command run and explicitly name checks skipped
because they require KiCad, another operating system, credentials, or release
infrastructure.

## Documentation

For a public behavior change, inspect README, `DEV.md`, `tool-directory.md`,
these developer maps, and bundled guidance under
`crates/konnect/assets/skills` and `assets/agents`. Tool-count locations and
their enforcement are defined by `CONTRIBUTING.md` and
`crates/konnect/tests/doc_tool_counts.rs`; link to the authoritative catalogue
instead of copying totals into new documents.

Every behavioral claim in these maps should name its source module. When the
module changes, a contributor can find the claim by searching for the old path.
Describe unimplemented future behavior explicitly as design intent rather than
current capability.

## Packaging And Release

Build the server with `cargo build --release -p konnect`. Build the viewer
separately when it is in scope. Changes to plugin files, binary layout, metadata,
icons, or release scripts require PCM assembly and
`packaging/validate-pcm.py` validation.

`packaging/build-pcm.ps1` and `build-pcm.sh` enumerate the files staged into the
PCM archive. Developer documentation stays in the repository and is not added
to release zips.
