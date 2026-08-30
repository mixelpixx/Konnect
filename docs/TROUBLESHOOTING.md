# Troubleshooting

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

## "KiCAD IPC socket path not configured"

Any tool that talks to a live KiCAD session (`save_project`, PCB editing,
`check_kicad_ui`, …) needs the IPC socket address. Two separate configurations
must both be correct — neither happens automatically:

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
in a `settings.json` beside the binary, or in a `konnect.toml` in the working directory (`kicad_cli`). Discovery order is `konnect.toml` and `settings.json` in the CWD, then `settings.json` next to the binary and one level up, then the platform config dir. A file under any other name is only read when passed with `--config`.

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
startup, so `tools/list` carries all 228 tools from the first call.

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
