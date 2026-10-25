"""Tests for the Konnect settings dialog and window parenting behavior."""

import os
from pathlib import Path
import sys
import types
import unittest
from unittest import mock

PLUGIN_DIR = Path(__file__).resolve().parents[1]


def _ensure_stubs():
    if "pcbnew" not in sys.modules:
        pcbnew = types.ModuleType("pcbnew")
        class _ActionPlugin:
            def register(self):
                pass
        pcbnew.ActionPlugin = _ActionPlugin
        pcbnew.GetBoard = lambda: None
        sys.modules["pcbnew"] = pcbnew
    else:
        pcbnew = sys.modules["pcbnew"]
        if not hasattr(pcbnew, "ActionPlugin"):
            class _ActionPlugin:
                def register(self):
                    pass
            pcbnew.ActionPlugin = _ActionPlugin
        if not hasattr(pcbnew, "GetBoard"):
            pcbnew.GetBoard = lambda: None

    if "wx" not in sys.modules:
        wx = types.ModuleType("wx")
        sys.modules["wx"] = wx
    else:
        wx = sys.modules["wx"]

    wx.DEFAULT_DIALOG_STYLE = 1
    wx.RESIZE_BORDER = 2
    wx.ID_ANY = -1
    wx.ID_OK = 5100
    wx.ID_YES = 5103
    wx.ID_NO = 5104
    wx.ID_CLOSE = 5105
    wx.ID_SAVE = 5106
    wx.OK = 4
    wx.ICON_INFORMATION = 800
    wx.ICON_ERROR = 400
    wx.ALIGN_CENTER_VERTICAL = 1
    wx.ALIGN_RIGHT = 2
    wx.EXPAND = 4
    wx.ALL = 8
    wx.LEFT = 16
    wx.RIGHT = 32
    wx.BOTTOM = 64
    wx.VERTICAL = 1
    wx.HORIZONTAL = 2
    wx.EVT_BUTTON = object()

    class _Window:
        def __init__(self, title=""):
            self._title = title
            self._shown = True
            self._active = False

        def GetTitle(self):
            return self._title

        def IsShown(self):
            return self._shown

        def IsActive(self):
            return self._active

        def Raise(self):
            pass

    class _Dialog(_Window):
        def __init__(self, parent=None, title="", style=0):
            super().__init__(title=title)
            self.parent = parent
            self._end_modal_code = None

        def EndModal(self, code):
            self._end_modal_code = code

        def FromDIP(self, size):
            return size

        def SetMinClientSize(self, size):
            pass

        def Fit(self):
            pass

        def CenterOnParent(self):
            pass

        def SetSizer(self, sizer):
            pass

    class _DummyCtrl:
        def __init__(self, *a, **k):
            self._val = ""
            self._sel = 0

        def SetValue(self, val):
            self._val = val

        def GetValue(self):
            return self._val

        def SetSelection(self, sel):
            self._sel = sel

        def GetSelection(self):
            return self._sel

        def SetLabel(self, lbl):
            self._val = lbl

        def SetForegroundColour(self, col):
            pass

        def Enable(self, en):
            pass

        def Bind(self, *a, **k):
            pass

        def SetSizer(self, sizer):
            pass

    wx.Window = _Window
    wx.Dialog = _Dialog
    wx.Panel = lambda *a, **k: _DummyCtrl()
    wx.BoxSizer = lambda *a, **k: mock.MagicMock()
    wx.StaticBoxSizer = lambda *a, **k: mock.MagicMock()
    wx.FlexGridSizer = lambda *a, **k: mock.MagicMock()
    wx.StdDialogButtonSizer = lambda *a, **k: mock.MagicMock()
    wx.StaticText = lambda *a, **k: _DummyCtrl()
    wx.TextCtrl = lambda *a, **k: _DummyCtrl()
    wx.Button = lambda *a, **k: _DummyCtrl()
    wx.Choice = lambda *a, **k: _DummyCtrl()
    wx.CheckBox = lambda *a, **k: _DummyCtrl()
    wx.Colour = lambda *a, **k: None
    wx.Size = lambda *a, **k: None
    wx.MessageBox = mock.MagicMock()
    wx.GetTopLevelWindows = lambda: []
    wx.GetActiveWindow = lambda: None
    wx.GetTopLevelParent = lambda win: win
    wx.Window.FindFocus = lambda: None

    if str(PLUGIN_DIR) not in sys.path:
        sys.path.insert(0, str(PLUGIN_DIR))

    import importlib
    spec_diag = importlib.util.spec_from_file_location(
        "settings_dialog", PLUGIN_DIR / "settings_dialog.py"
    )
    settings_dialog = importlib.util.module_from_spec(spec_diag)
    sys.modules["settings_dialog"] = settings_dialog
    spec_diag.loader.exec_module(settings_dialog)

    spec_init = importlib.util.spec_from_file_location(
        "konnect_init", PLUGIN_DIR / "__init__.py"
    )
    konnect_init = importlib.util.module_from_spec(spec_init)
    spec_init.loader.exec_module(konnect_init)

    return konnect_init, settings_dialog, wx, pcbnew


class SettingsDialogFocusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plugin, cls.dialog_mod, cls.wx, cls.pcbnew = _ensure_stubs()

    def test_find_pcb_editor_window_prefers_pcb_editor_over_project_manager(self):
        proj_mgr = self.wx.Window(title="MyProject — KiCad 10.0")
        pcb_editor = self.wx.Window(title="MyProject — PCB Editor")

        with mock.patch.object(self.plugin.wx, "GetTopLevelWindows", return_value=[proj_mgr, pcb_editor]), \
                mock.patch.object(self.plugin.wx.Window, "FindFocus", return_value=None), \
                mock.patch.object(self.plugin.wx, "GetActiveWindow", return_value=None):
            found = self.plugin.find_pcb_editor_window()
            self.assertIs(found, pcb_editor, "PCB Editor frame must be chosen over the KiCad Project Manager")

    def test_find_pcb_editor_window_uses_focus_when_available(self):
        proj_mgr = self.wx.Window(title="MyProject — KiCad 10.0")
        pcb_editor = self.wx.Window(title="MyProject — PCB Editor")
        focused_child = self.wx.Window(title="canvas")

        with mock.patch.object(self.plugin.wx, "GetTopLevelWindows", return_value=[proj_mgr, pcb_editor]), \
                mock.patch.object(self.plugin.wx.Window, "FindFocus", return_value=focused_child), \
                mock.patch.object(self.plugin.wx, "GetTopLevelParent", return_value=pcb_editor):
            found = self.plugin.find_pcb_editor_window()
            self.assertIs(found, pcb_editor)

    def test_find_pcb_editor_window_matches_board_filename(self):
        proj_mgr = self.wx.Window(title="Lichtwecker — KiCad 10.0")
        pcb_editor = self.wx.Window(title="*Lichtwecker.kicad_pcb")

        fake_board = mock.MagicMock()
        fake_board.GetFileName.return_value = "/path/to/Lichtwecker.kicad_pcb"

        with mock.patch.object(self.plugin.wx, "GetTopLevelWindows", return_value=[proj_mgr, pcb_editor]), \
                mock.patch.object(self.plugin.wx.Window, "FindFocus", return_value=None), \
                mock.patch.object(self.plugin.wx, "GetActiveWindow", return_value=None), \
                mock.patch.object(self.plugin.pcbnew, "GetBoard", return_value=fake_board):
            found = self.plugin.find_pcb_editor_window()
            self.assertIs(found, pcb_editor)

    def test_on_save_passes_parent_self_to_messagebox(self):
        dlg = self.dialog_mod.KonnectSettingsDialog(
            parent=None,
            plugin_dir=str(PLUGIN_DIR),
            binary_path="/fake/konnect",
        )
        self.wx.MessageBox.reset_mock()
        with mock.patch("settings_dialog.save_settings"):
            dlg._on_save(None)

        self.wx.MessageBox.assert_called_once()
        _args, kwargs = self.wx.MessageBox.call_args
        self.assertIs(kwargs.get("parent"), dlg, "wx.MessageBox must be parented to the dialog")

    def test_on_start_stop_with_callbacks_does_not_call_endmodal(self):
        start_called = [False]
        stop_called = [False]

        def on_start():
            start_called[0] = True
            return True

        def on_stop():
            stop_called[0] = True

        dlg = self.dialog_mod.KonnectSettingsDialog(
            parent=None,
            plugin_dir=str(PLUGIN_DIR),
            binary_path="/fake/konnect",
            server_running=False,
            on_start_server=on_start,
            on_stop_server=on_stop,
        )

        with mock.patch("settings_dialog.save_settings"):
            # Click Start Server
            dlg._on_start_stop(None)
            self.assertTrue(start_called[0])
            self.assertTrue(dlg._server_running)
            self.assertIsNone(dlg._end_modal_code, "EndModal must not be called when callback is present")

            # Click Stop Server
            dlg._on_start_stop(None)
            self.assertTrue(stop_called[0])
            self.assertFalse(dlg._server_running)
            self.assertIsNone(dlg._end_modal_code, "EndModal must not be called when callback is present")

    def test_on_start_stop_fallback_without_callbacks_calls_endmodal(self):
        dlg = self.dialog_mod.KonnectSettingsDialog(
            parent=None,
            plugin_dir=str(PLUGIN_DIR),
            binary_path="/fake/konnect",
            server_running=False,
            on_start_server=None,
            on_stop_server=None,
        )

        with mock.patch("settings_dialog.save_settings"):
            dlg._on_start_stop(None)
            self.assertEqual(dlg._end_modal_code, self.wx.ID_YES)

            dlg._server_running = True
            dlg._on_start_stop(None)
            self.assertEqual(dlg._end_modal_code, self.wx.ID_NO)


if __name__ == "__main__":
    unittest.main()
