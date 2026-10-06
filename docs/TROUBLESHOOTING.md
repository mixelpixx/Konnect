# Troubleshooting

## JLCPCB assembly cannot obtain native midpoint geometry

Open the exact requested `.kicad_pcb` in PCB Editor and enable KiCad IPC.
JLCPCB assembly export requires native per-pad bounding boxes; a saved file or
another open board is not a substitute. Stop editing during export. A missing
box or changing live snapshot refuses the attempt before output creation;
retry after correcting the reported cause. The tool does not save or modify
your board. Generic and fabrication-only exports retain file-based behavior.

If positions move after upgrading, review old anchor-compensation offsets in
your correction policy. Midpoints now precede user corrections, so old
workarounds can double-correct them. See [JLCPCB coordinates and evidence](JLCPCB_CPL_CORRECTIONS.md).
Always inspect the fabricator's placement preview before ordering.

## IPC operation returned `ipc_outcome_unknown`

Stop automatic retries. Konnect sent a request but could not establish its
outcome, or a transaction's attempted rollback failed. The live board may
contain some or all of the requested changes; this is not proof that nothing
happened. The structured error reports `board_state: unknown` and
`retry_safe: false`.

Inspect the requested board in KiCad and reconcile what was actually applied
before repeating any mutation. Do not edit the saved file as a workaround:
it may be older than the live editor state. Keep the full diagnostic, including
any commit identifier, when reporting the failure. Bounded recovery is only
available for schematic-to-PCB sync when the original operation answers within
its additional recovery window. An unknown result does not establish that an
undo will reverse a pending, unpublished commit.

## Schematic sync returned `ipc_batch_recovered`

The apply was aborted after a late IPC reply, not published. Konnect confirmed
the known commit was dropped and compared a fresh serialized live snapshot
with the exact target's pre-apply snapshot. This reports `board_state: unchanged`,
`drop_confirmed: true`, and `retry_safe: false`. Review the live board and run a
fresh dry run before deciding whether to apply again; do not replay the old
apply automatically or edit the saved file as a workaround.

Sync allows up to 30 additional seconds after an ordinary 30-second receive
window. The remaining allowance is shared by rollback and verification, not
renewed at each step. Missing or late publish replies cannot be rolled back
automatically; failed verification still reports `ipc_outcome_unknown`.

Sync creates are sent in ordered groups of at most 32 footprints, with every
group and the updates inside one KiCad commit. A successful apply is one Undo
entry, not one entry per group. This bounds the item count, not the serialized
request bytes: it does not establish KiCad's listener size limit or eliminate
timeouts. A rejected or uncertain group stops the apply; Konnect never retries
it or publishes a partial group sequence automatically.

## Which Konnect binary is this client using?

Call the always-visible `get_installation_info` tool in the affected MCP
session. The result comes from the process serving that call and includes its
version, build commit when available, executable path, conservatively detected
install source, the version produced by the binary currently on disk at that
same path, KiCad CLI version, redacted IPC endpoint, and restart guidance.

`installation.binary_on_disk.newer_than_running: true` is reported only when
both stable versions can be parsed and the on-disk binary is newer. `null`
means the comparison could not be proven, not that the process is current.
Likewise, `installation.source: "unknown"` means no trusted package manifest
identified the channel; Konnect does not guess from directory names. Endpoint
credentials and query or fragment data are redacted.

Follow the returned platform-specific guidance, restart the MCP client (and
KiCad when it owns the server process), then call `get_installation_info` again
to verify the process that actually restarted. This diagnostic writes nothing.

## Installed guidance is out of date

`konnect init` writes skills, agents, and Claude hooks once. Upgrading Konnect
does not touch them, so an older install keeps advising the old tools. Run
`konnect status` (add `--client codex` for Codex) with the binary your MCP
client uses. Each file and hook is compared with that binary's bundle:

