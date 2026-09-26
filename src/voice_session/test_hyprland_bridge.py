import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
spec = importlib.util.spec_from_file_location("hyprland_bridge", Path(__file__).with_name("hyprland_bridge.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)

MONITORS = [{"id": 0, "name": "HDMI-A-1", "x": 1920, "y": 0},
            {"id": 1, "name": "DP-1", "x": 0, "y": 0}]


class TargetTests(unittest.TestCase):
    def test_active_window_maps_to_kwin_report_fields(self):
        window = {"address": "0x5631", "class": "com.mitchellh.ghostty", "initialClass": "x",
                  "pid": 71, "monitor": 0}
        self.assertEqual(bridge.target_of(window, MONITORS), {
            "id": "0x5631", "app_id": "com.mitchellh.ghostty", "pid": 71,
            "output": "HDMI-A-1", "output_x": 1920, "output_y": 0})

    def test_no_focused_window_is_not_a_target(self):
        target = bridge.target_of({}, MONITORS)
        self.assertFalse(bridge.same_target(target, target))

    def test_empty_class_falls_back_to_initial_class_and_bad_pid_is_invalid(self):
        target = bridge.target_of({"address": "0x1", "class": "", "initialClass": "foot",
                                   "pid": -1, "monitor": 9}, MONITORS)
        self.assertEqual((target["app_id"], target["pid"], target["output"]), ("foot", 0, ""))
        self.assertFalse(bridge.same_target(target, target))


class FakeHyprland:
    def __init__(self, follow_mouse, fail_set=False):
        self.follow_mouse = follow_mouse
        self.fail_set = fail_set
        self.requests = []

    def __call__(self, request, deadline=None):
        self.requests.append(request)
        if request == "j/getoption input:follow_mouse":
            return json.dumps({"option": "input:follow_mouse", "int": self.follow_mouse, "set": True})
        if request.startswith("eval "):
            if self.fail_set:
                return "error: nope"
            self.follow_mouse = int(request.split("follow_mouse = ")[1].split()[0])
            return "ok"
        raise AssertionError(request)


class HoverTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / bridge.RESTORE_FILE
        self.deadline = bridge.Deadline()

    def run_with(self, fake, action):
        with patch.object(bridge, "ipc", fake):
            action()

    def test_hover_saves_original_before_changing_and_restores_it(self):
        fake = FakeHyprland(0)

        def on():
            bridge.hover_on(self.path, self.deadline)
            self.assertEqual(self.path.read_text(), "0")
            self.assertEqual(fake.follow_mouse, bridge.HOVER)
            # A second enable must not save hover itself as the original.
            bridge.hover_on(self.path, self.deadline)
            self.assertEqual(self.path.read_text(), "0")
            bridge.restore_hover(self.path)

        self.run_with(fake, on)
        self.assertEqual(fake.follow_mouse, 0)
        self.assertFalse(self.path.exists())

    def test_hover_already_on_is_left_alone(self):
        fake = FakeHyprland(bridge.HOVER)
        self.run_with(fake, lambda: bridge.hover_on(self.path, self.deadline))
        self.assertFalse(any(r.startswith("eval") for r in fake.requests))
        self.run_with(fake, lambda: bridge.restore_hover(self.path))
        self.assertEqual(fake.follow_mouse, bridge.HOVER)
        self.assertFalse(self.path.exists())

    def test_failed_restore_keeps_file_for_the_watchdog(self):
        self.path.write_text("2")
        fake = FakeHyprland(bridge.HOVER, fail_set=True)
        self.run_with(fake, lambda: bridge.restore_hover(self.path))
        self.assertEqual(self.path.read_text(), "2")
        fake.fail_set = False
        self.run_with(fake, lambda: bridge.restore_hover(self.path))
        self.assertEqual(fake.follow_mouse, 2)
        self.assertFalse(self.path.exists())

    def test_nothing_saved_means_nothing_to_restore(self):
        fake = FakeHyprland(bridge.HOVER)
        self.run_with(fake, lambda: bridge.restore_hover(self.path))
        self.assertEqual(fake.requests, [])


if __name__ == "__main__":
    unittest.main()
