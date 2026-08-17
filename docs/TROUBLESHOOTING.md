# Troubleshooting

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

## A closed-board edit landed after KiCAD crashed

`place_component`, `move_component` and `rotate_component` edit the board file
directly when the IPC transport is unreachable, which is safe when KiCAD was
never running.

They cannot currently tell that from "KiCAD died a second ago holding this
board" — the transport looks identical
([#240](https://github.com/mixelpixx/Konnect/issues/240)). If KiCAD crashes or
is force-quit mid-session, the next such call may edit the file behind it and
report success. Reopen the board and check before continuing, and prefer
reopening KiCAD first.

## "kicad-cli not found"

Common install paths are auto-detected (including the Windows registry). If
your install is somewhere unusual, set the path in the plugin settings dialog
in a `settings.json` beside the binary, or in a `konnect.toml` in the working directory (`kicad_cli`). Discovery order is `konnect.toml` and `settings.json` in the CWD, then `settings.json` next to the binary and one level up, then the platform config dir. A file under any other name is only read when passed with `--config`.

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

The fix for those clients is to make the *first* listing complete:

```json
{ "eager_toolsets": true }
```

in `konnect.toml` in the working directory, or a `settings.json` beside the
binary. Every toolset is then loaded at startup, so `tools/list` carries all
210 tools from the first call.

It is off by default because it costs what the router exists to save: roughly
25K tokens per listing instead of ~2K. Turn it on only if your client needs it.

Note that `auto_load_toolsets` does **not** solve this. It loads a toolset when
a tool from it is *called*, which helps only a client that already knows the
tool name — so it does nothing for a client whose tool list is stale.

## Plugin doesn't appear in KiCAD

Install via **Plugin and Content Manager → Install from File** with the
`konnect-pcm-*.zip` release asset (not the bare binary archives), then restart
KiCAD.