| Status | Meaning |
|--------|---------|
| `current` | Byte-identical to the bundle, or the exact hook command `init` writes |
| `different` | Present, but not identical. It may be an older release's copy or your own edit; the install marker records only a version, so the two cannot be told apart |
| `missing` | Not present |
| `unreadable` | Present but could not be read, or `settings.json` is not valid JSON or not the shape Claude expects |

A `different` hook carries a reason: `legacy_handler` is the pre-hook-JSON
`… skill <name>` form, which Claude ignores, and `other_executable` is a hook
that runs another Konnect binary, written exactly as `konnect init` writes it.

The serving process makes the same comparison for both clients once per
installed guidance version, meaning each install marker. The first start
for that marker scans, records the resulting state in
`~/.konnect/.guidance-checked-<client>`, and sets the MCP `initialize`
`instructions` to a one-line notice when either client is `out_of_sync`. Later
starts read the record instead of rescanning and give no notice. Running
`konnect init` starts a new check. Upgrading the binary alone does not. `get_installation_info` reports the
result under `guidance`; its `checked` field is `now` for a scan in this
process, with per-file detail, or `earlier` for a recorded state without it.
`konnect status` always scans. Nothing rewrites guidance. Run `konnect init` to
update. It overwrites `different` files, so save any edits you made first.

## "KiCAD IPC socket path not configured"

Any tool that talks to a live KiCAD session (`save_project`, PCB editing,
`check_kicad_ui`, …) needs the IPC socket address.

**At startup** — once, and only then — Konnect resolves it in order: the
`ipc_address` in your config, then `KICAD_API_SOCKET` (set only for plugins
KiCad launches itself), then the platform default — `<temp dir>/kicad/api.sock`,
used only if something is actually listening there. The startup log on stderr
says which it picked, and warns when nothing was found.

So **if KiCad is already running with the API enabled when Konnect starts**,
no configuration is needed on any platform. That order matters and is easy to
get wrong: an MCP client normally launches the Konnect server itself, before
you open KiCad. Konnect does not re-probe afterwards, so a server started first
stays unresolved for its whole life no matter what you open later. Start KiCad
first, or restart the Konnect server (in most clients, reconnect the MCP
server) once KiCad is up — or set `ipc_address` explicitly, which never depends
on ordering.

