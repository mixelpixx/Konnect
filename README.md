<a name="top"></a>

<div align="center">

<img src="resources/images/KiCAD-MCP-Server-rust.svg" alt="KiCAD-MCP-Server Logo" height="240" />


# Konnect *BETA Release

</div>

**AI-assisted PCB design for KiCAD 10.** Konnect is a native KiCAD plugin — a single
Rust binary — that lets Claude and other AI assistants design schematics and PCBs
through the [Model Context Protocol](https://modelcontextprotocol.io) (MCP).

**185 tools across 18 on-demand toolsets.** Schematic capture, PCB layout and
routing, ERC/DRC, design-review audits, JLCPCB part search, Freerouting, reference
circuits, and a full manufacturing export pipeline — with bundled skills and agents
that teach Claude KiCAD conventions out of the box.

> **Status: beta.** The core toolchain is tested and working, but this is a young
> release and it wants real-world mileage and review. Issues and PRs are welcome —
> see [CONTRIBUTING.md](CONTRIBUTING.md).

## Why Konnect exists

Konnect is the successor to [KiCAD-MCP-Server](https://github.com/mixelpixx/KiCAD-MCP-Server),
a Python/TypeScript project that proved AI-driven PCB design works — and, in the
process, showed exactly where that architecture runs out of road. Konnect was built
to fix those specific problems:

**The call path was too long.** In the original server, a single tool call travels
through TypeScript, schema validation, a spawned Python subprocess, JSON over
stdin/stdout, a command router, and finally SWIG-generated C++ proxy objects before
anything touches your board. That's four language and serialization boundaries, each
with its own failure modes — subprocess lifecycle management, stdout parsing that
filters out warnings KiCAD leaks into the stream, chunked-JSON reassembly. In
Konnect, a tool call is a function call. One process, one language, no plumbing.

**The dependency surface was enormous.** Running the original means carrying Node.js
and its npm tree, Python and its pip packages, wxPython, kicad-skip, and KiCAD's
SWIG bindings — two package ecosystems plus a binding layer, every one of them a
moving target that can break an install. Konnect is a single static binary, about
5 MB. There is nothing to install alongside it and nothing to version-match.

**SWIG is a dead end.** The original's PCB backend depends on KiCAD's SWIG Python
bindings, which KiCAD is deprecating in favor of its IPC API. SWIG also carried
real operational scars: a zone-fill call that can segfault the backend, proxy-object
comparison bugs, and a fallback path that can silently swap backends mid-session.
Konnect talks to KiCAD 10 through the official IPC API (protobuf over NNG) — the
interface KiCAD is investing in — with real-time board edits that integrate with
KiCAD's own undo/redo.

**Schematic edits should not corrupt files.** Konnect edits `.kicad_sch` files
through its own S-expression engine with atomic writes (write, fsync, rename), UUID
preservation, and round-trip tests — no third-party schematic library with known
gaps, no text-manipulation workarounds.

**Context economy is a feature.** Exposing ~180 tools to an LLM costs roughly 23K
tokens of context on every listing. Konnect's router loads a starter kit (~2K
tokens) and lets the model pull in toolsets on demand — plus built-in observability
(`get_recent_calls`, `server_stats`, JSONL call logs) so the model can diagnose its
own tool failures.

The result is smaller, faster to install, aligned with where KiCAD is going, and
built for production use rather than experimentation. The original project remains
open, maintained, and useful — see [the comparison below](#relationship-to-kicad-mcp-server).

## What it does

Instead of describing changes and applying them by hand, the AI works your project
directly:

- **Place and wire schematic components** — add resistors, ICs, connectors; wire them
  together by pin name
- **Lay out the PCB** — place, move, rotate, and route footprints in real time via
  KiCAD's IPC API, with full undo/redo integration
- **Run design checks** — ERC, DRC, connectivity validation, decoupling audits,
  power-rail review, BOM health checks
- **Export production files** — Gerbers, drill, BOM, pick-and-place, 3D models, PDF
- **Search JLCPCB parts** — find in-stock components in a local 2.5M-part catalog and
  suggest alternatives
- **Start from reference circuits** — USB-C, LDO, buck converter, STM32, I2C, LED
  templates with verified component values
- **Watch it happen** — a live schematic viewer auto-refreshes as the AI edits

The full tool catalog is documented in [tool-directory.md](tool-directory.md).

## How it works

| Layer | Mechanism |
|-------|-----------|
| Schematic editing | Direct `.kicad_sch` S-expression editing with atomic writes (no KiCAD required) |
| PCB editing | KiCAD 10 IPC API (NNG + protobuf) — real-time, undo-aware, requires KiCAD running |
| Exports & checks | `kicad-cli` subprocess (Gerber, PDF, ERC, DRC, …) |
| Transport | MCP JSON-RPC over stdio (default), or Streamable HTTP (`transport = "http"` / `"both"`) |

## Installation

### From the KiCAD Plugin Manager (recommended)

1. Download the package for your OS from [Releases](https://github.com/mixelpixx/Konnect/releases):
   `konnect-pcm-v<version>-windows.zip`, `-macos.zip`, or `-linux.zip`. Each
   bundles that platform's server binary — the macOS package is a universal
   build, so one download covers Apple Silicon and Intel. (The `konnect-pcm-*`
   assets are the KiCAD plugin packages; the other archives are standalone
   server binaries.)
2. Open KiCAD 10 → **Plugin and Content Manager**
3. Click **Install from File** and select the zip
4. Restart KiCAD

Verify: open the **PCB Editor** → **Tools → External Plugins** → you should see
**Konnect**.

### Build from source

```bash
# protoc is required (protobuf code generation), and cmake (the nng crate
# compiles the NNG C library with it).
# Windows: choco install protoc cmake
# macOS:   brew install protobuf cmake
# Linux:   apt install protobuf-compiler cmake
cargo build --release -p konnect
```

### macOS

The [Releases](https://github.com/mixelpixx/Konnect/releases) page ships
standalone server binaries for both Apple Silicon (`aarch64-apple-darwin`) and
Intel (`x86_64-apple-darwin`). They are not yet code-signed, so if you download
one through a browser, clear the quarantine flag before first launch:

```bash
tar xzf konnect-v*-aarch64-apple-darwin.tar.gz
xattr -d com.apple.quarantine ./konnect   # only needed for browser downloads
./konnect --help
```

Or build from source as above (verified on Apple Silicon; the same
`target/release/konnect` binary is the MCP server).

KiCad on macOS keeps its tools inside the app bundle and they are not on
`PATH`, so point Konnect at them in `~/Library/Application Support/konnect/config.toml`:

```toml
kicad_cli = "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"
kicad_binary = "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad"
# KiCad 10's IPC socket on macOS (enable it in KiCad:
# Preferences → Plugins → "Enable KiCad API")
ipc_address = "ipc:///tmp/kicad/api.sock"
```

Claude Desktop's config lives at
`~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "konnect": {
      "command": "/path/to/konnect"
    }
  }
}
```

For Claude Code, put the same snippet in a `.mcp.json` in your project root.

Starting with the next release, the PCM package for macOS
(`konnect-pcm-v<version>-macos.zip`) bundles a universal server binary; for
v0.1.3 and earlier, install via a release tarball or a source build. The schematic
viewer compiles and launches on macOS (Tauri 2 uses the system WKWebView —
WebView2 is only a Windows requirement) but hasn't had the same mileage as
the Windows build yet.

## Setup with Claude Desktop

After a PCM install, the server binary lives in your KiCAD documents folder:

```
C:\Users\<YOU>\Documents\KiCad\10.0\3rdparty\plugins\com_github_mixelpixx_konnect\bin\konnect.exe
```

Edit `%APPDATA%\Claude\claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "konnect": {
      "command": "C:\\Users\\<YOU>\\Documents\\KiCad\\10.0\\3rdparty\\plugins\\com_github_mixelpixx_konnect\\bin\\konnect.exe"
    }
  }
}
```

Restart Claude Desktop and the Konnect tools appear. For Claude Code, drop the same
snippet into a `.mcp.json` in your project root (see [examples/](examples/)).

## Schematic viewer

A standalone viewer that auto-refreshes as the schematic file changes:

```bash
schematic-viewer.exe path\to\your\root_schematic.kicad_sch
```

Point it at the root sheet of a hierarchical design and every sub-sheet is available
in a live thumbnail filmstrip along the bottom. Click a thumbnail—or use the
left/right arrow keys—to open that page. Pan with click-drag, zoom with the wheel,
use `0` to fit and `R` to refresh. It can also be launched through the
`open_schematic_viewer` tool.

The native editor uses a compact vector-icon rail with hover guidance and an
action-icon timeline. Its text-selection mode supports UI character ranges and
all rendered schematic text, copied through the native platform clipboard.
Its complete local/external change log remains scrollable for the lifetime of
the Schematic Studio process.
It opens read-only: enable the explicit Edit mode checkbox to stage changes in
memory, then use Commit to write every dirty sheet as one exact-precondition
transaction or Discard to restore the durable source.

The viewer has two mutually exclusive compile-time renderer modes:

| Feature | Status | Render path | Runtime requirements |
|---|---|---|---|
| `renderer-kicad-cli` | Default, compatibility | KiCad exports SVG; Tauri displays it | KiCad plus WebView2 (Windows), WKWebView (macOS), or GTK3 + WebKitGTK 4.1 (Linux) |
| `renderer-vello` | Native preview | Konnect parses `.kicad_sch` and Vello draws the semantic scene directly | A wgpu-supported graphics backend; no KiCad CLI or WebKit runtime |

```bash
cd crates/schematic-viewer

# Compatibility renderer (default)
cargo build --release

# Direct native renderer
cargo build --release --no-default-features --features renderer-vello
```

Both modes watch the complete hierarchy and update only changed or newly discovered
sheets. Compatibility mode uses temporary snapshots so it never blocks KiCad from
saving and remains the fidelity reference. Native mode avoids the SVG/WebView path,
keeps parsed geometry cached, and repaints immediately. It renders schematic text
with KiCad's Newstroke geometry and has a headless golden-image comparison path.
Light mode follows KiCad's palette and default worksheet; dark mode uses
role-distinct high-contrast colors. The supported primitive set is pixel-identical
to KiCad's exported scene when both are rasterized by Vello;
unsupported or less-common schematic primitives are why native mode remains
opt-in.

Compatibility exports also populate an atomic per-user cache under
`$XDG_CACHE_HOME/konnect/schematic-viewer` (or the platform-equivalent user cache).
When a cache entry is newer than its schematic, native mode reuses only its symbol
paint-order metadata so compiler- and KiCad-build-specific unstable ties match the
installed KiCad renderer. Geometry still comes directly from `.kicad_sch`; stale or
missing entries fall back to Konnect's deterministic KiCad-source ordering. Nothing
is written into the project tree.

Native mode provides revision-aware editing for symbols and wiring primitives,
including selection, drag/nudge, rotate, mirror, duplicate, delete, properties,
wire and label placement, undo/redo, search, and live connectivity warnings.
Commands safely rebase across unrelated changes and report an explicit conflict
when the same KiCad item changed. See
[the viewer/editor architecture and shortcut reference](docs/SCHEMATIC_VIEWER.md)
for the complete interaction and transaction model.

To compare the native pixels against the installed KiCad version, run:

```bash
scripts/compare-schematic-renderers.sh path/to/sheet.kicad_sch render-diff.png
KONNECT_VELLO_SVG_ORACLE=1 KONNECT_MAX_SEMANTIC_RMSE=0 \
  scripts/compare-schematic-renderers.sh path/to/sheet.kicad_sch
scripts/compare-schematic-project.sh path/to/kicad-project
KONNECT_VELLO_SVG_ORACLE=1 scripts/compare-schematic-renderers.sh path/to/sheet.kicad_sch
scripts/test-schematic-renderer-goldens.sh
```

The second form is the strict pixel-identical semantic gate. The first reports
normalized cross-rasterizer RMSE and writes a visual diff; librsvg and Vello use
different antialiasing coverage, so that metric is useful for visual regressions
but is not the semantic parity result.

The optional same-Vello oracle renders KiCad's exported SVG and Konnect's
semantic scene through the identical Vello area-AA backend. This separates
semantic geometry, color, and ordering errors from librsvg/Vello coverage
differences. The comparison script supplies that fresh SVG as the paint-order
oracle for the native render. Headless renders use Vello's deterministic CPU path;
the live viewer remains GPU-accelerated. `KONNECT_MAX_SEMANTIC_RMSE` sets the
independent parity gate.
The golden test script enforces zero same-Vello semantic RMSE for the minimal
page and wire fixtures, so exact primitives cannot silently regress while
broader schematic coverage is being completed.

On Linux, both modes select native Wayland or X11 from the desktop session. The
compatibility mode disables WebKitGTK's failure-prone DMA-BUF renderer by default;
an explicit `WEBKIT_DISABLE_DMABUF_RENDERER` value is respected.

## Requirements

- KiCAD 10 (Windows is the most-tested platform; macOS works from the release
  binaries or a source build — see the [macOS section](#macos) above. Linux
  compiles and passes tests in CI but hasn't had per-platform QA yet; both are
  tracked on the [roadmap](ROADMAP.md))
- `kicad-cli` (ships with KiCAD — used for exports, ERC, DRC)
- For PCB tools: KiCAD running with the target board open (IPC API)

## License: free for the little guys

Konnect is licensed under the **[GNU AGPL-3.0](LICENSE)**.

If you're a hobbyist, student, freelancer, or open-source project: **use it freely,
no strings attached.** Design boards, ship them, sell them.

If you're a business: the AGPL requires that anything you build on or around Konnect —
including software provided over a network — be open-sourced under the same license.
If that doesn't work for you, **commercial licenses are available**: see
[COMMERCIAL.md](COMMERCIAL.md).

## Relationship to KiCAD-MCP-Server

The original [Python/TypeScript project](https://github.com/mixelpixx/KiCAD-MCP-Server)
remains fully open (MIT) and maintained. Konnect is where new development happens —
the architecture it proved, rebuilt for production:

| | KiCAD-MCP-Server | Konnect |
|---|---|---|
| Runtime | Node.js + Python + SWIG bindings | Single static binary (~5 MB) |
| Tool call path | TS → subprocess → Python → SWIG C++ | Direct function call |
| PCB backend | SWIG (deprecated by KiCAD) + experimental IPC | KiCAD 10 IPC API |
| Schematic backend | kicad-skip + custom loaders | Native S-expression engine, atomic writes |
| Context cost | Router pattern | Load/unload toolsets + observability |
| Skills / agents | — | 6 skills + 2 agents bundled |
| License | MIT | AGPL-3.0 + commercial |

## Troubleshooting

**Plugin doesn't appear in KiCAD** — install via the Plugin and Content Manager (not
manual copy), then restart KiCAD.

**PCB tools return "IPC connect failed"** — open KiCAD with your board file first;
PCB tools talk to the running PCB editor.

**"kicad-cli not found"** — common install paths are auto-detected; set the path
explicitly in the plugin settings dialog or your `konnect-settings.json` if yours
is elsewhere.

## Support

- Issues & feature requests: [GitHub Issues](https://github.com/mixelpixx/Konnect/issues)
- Roadmap: [ROADMAP.md](ROADMAP.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md)
