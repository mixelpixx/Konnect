"""
Konnect — Settings Dialog.

wxPython dialog for configuring the MCP server: paths and
server control. Launched from the KiCAD Tools menu via the action plugin.

Settings persist to settings.json in the plugin directory.
"""

import json
import os
import subprocess
import sys

import wx


# ─── Default settings ────────────────────────────────────────────────────────

DEFAULT_SETTINGS = {
    "kicad_cli": "",
    "ipc_socket_path": "",
    "jlcpcb_db_path": "",
    "log_level": "info",
    "transport": "stdio",
    "native_specctra_bridge": False,
}

LOG_LEVELS = ["error", "warn", "info", "debug", "trace"]

TRANSPORTS = ["stdio", "http", "both"]


# ─── Settings I/O ────────────────────────────────────────────────────────────

def load_settings(settings_path):
    """Load settings from JSON, falling back to defaults."""
    settings = dict(DEFAULT_SETTINGS)
    if os.path.exists(settings_path):
        try:
            with open(settings_path, "r") as f:
                saved = json.load(f)
            settings.update(saved)
        except (json.JSONDecodeError, IOError):
            pass
    return settings


def save_settings(settings_path, settings):
    """Write settings to JSON."""
    with open(settings_path, "w") as f:
        json.dump(settings, f, indent=2)


# ─── KiCAD CLI auto-detection ────────────────────────────────────────────────

def detect_kicad_cli():
    """Try to find kicad-cli by scanning common locations, registry, and PATH."""
    binary = "kicad-cli.exe" if sys.platform == "win32" else "kicad-cli"

    if sys.platform == "win32":
        # Check both per-user and machine-wide uninstall records. Current-user
        # installs are KiCad's normal result when machine-wide installation is
        # declined, and are recorded under HKCU rather than HKLM.
        try:
            import winreg
            uninstall = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"
            for version in ["10.0", "9.0", "8.0"]:
                for hive in [winreg.HKEY_CURRENT_USER, winreg.HKEY_LOCAL_MACHINE]:
                    for view in [winreg.KEY_WOW64_64KEY, winreg.KEY_WOW64_32KEY]:
                        try:
                            with winreg.OpenKey(
                                    hive, uninstall, 0, winreg.KEY_READ | view) as parent:
                                for index in range(winreg.QueryInfoKey(parent)[0]):
                                    with winreg.OpenKey(
                                            parent, winreg.EnumKey(parent, index)) as child:
                                        try:
                                            display_name = winreg.QueryValueEx(
                                                child, "DisplayName")[0]
                                            display_version = winreg.QueryValueEx(
                                                child, "DisplayVersion")[0]
                                            install_dir = winreg.QueryValueEx(
                                                child, "InstallLocation")[0]
                                        except (FileNotFoundError, OSError):
                                            continue
                                        if (display_name.lower().startswith("kicad ") and
                                                (display_version.startswith(version) or
                                                 version in display_name)):
                                            cli = os.path.join(
                                                install_dir, "bin", "kicad-cli.exe")
                                            if os.path.isfile(cli):
                                                return cli
                        except (FileNotFoundError, OSError):
                            pass
        except ImportError:
            pass

        # Scan common root directories for KiCad installations
        versions = ["10.0", "9.0", "8.0"]
        roots = [
            os.path.join(os.environ.get("LOCALAPPDATA", ""), "Programs"),
            os.environ.get("ProgramFiles", "C:\\Program Files"),
            os.environ.get("ProgramFiles(x86)", "C:\\Program Files (x86)"),
            "C:\\", "D:\\", "D:\\Program Files",
        ]
        for root in roots:
            for ver in versions:
                cli = os.path.join(root, "KiCad", ver, "bin", "kicad-cli.exe")
                if os.path.isfile(cli):
                    return cli
            # Also check without version subdir
            cli = os.path.join(root, "KiCad", "bin", "kicad-cli.exe")
            if os.path.isfile(cli):
                return cli

    elif sys.platform == "darwin":
        candidates = [
            "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli",
            "/usr/local/bin/kicad-cli",
        ]
        for path in candidates:
            if os.path.isfile(path):
                return path
    else:
        candidates = [
            "/usr/bin/kicad-cli",
            "/usr/local/bin/kicad-cli",
            "/snap/kicad/current/usr/bin/kicad-cli",
        ]
        for path in candidates:
            if os.path.isfile(path):
                return path

    # Try PATH as last resort
    try:
        result = subprocess.run(
            [binary, "--version"],
            capture_output=True, text=True, timeout=5,
        )
        if result.returncode == 0:
            return binary
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    return ""