Where the probe looks differs by platform, because the endpoint does. On Linux
and macOS the socket is a filesystem entry and its metadata is read. On Windows
KiCad's `ipc://` endpoint is a **named pipe**, which has no filesystem presence
at all, so the same path is looked up in the pipe namespace (`\\.\pipe\`)
instead. Before that was true, Windows detected nothing and every setup had to
be configured by hand (#529); if you are on a Konnect older than that fix, set
`ipc_address` or `KICAD_API_SOCKET` explicitly.

If the address is unresolved, both of the following must be correct — neither
happens automatically:

1. **The socket path in Konnect's plugin settings** (inside KiCAD)
2. **The Konnect server registration in your AI client's MCP config**

Step by step (based on the diagnostic guide contributed in
[#18](https://github.com/mixelpixx/Konnect/issues/18)):

1. Open KiCAD normally.
2. Go to **Edit → Preferences → Plugins** and check **"Enable KiCad API"**.
   Confirm a line like this appears:

   ```
   Listening on ipc://C:\Users\<you>\AppData\Local\Temp\kicad\api.sock
   ```

   Copy the whole address including the `ipc://` prefix — it is unique to
   your machine and user.
3. In KiCAD, open **Tools → External Plugins → Konnect** to open the settings
   dialog.
4. Paste the address into the **IPC Socket** field and click **Save**.
5. Confirm your AI client (Claude Code, Claude Desktop, …) has the `konnect`
   MCP server registered in its own config (`.mcp.json` or
   `claude_desktop_config.json`) pointing at the `konnect` binary — see
   [examples/](../examples/). This registration is separate from the KiCAD
   plugin settings.
6. Restart the AI client session so it spawns a fresh Konnect process that
   reads the saved settings.
7. Verify: have the AI call `open_project`. Expected:

   ```json
   { "kicad_ui_running": true, "message": "KiCAD is running and IPC is available." }
   ```

Alternative: launching the server from within KiCAD sets `KICAD_API_SOCKET`
automatically, and a `konnect-settings.json` passed via `--config` can carry
`ipc_socket_path` directly.

## PCB tools return "KiCAD must be running with the board loaded"

The IPC tools talk to KiCAD's **running PCB editor**. Open your board file in
KiCAD first, and make sure the API is enabled (previous section).

That message means the transport was unreachable. If KiCAD *is* running with
that board open and a tool still refuses, the error you get back is the tool's
own reason — "a polygon needs at least 3 points", "requested board … is not open
in KiCAD" — and it names what to change about the request.

## KiCad is running, but `ipc_failure` says it did not answer

`check_kicad_ui` and `open_project` report `ipc_failure` as
`{ "kind", "message" }` whenever they establish why KiCad did not answer their
Ping. It is `null` when no kind was established: the Ping succeeded with
`AS_OK`, or
`check_kicad_ui`'s own timeout expired first, which it reports as
`timed_out: true`. The kind comes from the error NNG returned, so it tells
apart cases that used to share one `false`, and each has a different fix:

| `kind` | What happened | What to do |
|---|---|---|
| `not_configured` | No address was configured or discovered. | See ["KiCAD IPC socket path not configured"](#kicad-ipc-socket-path-not-configured). |
| `no_listener` | Nothing is listening at the address. | Open an editor with the API enabled, and check the address against KiCad's "Listening on" line. |
| `access_denied` | The operating system refused this account access to the endpoint. Likely a different account or a restrictive ACL. | Run Konnect as the same operating-system user as KiCad; see below. |
| `handshake_failed` | A listener accepted the connection and did not complete NNG's handshake. | Another program holds KiCad's address ([#531](https://github.com/mixelpixx/Konnect/issues/531)). Close it, then restart KiCad so it can bind. |
| `transport_error` | Any other dial or send failure. | Read `message`. |
| `request_failed` | The request did not complete and may have reached the endpoint. A KiCad status in `message` (for example `AS_NOT_READY` while an editor is still loading) proves it did; a receive timeout or a malformed reply does not. | With a KiCad status: wait for the editor to finish loading, then retry. Without one: treat the endpoint as unproven and check what holds the address. |

`handshake_failed` takes NNG's fixed 10-second negotiation limit to appear.
That is longer than `check_kicad_ui`'s default 5-second budget, which then
reports `timed_out` instead, so pass `timeout_seconds` above 10 to see it.

### `access_denied` from a sandboxed AI client

Some AI clients run tool commands in a sandbox under a separate
operating-system account. On Windows, Codex does
([#300](https://github.com/mixelpixx/Konnect/issues/300),
[#532](https://github.com/mixelpixx/Konnect/issues/532)). The refusal itself
proves only that the operating system denied this account. The likely cause in
that setup: KiCad's endpoint was created by the desktop user, and a Windows
named pipe created without an explicit security descriptor gives full control
only to its creator and read access to everyone else, while a request needs
both read and write. A Konnect started inside the sandbox is then refused,
however its address is configured. A restrictive ACL on the pipe produces the
same kind.

The known workaround: run Konnect outside the sandbox, as the same user as
KiCad, over HTTP:

```toml
# konnect.toml in the directory Konnect is started from
transport = "http"
http_address = "127.0.0.1:3000"
ipc_address = "ipc://C:/Users/<you>/AppData/Local/Temp/kicad/api.sock"
```

Start `konnect` from a normal terminal and point the client at
`http://127.0.0.1:3000/mcp`; `http://127.0.0.1:3000/health` answers `ok` while
the server is up. Keep the address on `127.0.0.1`: the HTTP transport has no
authentication. KiCad needs no change.

## Tools answer from the file while KiCad is open

Tools that read the board IPC-first report `"source": "ipc"` or `"file"`, and a
`file` answer means the live board was never consulted — unsaved changes are
missing from it. Those tools go through one shared IPC helper, and it logs a
`WARN` on stderr each time the transport could not be reached, naming the
address it tried; check that against the address KiCad reports as "Listening
on".

The warning covers that helper, which is every tool that falls back to a file.
It is not a global "IPC failed" log: the health tools (`check_kicad_ui`,
`launch_kicad_ui`) and `get_project_info` dial KiCad directly and report the
outcome in their own response — `ipc_responsive`, `connected` — rather than
falling back to anything, so read those fields instead of looking for a
warning. `check_kicad_ui` and `open_project` also say why in `ipc_failure`
(previous section). A KiCad that *answered and refused* is not warned about anywhere; that
is a tool error, and it says so.

## Live schematic navigation on KiCad 10: `schematic_editor_document_identity`

KiCad 10.x's IPC API covers the PCB editor; the schematic editor arrives with
KiCad 11. A KiCad 10 schematic editor still answers the open-documents query,
but with the sheet's file name only: no project and no sheet path. Konnect
cannot address a schematic without both and does not guess them from the
saved file, the board or your arguments. So on KiCad 10:

- `get_editor_state` reports the schematic editor with `addressable: false`,
  no documents, and an `unavailable_reason` naming your KiCad version. The PCB
  editor is reported as usual.
- `get_editor_selection`, `mutate_editor_selection`,
  `resolve_navigation_target` and `resolve_cross_probe_target` refuse a
  schematic request with `unsupported_capability`,
  `capability: "schematic_editor_document_identity"`, and your KiCad version
  in `kicad_version`. That field is `null` from KiCad 10's standalone
  schematic editor, which does not report its version.

This is a limit of the KiCad version, not a stale or changed document, so
retrying will not help. PCB navigation works on KiCad 10. Every file-based
schematic tool is unaffected. Live schematic navigation needs KiCad 11 or
later; KiCad's 10.99 development builds already report the identity.

## "layer 'X' has no KiCAD board layer this build can represent"

The footprint or request names a layer this build cannot map, so the request was
refused before anything was sent. Nothing on the board changed.

This refusal exists because the alternative is worse. KiCAD 10.0.5 does not
validate the layer field on an incoming item, so an unrepresentable value used
to reach it and **terminate the process**, discarding any unsaved board
([#237](https://github.com/mixelpixx/Konnect/issues/237)). Konnect now stops at
its own boundary instead.

Every layer a KiCAD 10 footprint can legally draw on is supported, including
`Dwgs.User`, `Cmts.User`, `Eco1/2.User`, `F/B.Adhes`, `Margin`, `Rescue`,
`In1.Cu`–`In30.Cu` and `User.1`–`User.45`. If you hit this on stock library
content, that is a bug worth reporting with the footprint name — the message
names the layer and the item.

**If you are on v0.6.0 or earlier**, placing
`Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal` or
`Connector:BJB_Pico_46.110.1001_Receptacle_Horizontal` can kill KiCAD outright.
Update to v0.6.1 or later.

## `unsafe_file_fallback` after KiCad disappears

Konnect remembers each board it positively observes open through IPC during the
current server process. If IPC later becomes unreachable, a board-file mutation
for that same board fails with `error.kind: "unsafe_file_fallback"` instead of
editing the saved file. KiCad may have crashed or been force-quit with unsaved
state, so the saved `.kicad_pcb` is not known to be authoritative. The error
confirms that Konnect left it unchanged
([#240](https://github.com/mixelpixx/Konnect/issues/240)).

KiCad running with no PCB editor open is *not* this state: nothing was ever
identified, so a `board_source` read answers from the saved file and says
`no_pcb_editor_at_endpoint`. The refusal below is only for a board this server
did observe live.

Read-only tools that take a `board_source` selector report the same kind for the
same reason, and nothing was going to be written: under the default
`board_source: "auto"` they refuse rather than present a possibly stale file as
the board's current state. The recovery below applies, and inspecting the
snapshot deliberately — `board_source: "saved"`, which answers and states its
freshness limitation — is also available there.

Recover deliberately:

1. Reopen or recover the board in KiCad.
2. Reconcile any recovered/unsaved work and save the authoritative board.
3. Continue through live IPC.

If KiCad was intentionally closed cleanly and closed-board mode is desired,
first confirm that the saved file is authoritative, then restart Konnect to
begin a new server session. Repeating the tool call does not clear the safety
memory, and an agent must not restart Konnect or edit `.kicad_pcb` directly to
bypass the refusal.

This memory is intentionally process-local. It cannot detect a KiCad crash that
happened before the current Konnect process started. File-fallback success
therefore carries a warning describing that cold-start limitation.

## `invalid_configuration` from a config tool

`load_user_config`, `get_effective_config` or a save refuses with
`error.kind: "invalid_configuration"` when a Konnect preferences file exists
but cannot be used. `error.path` names the file and `error.reason` starts with
`malformed_json:` (invalid JSON, or a root that is not an object — often a
truncated write or a hand edit) or `unreadable:` (permissions, a directory at
that path, invalid UTF-8).

Konnect used to answer with its defaults here, and the next save then replaced
the file with them. It now refuses and writes nothing, so your settings are
still in the file. Open it, fix the JSON (or move it aside to start again from
the defaults), and retry. The user file is `%APPDATA%\konnect\config.json` on
Windows, `~/Library/Application Support/konnect/config.json` on macOS and
`~/.konnect/config.json` elsewhere; a project's is
`<project_dir>/.konnect/project.json`.

A save that ends in `mutation_outcome_uncertain` could not prove what is on
disk. Read the file before retrying; do not assume nothing was written.

## An older schematic-to-PCB sync left extra unnamed pads

Konnect versions v0.4.0 through v0.6.1 could rewrite each drawing shape inside
a footprint as an anonymous pad while `update_pcb_from_schematic` reassigned
pad nets ([#244](https://github.com/mixelpixx/Konnect/issues/244)). Current
versions prevent and detect that corruption, but prevention does not repair a
board already saved by an affected release.

Open the affected board in KiCAD, load `pcb_components`, and call
`repair_corrupted_footprints` with the board path. Its default dry run scans
for #244's exact signature: an anonymous pad with no net and an empty layer set,
paired one-for-one with a drawing shape missing from the registered footprint
library. It refuses ambiguous pad layouts or an unavailable library rather
than guessing. Optionally pass `references` to restrict the scan.

Review `candidates`, then call the tool again with `dry_run: false` and the
exact returned `plan_revision` as `expected_plan_revision`. All candidates are
repaired in one KiCAD undo commit. Placement, footprint identity, symbol path,
pad nets and non-shape children are preserved; a live read-back verifies that
the phantom pads are gone and the expected drawing shapes returned. Save the
board and run DRC afterward. Ctrl-Z reverses the complete repair if its visual
result is not what you expect.

## "kicad-cli not found"

Common install paths are auto-detected (including the Windows registry). If
your install is somewhere unusual, set the path in the plugin settings dialog
in a `settings.json` beside the binary, or in a `konnect.toml` in the working directory (`kicad_cli`). Discovery order is `konnect.toml` and `settings.json` in the CWD, then `settings.json` next to the binary and one level up, then the platform config dir. **Only the first existing file is loaded — later ones are not merged in**, so a `kicad_cli` set in a lower-priority file is ignored while a higher-priority file exists. A file under any other name is only read when passed with `--config`.

If a setting appears to be ignored, call `get_installation_info` and read its
`configuration` block: `selected_path` is the file that configured the running
process and `skipped_existing_paths` lists the ones it shadowed. That
distinguishes "my file was never read" from "my file was read and the value is
wrong", which otherwise look identical.

## Native Specctra export used the Rust fallback

The KiCad-native exporter is an optional KiCad 10 compatibility path. Open the
PCB Editor, choose **Tools → External Plugins → Konnect**, enable **KiCad 10
native Specctra bridge**, save, and close the dialog. The status changes to
running after the setting is applied. The requested board must be saved and be
the active PCB Editor document.

`export_specctra_dsn` defaults to `native_bridge_mode: "prefer"`. Its response says
whether `method` was `kicad10_native_actionplugin` or `kicad_ipc_snapshot` and
includes bounded `native_bridge_diagnostics`. Use `native_bridge_mode: "require"`
when testing the native path so an unavailable bridge is an error instead of a
fallback. Use `disable` to force Rust output.

The bridge listens only on an ephemeral IPv4 loopback port and requires a
per-session bearer token. Registration and temporary DSN files live under the
per-user local-data directory (`%LOCALAPPDATA%\konnect\native-bridge` on
Windows); `KONNECT_BRIDGE_DIR` overrides it for diagnostics and tests. A clean
plugin shutdown removes its own registration and temporary files. Stale
registrations left by a hard KiCad crash are ignored because Konnect probes and
authenticates each candidate before use.

This option is unavailable on KiCad 11 after removal of the legacy SWIG Python
API. Konnect then uses its Rust exporter unless KiCad gains an equivalent
supported IPC operation.

## A schematic write is blocked by a KiCad editor lock

Konnect refuses to change a `.kicad_sch` file while the sibling
`~<name>.kicad_sch.lck` exists. Close the schematic editor normally and retry.
Read-only schematic tools remain available while the lock exists.

KiCad's lock stores only a username and hostname, not a process identifier or
document-instance token. Konnect therefore cannot distinguish a live lock from
one left by a crash without risking unsaved editor state. It treats valid,
foreign-host, empty, and malformed locks alike and never removes one
automatically. If KiCad crashed, first confirm that no schematic editor owns the
file; reopening and closing the project cleanly is the preferred way to resolve
the lock. Remove a confirmed stale lock manually only as a last resort.

## Transaction recovery is blocked by divergent content

Multi-file schematic changes persist a `.konnect-transaction-<id>.json`
write-ahead journal in the project before changing any target. On restart,
Konnect safely completes files that still match either the recorded before
image or intended replacement. It never overwrites a file changed by KiCad or
another process after the journal was written.

Inspect active journals without printing their contents:

```text
konnect transaction status <project-dir>
```

Each target is reported as `pending`, `applied`, or `divergent`. Retry safe
recovery with:

```text
konnect transaction recover <project-dir>
```

If a target is divergent, first inspect the schematic in KiCad and preserve
the version you want. To unblock future transactions without changing any
schematic file, explicitly abandon the journal:

```text
konnect transaction abandon <project-dir> <transaction-id> --force
```

Abandonment renames the journal to
`.konnect-transaction-<id>.abandoned.json`; it does not restore, replace, or
delete a target. The abandoned file is retained as recovery evidence and is
ignored by future transactions. Delete it only after you have made any backup
you need.

Active and abandoned journals contain complete before/after images of every
affected schematic. Treat them as sensitive, do not attach them to bug reports
without reviewing their contents, and do not commit them. Both forms are
ignored by the repository `.gitignore`.

Cooperative document locks are stored outside the project under the platform
local-data directory. Set `KONNECT_STATE_DIR` to an absolute directory to
override that location. A relative override is rejected rather than falling
back to project-local sidecars.

## Tools don't appear after `load_toolset`

After a successful `load_toolset` call the server sends a
`notifications/tools/list_changed` notification, and MCP clients are expected to
re-fetch `tools/list` in response. If newly loaded tools never show up:

1. Check your client honors `notifications/tools/list_changed` (most current MCP
   clients do; some cache the initial tool list forever).
2. Disable any competing tool-search or tool-filter layer sitting between the
   model and the server. A Chrome-extension "tool search" that shadowed the real
   tool list caused exactly this in
   [#67](https://github.com/mixelpixx/Konnect/issues/67).
3. Re-issue `tools/list` (e.g. restart the client session) — the loaded toolset
   state lives in the server process and survives a list refresh.

If your client caches the initial tool list and never re-fetches it, none of the
above helps: the tools are loaded server-side, but the client has no schema to
invoke them with. `load_toolset` reports the names it loaded and *not* their
schemas, so a model can see a tool named in the reply and still be unable to
call it. That is the symptom in
[#134](https://github.com/mixelpixx/Konnect/issues/134) and
[#169](https://github.com/mixelpixx/Konnect/issues/169) — reported against
Claude Desktop.

For clients that cache the initial list but do not also cap the number of
callable tools, the fix is to make the *first* listing complete:

```json
{ "eager_toolsets": true }
```

in `konnect.toml` in the working directory, or a `settings.json` beside the binary. Every toolset is then loaded at
startup, so `tools/list` carries all 235 tools from the first call.

It is off by default because it costs what the router exists to save: roughly
25K tokens per listing instead of ~2K. Turn it on only if your client needs it.

Note that `auto_load_toolsets` does **not** solve this. It loads a toolset when
a tool from it is *called*, which helps only a client that already knows the
tool name — so it does nothing for a client whose tool list is stale.

## VS Code Copilot says a tool is "currently disabled by the user"

That exact message comes from the VS Code Copilot client layer, not Konnect.
In the confirmed report in
[#325](https://github.com/mixelpixx/Konnect/issues/325), the attempted calls did
not appear in Konnect's `get_recent_calls` output because Copilot refused them
before they reached the server.

Two Copilot behaviors make the normal toolset settings ineffective:

- With `eager_toolsets = false`, Copilot caches the initial `tools/list` and
  does not re-fetch it after `notifications/tools/list_changed`. Tools loaded
  later therefore remain unavailable to the model.
- With `eager_toolsets = true`, Konnect advertises its full catalog at startup,
  but Copilot applies its own total callable-tool budget across all configured
  MCP servers. The #325 reporter measured a 128-tool ceiling and saw only an
  arbitrary, changing subset of Konnect tools exposed. Tools outside that
  subset produced "currently disabled by the user."

Changing Konnect from stdio to HTTP/SSE does not remove a limit applied by the
client after it receives `tools/list`. Reloading the VS Code window also does
not make an over-budget catalog callable.

Current options are:

1. Disable unrelated MCP servers or tools if that brings the complete set you
   need below the client's budget.
2. Use an MCP client that honors `tools/list_changed` or can expose Konnect's
   full catalog.
3. Use the community two-tool proxy pattern demonstrated in
   [the #325 follow-up](https://github.com/mixelpixx/Konnect/issues/325#issuecomment-5407317596):
   expose only `konnect_help` and `konnect_call` to Copilot, let
   `konnect_help()` list names or return one tool's description and schema,
   and let `konnect_call(tool, arguments)` forward the actual call to a child
   Konnect process started with `eager_toolsets = true`.

The proxy is a community workaround attached to the issue, not code shipped or
reviewed by Konnect; inspect it and configure its executable path before use.
A native compact tool-surface mode and MCP tool-directory resource are planned
in the [client compatibility roadmap](../ROADMAP.md#4-client-compatibility),
but are not available yet.

## Plugin doesn't appear in KiCAD

Install via **Plugin and Content Manager → Install from File** with the
`konnect-pcm-*.zip` release asset (not the bare binary archives), then restart
KiCAD.

## A board no longer opens after `set_active_layer`

Konnect v0.12.0 could write an unsupported `(active_layer "...")` entry into a
board's `(setup ...)` block. KiCad 10.0.6 cannot parse that token because the
active layer belongs to the editor session, not the `.kicad_pcb` document.

Close the board in KiCad, make a backup copy, remove only the injected
`(active_layer "...")` line from the board file, and reopen the board. Do not
remove the surrounding `(setup ...)` block. Konnect versions containing the
fix for #610 refuse `set_active_layer` with `unsupported_capability` and leave
the file byte-identical until stable KiCad IPC exposes a native operation with
readback.
