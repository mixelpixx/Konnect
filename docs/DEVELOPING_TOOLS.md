# Developing Tools

Use this checklist when adding or changing an MCP tool. Start with
`CONTRIBUTING.md`, `docs/NAMING_CONVENTIONS.md`, and the related handler module;
the safest implementation usually follows an existing neighboring tool.

## Choose The Backend And Gate

| Operation | Preferred path | Required safety gate |
|---|---|---|
| Saved schematic mutation | `konnect-schematic-editor` or `konnect-sexp` | Target editor-lock refusal plus revision-aware atomic write; transaction journal for multi-file changes |
| Live board mutation | `konnect-ipc` | `KiCadIpcClient::ensure_board_is_active` before sending the mutation |
| Board mutation with a safe closed-file implementation | IPC first, file fallback | `attempt_ipc_write` in `tools/pcb_board.rs`; its observed editor state and lock checks authorize fallback, not transport failure alone |
| Board file mutation with no IPC implementation | `konnect-sexp` or focused parser/edit code | `refuse_if_board_open_in_kicad` before touching the file |
| Project settings (`.kicad_pro`) mutation | Existing project-settings writers | Existing board guard before reading/writing; require a closed project workflow and consult the ownership limitations below |
| KiCad-supported check or export | `kicad-cli` through `tools/cli.rs` | Validate exit status, output existence, and parsed result coverage |

Do not fall back to a file edit after a request reached KiCad and was rejected or
timed out. `konnect-ipc` classifies that differently from an unreachable
transport because KiCad may already hold or have changed the target document.

Read [file ownership before mutation](KICAD_INTEGRATION.md#file-ownership-before-mutation)
when changing a file writer. Check every target, including sibling project files,
against the programs that can save it; an atomic write is not an editor lock.
For a shared defect, complete the bounded
[same-class sweep](../GOVERNANCE.md#same-class-defect-review) rather than copying
the nearest handler's assumptions unchecked.

## Define And Implement The Tool

1. Add the definition to the related `tools()` vector under
   `crates/konnect-core/src/tools`.
2. Implement the handler near related handlers and reuse their target checks and
   response conventions.
3. Use valid Draft 2020-12 JSON Schema, list every required field, declare
   bounds, and set `additionalProperties: false` only when the object is
   intentionally closed. The catalogue compiles and caches this schema; MCP
   dispatch enforces exactly what is advertised.
4. Read required values with `require_*` or `get_path`, and malformed-present
   options with a checked optional helper. Dispatch validation does not replace
   direct-handler validation. Never turn a malformed present value into an
   omitted value or silent default.
5. Validate the target and all preconditions before the first mutation.
6. Use `CallToolResult::json`, `text`, `image`, or `error_kind` as appropriate.
7. Update `router/registry.rs`, `tool-directory.md`, and the guarded documentation
   named by `CONTRIBUTING.md`.

For an IPC write that accepts a layer name, use `try_layer_from_name` so an
unknown layer is refused before it becomes an invalid KiCad message.

## Evidence Rules

Treat success, counts, and verdicts as claims that require evidence.

- Derive response fields from the parsed result, committed state, or read-back;
  do not populate them from the request merely because the call returned.
- A positive review/manufacturing verdict requires every prerequisite check. If
  a required check could not run, return an incomplete result with the missing
  evidence named.
- Preserve all categories returned by an external tool. The DRC parser in
  `tools/cli.rs` must retain design-rule violations, unconnected items, and
  schematic parity because each category can independently make a board fail.
- After a risky IPC transformation, compare a bounded post-state with what was
  sent. `tools/pcb_sync.rs` is the reference for a commit plus read-back check.

These rules address failures where the transport and KiCad both returned
success but the resulting board was wrong, and where a verdict appeared clean
only because one result category was ignored.

## Errors And Compatibility

Use stable structured errors for caller-actionable failures: wrong arguments,
missing files, conflicts, inactive toolsets, and similar conditions. Do not
classify failures by matching display strings when a typed marker or enum can
cross the layer boundary.

Treat MCP tool names, schema fields, CLI flags, config keys, environment
variables, and documented paths as public API. Renames require compatibility or
an explicit migration note.

## Tests

Add the lowest-level test that can prove the risky behavior, then add handler or
protocol coverage when the public contract is involved:

- parser/writer logic in `konnect-sexp`;
- typed model behavior in `konnect-schematic-editor`;
- handler and response behavior in `konnect-core`;
- IPC message construction/classification in `konnect-ipc`;
- MCP/CLI behavior in `crates/konnect/tests`.

Fixtures that parse boards, footprints, symbols, or KiCad reports should come
from real KiCad output. Synthetic fixtures are useful for narrow grammar tests,
but they omit optional or positionless forms that real KiCad emits. Minimize a
real fixture only after the failing shape remains represented.

For a mutating tool, cover the refusal path before the success path: wrong open
board, reachable IPC rejection, stale file revision, unsupported geometry, or
missing evidence. For IPC behavior that mocks cannot prove, add an ignored live
test and record the live validation command in the PR.