# ─── Dialog ──────────────────────────────────────────────────────────────────

class KonnectSettingsDialog(wx.Dialog):
    """Settings dialog for the Konnect plugin."""

    def __init__(
        self,
        parent,
        plugin_dir,
        binary_path,
        server_running=False,
        native_bridge_available=False,
        native_bridge_running=False,
        on_start_server=None,
        on_stop_server=None,
    ):
        # No fixed pixel size: on high-DPI/scaled Windows displays a fixed
        # (520, 480) clipped the Save/Close buttons off-screen (issue #18).
        # The dialog is sized to fit its content after _build_ui instead.
        super().__init__(
            parent,
            title="Konnect Settings",
            style=wx.DEFAULT_DIALOG_STYLE | wx.RESIZE_BORDER,
        )
        self.plugin_dir = plugin_dir
        self.binary_path = binary_path
        self.settings_path = os.path.join(plugin_dir, "settings.json")
        self.settings = load_settings(self.settings_path)
        self._server_running = server_running
        self._native_bridge_available = native_bridge_available
        self._native_bridge_running = native_bridge_running
        self._on_start_server = on_start_server
        self._on_stop_server = on_stop_server

        self._build_ui()
        self._populate_fields()

        # Size to content (DPI-aware), with a sensible minimum width so the
        # path fields aren't cramped. FromDIP scales with the display.
        self.SetMinClientSize(self.FromDIP(wx.Size(520, 0)))
        self.Fit()
        self.CenterOnParent()

    def _build_ui(self):
        panel = wx.Panel(self)
        main_sizer = wx.BoxSizer(wx.VERTICAL)

        # ── Paths section ────────────────────────────────────────────
        paths_box = wx.StaticBoxSizer(wx.VERTICAL, panel, "Paths")
        pgrid = wx.FlexGridSizer(2, 3, 5, 5)
        pgrid.AddGrowableCol(1, 1)

        pgrid.Add(wx.StaticText(panel, label="kicad-cli:"), 0, wx.ALIGN_CENTER_VERTICAL)
        self.kicad_cli_ctrl = wx.TextCtrl(panel)
        pgrid.Add(self.kicad_cli_ctrl, 1, wx.EXPAND)
        self.browse_cli_btn = wx.Button(panel, label="Browse")
        self.browse_cli_btn.Bind(wx.EVT_BUTTON, self._on_browse_cli)
        pgrid.Add(self.browse_cli_btn, 0)

        pgrid.Add(wx.StaticText(panel, label="JLCPCB DB:"), 0, wx.ALIGN_CENTER_VERTICAL)
        self.jlcpcb_ctrl = wx.TextCtrl(panel)
        pgrid.Add(self.jlcpcb_ctrl, 1, wx.EXPAND)
        self.browse_jlcpcb_btn = wx.Button(panel, label="Browse")
        self.browse_jlcpcb_btn.Bind(wx.EVT_BUTTON, self._on_browse_jlcpcb)
        pgrid.Add(self.browse_jlcpcb_btn, 0)

        paths_box.Add(pgrid, 0, wx.EXPAND | wx.ALL, 5)
        main_sizer.Add(paths_box, 0, wx.EXPAND | wx.ALL, 8)

        # ── Advanced section ─────────────────────────────────────────
        adv_box = wx.StaticBoxSizer(wx.VERTICAL, panel, "Advanced")
        agrid = wx.FlexGridSizer(3, 2, 5, 5)
        agrid.AddGrowableCol(1, 1)

        agrid.Add(wx.StaticText(panel, label="IPC Socket:"), 0, wx.ALIGN_CENTER_VERTICAL)
        self.ipc_ctrl = wx.TextCtrl(panel)
        agrid.Add(self.ipc_ctrl, 1, wx.EXPAND)

        agrid.Add(wx.StaticText(panel, label="Log Level:"), 0, wx.ALIGN_CENTER_VERTICAL)
        self.log_level_ctrl = wx.Choice(panel, choices=LOG_LEVELS)
        agrid.Add(self.log_level_ctrl, 1, wx.EXPAND)

        agrid.Add(wx.StaticText(panel, label="Transport:"), 0, wx.ALIGN_CENTER_VERTICAL)
        self.transport_ctrl = wx.Choice(panel, choices=TRANSPORTS)
        agrid.Add(self.transport_ctrl, 1, wx.EXPAND)

        adv_box.Add(agrid, 0, wx.EXPAND | wx.ALL, 5)

        self.native_bridge_ctrl = wx.CheckBox(
            panel,
            label="Enable KiCad 10 native Specctra bridge",
        )
        self.native_bridge_ctrl.Enable(self._native_bridge_available)
        adv_box.Add(self.native_bridge_ctrl, 0, wx.LEFT | wx.RIGHT | wx.BOTTOM, 5)
        bridge_state = "Running" if self._native_bridge_running else (
            "Available after Save/Close" if self._native_bridge_available else "Unavailable"
        )
        self.native_bridge_status = wx.StaticText(
            panel,
            label=f"Native bridge: {bridge_state}",
        )
        adv_box.Add(self.native_bridge_status, 0, wx.LEFT | wx.RIGHT | wx.BOTTOM, 5)
        main_sizer.Add(adv_box, 0, wx.EXPAND | wx.ALL, 8)

        # ── Server status section ────────────────────────────────────
        server_box = wx.StaticBoxSizer(wx.HORIZONTAL, panel, "Server")
        self.server_status = wx.StaticText(panel, label="Stopped")
        server_box.Add(self.server_status, 1, wx.ALIGN_CENTER_VERTICAL | wx.LEFT, 5)
        self.start_stop_btn = wx.Button(panel, label="Start Server")
        self.start_stop_btn.Bind(wx.EVT_BUTTON, self._on_start_stop)
        server_box.Add(self.start_stop_btn, 0, wx.ALL, 5)
        main_sizer.Add(server_box, 0, wx.EXPAND | wx.ALL, 8)

        # ── Bottom buttons ───────────────────────────────────────────
        btn_sizer = wx.StdDialogButtonSizer()
        self.save_btn = wx.Button(panel, wx.ID_SAVE, "Save")
        self.save_btn.Bind(wx.EVT_BUTTON, self._on_save)
        btn_sizer.AddButton(self.save_btn)

        close_btn = wx.Button(panel, wx.ID_CLOSE, "Close")
        close_btn.Bind(wx.EVT_BUTTON, self._on_close)
        btn_sizer.AddButton(close_btn)
        btn_sizer.Realize()

        main_sizer.Add(btn_sizer, 0, wx.ALIGN_RIGHT | wx.ALL, 8)

        panel.SetSizer(main_sizer)

        # Dialog-level sizer so Fit() sizes the dialog to the panel's content.
        dlg_sizer = wx.BoxSizer(wx.VERTICAL)
        dlg_sizer.Add(panel, 1, wx.EXPAND)
        self.SetSizer(dlg_sizer)

        self._update_server_status()

    def _populate_fields(self):
        """Fill dialog fields from loaded settings."""
        cli = self.settings.get("kicad_cli", "")
        if not cli:
            cli = detect_kicad_cli()
        self.kicad_cli_ctrl.SetValue(cli)

        self.jlcpcb_ctrl.SetValue(self.settings.get("jlcpcb_db_path", ""))
        ipc_path = self.settings.get("ipc_socket_path", "")
        if not ipc_path:
            ipc_path = os.environ.get("KICAD_API_SOCKET", "")
        self.ipc_ctrl.SetValue(ipc_path)

        level = self.settings.get("log_level", "info")
        if level in LOG_LEVELS:
            self.log_level_ctrl.SetSelection(LOG_LEVELS.index(level))
        else:
            self.log_level_ctrl.SetSelection(2)  # "info"

        transport = self.settings.get("transport", "stdio")
        if transport in TRANSPORTS:
            self.transport_ctrl.SetSelection(TRANSPORTS.index(transport))
        else:
            self.transport_ctrl.SetSelection(0)  # "stdio"

        self.native_bridge_ctrl.SetValue(
            self._native_bridge_available
            and bool(self.settings.get("native_specctra_bridge", False))
        )

    def _collect_settings(self):
        """Read current field values into a settings dict."""
        return {
            "kicad_cli": self.kicad_cli_ctrl.GetValue().strip(),
            "ipc_socket_path": self.ipc_ctrl.GetValue().strip(),
            "jlcpcb_db_path": self.jlcpcb_ctrl.GetValue().strip(),
            "log_level": LOG_LEVELS[self.log_level_ctrl.GetSelection()],
            "transport": TRANSPORTS[self.transport_ctrl.GetSelection()],
            "native_specctra_bridge": self.native_bridge_ctrl.GetValue(),
        }

    def _update_server_status(self):
        """Update the server status display."""
        if self._server_running:
            self.server_status.SetLabel("Running")
            self.server_status.SetForegroundColour(wx.Colour(0, 128, 0))
            self.start_stop_btn.SetLabel("Stop Server")
        else:
            self.server_status.SetLabel("Stopped")
            self.server_status.SetForegroundColour(wx.Colour(180, 0, 0))
            self.start_stop_btn.SetLabel("Start Server")

    # ── Event handlers ───────────────────────────────────────────────

    def _on_browse_cli(self, event):
        wildcard = "Executables (*.exe)|*.exe|All files (*)|*" if sys.platform == "win32" else "All files (*)|*"
        dlg = wx.FileDialog(self, "Select kicad-cli", wildcard=wildcard, style=wx.FD_OPEN)
        if dlg.ShowModal() == wx.ID_OK:
            self.kicad_cli_ctrl.SetValue(dlg.GetPath())
        dlg.Destroy()

    def _on_browse_jlcpcb(self, event):
        dlg = wx.FileDialog(self, "Select JLCPCB database", wildcard="SQLite (*.db)|*.db|All files (*)|*", style=wx.FD_OPEN)
        if dlg.ShowModal() == wx.ID_OK:
            self.jlcpcb_ctrl.SetValue(dlg.GetPath())
        dlg.Destroy()

    def _on_save(self, event):
        self.settings = self._collect_settings()
        save_settings(self.settings_path, self.settings)
        wx.MessageBox("Settings saved.", "Konnect", wx.OK | wx.ICON_INFORMATION, parent=self)

    def _on_start_stop(self, event):
        """Toggle server state — executes callbacks in-place or delegates via EndModal."""
        self.settings = self._collect_settings()
        save_settings(self.settings_path, self.settings)

        if not self._server_running:
            if self._on_start_server:
                success = self._on_start_server()
                if success is not False:
                    self._server_running = True
                    self._update_server_status()
            else:
                self.EndModal(wx.ID_YES)
        else:
            if self._on_stop_server:
                self._on_stop_server()
                self._server_running = False
                self._update_server_status()
            else:
                self.EndModal(wx.ID_NO)

    def _on_close(self, event):
        self.EndModal(wx.ID_CLOSE)
