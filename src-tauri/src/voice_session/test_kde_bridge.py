from contextlib import redirect_stderr
import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("bridge", Path(__file__).with_name("kde_bridge.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class VoiceBridgeTests(unittest.TestCase):
    def setUp(self):
        self.target = {"id": "window-A", "app_id": "com.mitchellh.ghostty", "pid": 71}
        self.focus = dict(self.target)
        self.generation = 3
        self.copies = []
        self.keys = []
        self.events = []
        self.gate = bridge.PasteGate()
        self.hooks = {}
        self.now = 100.
        self.budgets = set()
        self.clock = patch.object(bridge.time, "monotonic", lambda: self.now)
        self.sleep = patch.object(bridge.time, "sleep", self.tick)
        self.clock.start()
        self.sleep.start()
        self.addCleanup(self.clock.stop)
        self.addCleanup(self.sleep.stop)
        self.request = {"generation": 3, "target": self.target, "text": "Привет\nмир"}

    def tick(self, elapsed):
        self.now += max(elapsed, .000001)

    def stage(self, name, deadline):
        self.assertTrue(self.gate.busy)
        self.budgets.add(id(deadline))
        deadline.remaining()
        self.events.append(name)
        hook = self.hooks.pop(name, None)
        if hook:
            hook()

    def paste(self, copy=None, probe=None):
        def target_now(deadline):
            self.stage("probe", deadline)
            return probe() if probe else self.focus

        def copy_text(text, deadline):
            self.stage("copy", deadline)
            (copy or self.copies.append)(text)

        def send(keys, deadline):
            self.keys.append(keys)
            self.stage("submit" if keys == ["28:1", "28:0"] else "paste", deadline)

        bridge.guarded_paste(self.request, lambda gen: gen == self.generation,
                             target_now, copy_text, send, self.gate,
                             lambda d: self.stage("pump", d))

    def submit(self, probe=None):
        def target_now(deadline):
            self.stage("probe", deadline)
            return probe() if probe else self.focus

        def send(keys, deadline):
            self.keys.append(keys)
            self.stage("submit", deadline)

        bridge.guarded_submit({"generation": 3, "target": self.target},
                              lambda gen: gen == self.generation, target_now, send, self.gate)

    def test_exact_ghostty_paste_never_emits_enter(self):
        self.paste()
        self.assertEqual(self.copies, ["Привет мир"])
        self.assertEqual(self.keys, [["135:1", "135:0"]])
        self.assertEqual([e for e in self.events if e not in ("pump", "probe")],
                         ["copy", "paste"])
        self.assertEqual(len(self.budgets), 1)
        self.assertFalse(self.gate.busy)

    def test_paste_emits_no_enter_even_with_legacy_auto_submit_flag(self):
        for flag in (True, False, None):
            with self.subTest(flag=flag):
                self.copies.clear()
                self.keys.clear()
                self.events.clear()
                if flag is None:
                    self.request.pop("auto_submit", None)
                else:
                    self.request["auto_submit"] = flag
                self.paste()
                self.assertEqual(self.copies, ["Привет мир"])
                self.assertEqual(self.keys, [["135:1", "135:0"]])
                self.assertNotIn(["28:1", "28:0"], self.keys)
                self.assertFalse(self.gate.busy)

    def test_submit_sends_one_enter_into_the_pasted_window(self):
        self.submit()
        self.assertEqual(self.keys, [["28:1", "28:0"]])
        self.assertEqual(self.events, ["probe", "submit"])
        self.assertEqual(self.copies, [])
        self.assertFalse(self.gate.busy)

    def test_submit_skipped_when_focus_moved_without_touching_clipboard(self):
        for key, value in (("id", "window-B"), ("pid", 72), ("app_id", "firefox")):
            self.focus = {**self.target, key: value}
            with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
                self.submit()
        self.assertEqual((self.copies, self.keys), ([], []))
        self.assertFalse(self.gate.busy)

    def test_submit_skipped_on_generation_change_before_or_during_probe(self):
        self.generation = 4
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.submit()
        self.generation = 3
        def probe():
            self.generation = 4
            return self.focus
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.submit(probe=probe)
        self.assertEqual(self.keys, [])

    def test_submit_rejected_while_paste_holds_gate_and_failed_enter_is_uncertain(self):
        self.gate.busy = True
        with self.assertRaisesRegex(bridge.BridgeError, "busy"):
            self.submit()
        self.gate.busy = False
        self.hooks["submit"] = lambda: (_ for _ in ()).throw(
            bridge.subprocess.CalledProcessError(1, "ydotool"))
        with self.assertRaisesRegex(bridge.BridgeError, "delivery_uncertain"):
            self.submit()
        self.assertEqual(self.keys, [["28:1", "28:0"]])
        self.assertFalse(self.gate.busy)

    def test_non_ghostty_target_uses_shift_insert_paste_keys(self):
        for app in ("google-chrome", "brave-browser", "claude", "org.kde.konsole", "firefox", "kitty"):
            self.request["target"] = {"id": "window-C", "app_id": app, "pid": 100}
            self.focus = dict(self.request["target"])
            self.keys.clear()
            self.paste()
            self.assertEqual(self.keys, [["42:1", "110:1", "110:0", "42:0"]])

    def test_paste_keys_returns_shift_insert_for_all_non_ghostty_apps(self):
        for app in ("google-chrome", "brave-browser", "claude", "org.kde.konsole", "firefox", "kitty", "terminal"):
            self.assertEqual(bridge.paste_keys(app), ["42:1", "110:1", "110:0", "42:0"])
        self.assertEqual(bridge.paste_keys("ghostty"), ["135:1", "135:0"])
        self.assertEqual(bridge.paste_keys("com.mitchellh.ghostty"), ["135:1", "135:0"])

    def test_copy_records_text_to_both_clipboard_and_primary_selection(self):
        commands = []
        deadline = bridge.Deadline()
        def mock_run(argv, budget, pump, data=None, duration=None):
            commands.append((argv, data))
        with patch.object(bridge, "run_command", mock_run):
            bridge.copy("Привет\nмир", deadline)
        self.assertEqual(commands, [
            (["wl-copy", "--type", "text/plain;charset=utf-8"], "Привет мир".encode()),
            (["wl-copy", "--primary", "--type", "text/plain;charset=utf-8"], "Привет мир".encode()),
        ])

    def test_every_spoken_word_is_literal(self):
        self.request["text"] = "Диктор отправляй! отправляй  дословно\nпожалуйста."
        self.paste()
        self.assertEqual(self.copies, ["Диктор отправляй! отправляй  дословно пожалуйста."])

    def test_target_mismatch_does_not_write_clipboard_or_keys(self):
        self.focus = {**self.target, "id": "window-B"}
        with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
            self.paste()
        self.assertEqual((self.copies, self.keys), ([], []))

    def test_same_app_different_pid_and_same_pid_different_uuid_are_rejected(self):
        for key, value in (("pid", 72), ("id", "window-B"), ("app_id", "other")):
            self.focus = {**self.target, key: value}
            with self.assertRaises(bridge.BridgeError):
                self.paste()
        self.assertEqual((self.copies, self.keys), ([], []))

    def test_off_before_queued_paste_has_no_side_effect(self):
        self.generation = 4
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.paste()
        self.assertEqual((self.copies, self.keys), ([], []))

    def test_off_during_clipboard_handoff_prevents_keys(self):
        def copy(text):
            self.copies.append(text)
            self.generation = 4
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.paste(copy=copy)
        self.assertEqual(self.keys, [])

    def test_focus_change_during_clipboard_handoff_prevents_keys(self):
        def copy(text):
            self.copies.append(text)
            self.focus = {**self.target, "id": "window-B"}
        with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
            self.paste(copy=copy)
        self.assertEqual(self.keys, [])

    def test_generation_changes_during_focus_probe(self):
        def probe():
            self.generation = 4
            return self.focus
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.paste(probe=probe)
        self.assertEqual((self.copies, self.keys), ([], []))

    def test_retry_after_original_window_returns(self):
        self.focus = {**self.target, "id": "window-B"}
        with self.assertRaises(bridge.BridgeError):
            self.paste()
        self.focus = dict(self.target)
        self.paste()
        self.assertEqual(self.keys, [["135:1", "135:0"]])

    def test_all_paste_sequences_release_keys_and_contain_no_enter(self):
        for app in ("ghostty", "org.kde.konsole", "firefox", "google-chrome"):
            pressed = set()
            for event in bridge.paste_keys(app):
                key, state = map(int, event.split(":"))
                self.assertNotIn(key, (28, 96))
                if state:
                    pressed.add(key)
                else:
                    pressed.remove(key)
            self.assertFalse(pressed)

    def test_empty_final_and_control_only_final_do_nothing(self):
        for text in ("", "\r\n\t\x1b", "\u2028\u2029"):
            self.request["text"] = text
            self.paste()
        self.assertEqual((self.copies, self.keys), ([], []))

    def test_all_terminal_controls_are_removed(self):
        text = "a" + "".join(chr(i) for i in range(32)) + "\x7f\u0085\u202e\u2028\u2029b"
        sanitized = bridge.sanitize(text)
        self.assertTrue(all(c == " " for c in sanitized[1:-1]))
        self.assertEqual((sanitized[0], sanitized[-1]), ("a", "b"))

    def test_off_after_paste_gap_is_terminal(self):
        for stage in ("paste", "pump"):
            with self.subTest(stage=stage):
                self.focus = dict(self.target)
                self.generation = 3
                self.keys.clear()
                self.events.clear()
                self.hooks[stage] = lambda: setattr(self, "generation", 4)
                with self.assertRaisesRegex(bridge.BridgeError, "delivery_uncertain"):
                    self.paste()
                self.assertEqual(self.keys, [["135:1", "135:0"]])

    def test_focus_change_after_paste_does_not_fail_delivered_paste(self):
        self.hooks["pump"] = lambda: setattr(self, "focus", {**self.target, "id": "window-B"})
        self.paste()
        self.assertEqual(self.keys, [["135:1", "135:0"]])

    def test_paste_subprocess_nonzero_and_timeout_are_terminal(self):
        for failure in (bridge.subprocess.CalledProcessError(1, "ydotool"),
                        bridge.subprocess.TimeoutExpired("ydotool", 1)):
            self.keys.clear()
            def fail():
                raise failure
            self.hooks["paste"] = fail
            with self.assertRaisesRegex(bridge.BridgeError, "delivery_uncertain"):
                self.paste()
            self.assertEqual(self.keys, [["135:1", "135:0"]])

    def test_nested_delivery_is_rejected_before_any_nested_query(self):
        def nested():
            before = len(self.events)
            with self.assertRaisesRegex(bridge.BridgeError, "busy"):
                self.paste()
            self.assertEqual(before, len(self.events))
        self.hooks["probe"] = nested
        self.paste()
        self.assertEqual(self.keys, [["135:1", "135:0"]])

class FakeTmux:
    """run_command stand-in: tmux with one chat-like pane per id, plus a record
    of every argv (wl-copy/ydotool included) the helper tried to run."""
    RULE = "─" * 30

    def __init__(self, clients):
        self.clients = clients
        self.calls = []
        self.history = []
        self.input = ""
        self.extra_rows = []
        self.buffer = None
        self.pasted = []
        self.sent = []
        self.render_paste = True
        self.on_enter = "submit"
        self.hooks = {}

    def screen(self):
        rows = ["> " + line for line in self.history]
        rows += ["", self.RULE, "> " + self.input, *self.extra_rows, self.RULE, "  ? for shortcuts"]
        return "\n".join(rows + [""] * 6) + "\n"

    def __call__(self, argv, deadline, pump, data=None, duration=None, capture=False):
        deadline.remaining()
        self.calls.append(list(argv))
        if argv[0] != "tmux":
            return None
        op = argv[1]
        hook = self.hooks.pop((op, sum(call[1:2] == [op] for call in self.calls)), None)
        if hook:
            hook()
        if op == "list-clients":
            assert capture
            return "".join(f"{flags}\t{pane}\t{pid}\n" for flags, pane, pid in self.clients).encode()
        if op == "capture-pane":
            assert capture
            return self.screen().encode()
        if op == "load-buffer":
            assert argv[2:] == ["-b", bridge.TMUX_BUFFER, "-"]
            self.buffer = data.decode()
        elif op == "paste-buffer":
            assert argv[2:6] == ["-p", "-d", "-b", bridge.TMUX_BUFFER]
            self.pasted.append((argv[argv.index("-t") + 1], self.buffer))
            if self.render_paste:
                self.input += self.buffer
            self.buffer = None
        elif op == "send-keys":
            self.sent.append((argv[argv.index("-t") + 1], argv[-1]))
            if self.on_enter == "submit":
                self.history.append(self.input)
                self.input = ""
            elif self.on_enter == "newline":
                self.extra_rows.append("  ")
        return None

    def ops(self):
        return [call[1] if call[0] == "tmux" else call[0] for call in self.calls]


class TmuxDeliveryTests(unittest.TestCase):
    tick = VoiceBridgeTests.tick
    stage = VoiceBridgeTests.stage

    def setUp(self):
        VoiceBridgeTests.setUp(self)
        self.target["pid"] = 3843  # the Ghostty process KWin reports
        self.focus = dict(self.target)
        self.tmux = FakeTmux([("attached,focused,UTF-8", "%3", 500), ("attached,UTF-8", "%1", 600)])
        self.route = bridge.TmuxRoute(lambda d: self.stage("pump", d))
        self.stderr = io.StringIO()
        self.ancestry = {500: True, 600: True}
        for patcher in (patch.object(bridge, "run_command", self.tmux),
                        patch.object(bridge.shutil, "which", lambda name: "/usr/bin/" + name),
                        patch.object(bridge, "descends", lambda pid, ancestor: self.ancestry.get(pid, False)
                                     and ancestor == 3843),
                        redirect_stderr(self.stderr)):
            patcher.__enter__()
            self.addCleanup(patcher.__exit__, None, None, None)

    def paste(self, copy=None, probe=None):
        def target_now(deadline):
            self.stage("probe", deadline)
            return probe() if probe else self.focus

        def copy_text(text, deadline):
            self.stage("copy", deadline)
            bridge.copy(text, deadline)  # real wl-copy argv reaches the fake
            self.copies.append(text)

        def send(keys, deadline):
            self.keys.append(keys)

        return bridge.guarded_paste(self.request, lambda gen: gen == self.generation, target_now,
                                    copy_text, send, self.gate, lambda d: self.stage("pump", d),
                                    route=self.route)

    def submit(self, probe=None):
        def target_now(deadline):
            self.stage("probe", deadline)
            return probe() if probe else self.focus

        def send(keys, deadline):
            self.keys.append(keys)

        return bridge.guarded_submit({"generation": 3, "target": self.target},
                                     lambda gen: gen == self.generation, target_now, send, self.gate,
                                     route=self.route)

    def test_one_focused_client_pastes_into_its_pane_without_clipboard_or_keys(self):
        result = self.paste()
        self.assertEqual(self.tmux.pasted, [("%3", "Привет мир")])
        self.assertEqual(self.tmux.ops(), ["list-clients", "list-clients", "capture-pane", "load-buffer",
                                           "paste-buffer", "capture-pane"])
        self.assertEqual((self.copies, self.keys, self.tmux.sent), ([], [], []))
        self.assertEqual(result, {"path": "tmux", "pane": "%3", "confirmed": True, "visible_ms": 0})
        self.assertEqual(self.route.last["pane"], "%3")
        self.assertIn("voice delivery: op=paste path=tmux pane=%3 confirmed=True", self.stderr.getvalue())
        self.assertFalse(self.gate.busy)

    def test_zero_or_several_focused_clients_or_no_tmux_fall_back_to_clipboard_keys(self):
        cases = {
            "focused_clients:0": dict(clients=[("attached,UTF-8", "%3", 500)]),
            "focused_clients:2": dict(clients=[("attached,focused", "%3", 500), ("focused", "%1", 600)]),
            "no_tmux": dict(which=None),
            "client_outside_target": dict(outside=True),
        }
        for reason, case in cases.items():
            with self.subTest(reason=reason):
                self.copies.clear()
                self.keys.clear()
                self.tmux.calls.clear()
                self.tmux.clients = case.get("clients", [("attached,focused", "%3", 500)])
                self.ancestry[500] = not case.get("outside")
                with patch.object(bridge.shutil, "which",
                                  lambda name: None if "which" in case else "/usr/bin/" + name):
                    result = self.paste()
                self.assertEqual(result, {"path": "ydotool"})
                self.assertEqual(self.copies, ["Привет мир"])
                self.assertEqual(self.keys, [["135:1", "135:0"]])
                self.assertNotIn("paste-buffer", self.tmux.ops())
                self.assertNotIn("load-buffer", self.tmux.ops())
                self.assertIn("wl-copy", self.tmux.ops())
                self.assertIn(f"path=ydotool reason={reason}", self.stderr.getvalue())
                self.assertEqual(self.route.last, {"generation": 3, "pane": None})

    def test_kwin_mismatch_touches_neither_tmux_nor_clipboard(self):
        self.focus = {**self.target, "id": "window-B"}
        with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
            self.paste()
        self.assertEqual((self.tmux.calls, self.copies, self.keys), ([], [], []))

    def test_pane_switch_between_resolves_pastes_nothing(self):
        self.tmux.hooks[("list-clients", 2)] = lambda: setattr(
            self.tmux, "clients", [("attached,focused", "%4", 500)])
        with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
            self.paste()
        self.assertEqual(self.tmux.ops(), ["list-clients", "list-clients"])
        self.assertEqual((self.copies, self.keys, self.route.last), ([], [], None))

    def test_submit_sends_enter_only_to_remembered_pane_and_confirms_it_cleared(self):
        self.paste()
        self.tmux.calls.clear()
        result = self.submit()
        self.assertEqual(self.tmux.sent, [("%3", "Enter")])
        self.assertEqual(self.tmux.history, ["Привет мир"])
        self.assertEqual(self.keys, [])
        self.assertEqual(result, {"path": "tmux", "pane": "%3", "confirmed": True, "cleared_ms": 0})
        self.assertEqual(self.tmux.ops(), ["list-clients", "capture-pane", "send-keys", "capture-pane"])
        self.assertIn("op=submit path=tmux pane=%3 confirmed=True cleared_ms=0", self.stderr.getvalue())

    def test_pane_change_before_submit_is_target_changed_without_keys(self):
        self.paste()
        for clients in ([("attached,focused", "%4", 500)],
                        [("attached,focused", "%3", 500), ("focused", "%4", 600)], []):
            with self.subTest(clients=clients):
                self.tmux.clients = clients
                with self.assertRaisesRegex(bridge.BridgeError, "target_changed"):
                    self.submit()
                self.assertEqual((self.tmux.sent, self.keys), ([], []))
        self.assertIn("expected=%3", self.stderr.getvalue())

    def test_generation_change_before_or_during_pane_lookup_cancels_submit(self):
        self.paste()
        self.generation = 4
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.submit()
        self.generation = 3
        self.tmux.hooks[("list-clients", 3)] = lambda: setattr(self, "generation", 4)
        with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
            self.submit()
        self.assertEqual((self.tmux.sent, self.keys), ([], []))
        self.assertFalse(self.gate.busy)

    def test_submit_after_ydotool_paste_or_other_generation_uses_ydotool_enter(self):
        self.tmux.clients = []
        self.paste()
        self.assertEqual(self.submit(), {"path": "ydotool"})
        self.assertEqual(self.keys, [["135:1", "135:0"], ["28:1", "28:0"]])
        self.route.last = {"generation": 2, "pane": "%3", "candidates": [], "needle": "x"}
        self.keys.clear()
        self.submit()
        self.assertEqual(self.keys, [["28:1", "28:0"]])
        self.assertNotIn("send-keys", self.tmux.ops())

    def test_swallowed_or_newline_enter_is_logged_once_never_retried(self):
        for mode in ("ignore", "newline"):
            with self.subTest(mode=mode):
                self.tmux.__init__([("attached,focused", "%3", 500)])
                self.tmux.on_enter = mode
                self.paste()
                result = self.submit()
                self.assertEqual(self.tmux.sent, [("%3", "Enter")])
                self.assertFalse(result["confirmed"])
                self.assertLessEqual(result["cleared_ms"], 1600)
                log = self.stderr.getvalue()
                self.assertIn("submit unconfirmed pane=%3", log)
                self.assertIn("Привет мир", log.split("submit unconfirmed")[-1])

    def test_invisible_paste_is_reported_and_blocks_enter(self):
        self.tmux.render_paste = False
        result = self.paste()
        self.assertEqual(result["confirmed"], False)
        self.assertIn("paste unconfirmed pane=%3", self.stderr.getvalue())
        with self.assertRaisesRegex(bridge.BridgeError, "paste_unconfirmed"):
            self.submit()
        self.assertEqual((self.tmux.sent, self.keys), ([], []))

    def test_late_render_and_collapsed_paste_marker_allow_enter(self):
        self.tmux.render_paste = False
        self.paste()
        self.tmux.input = "[Pasted text #1 +12 lines]"
        self.submit()
        self.assertEqual(self.tmux.sent, [("%3", "Enter")])

    def test_screen_depth_ignores_wrapping_and_borders(self):
        screen = "> old Привет мир\n│ > Привет │\n│ мир      │\n╰──────────╯\n\n\n"
        self.assertEqual(bridge.depth(screen, bridge.screen_key("Привет мир")), 3)
        self.assertIsNone(bridge.depth(screen, "absent"))
        self.assertEqual(bridge.needles(" \n"), [bridge.PASTED_MARKER])
        self.assertEqual(bridge.needles("a" * 40)[0], "a" * 24)


class ProcessBudgetTests(unittest.TestCase):
    def test_input_slice_also_bounds_nested_event_dispatch_and_reaps_child(self):
        deadline = bridge.Deadline()
        events = []
        class Process:
            def communicate(self, input, timeout):
                raise bridge.subprocess.TimeoutExpired("mock", timeout)
            def poll(self):
                return None
            def kill(self):
                events.append("kill")
            def wait(self, timeout):
                events.append("reaped")
        def pump(budget):
            self.assertIs(budget, deadline)
            self.assertLessEqual(budget.remaining(), .15)
            raise bridge.BridgeError("cancelled")
        with patch.object(bridge.subprocess, "Popen", return_value=Process()):
            with self.assertRaisesRegex(bridge.BridgeError, "cancelled"):
                bridge.run_command(["mock"], deadline, pump, duration=.15)
        self.assertIsNone(deadline.step_end)
        self.assertEqual(events, ["kill", "reaped"])

    def test_partial_input_failure_only_releases_never_replays_keydown(self):
        for keys in (["28:1", "28:0"], bridge.paste_keys("konsole")):
            for error in (bridge.subprocess.CalledProcessError(1, "ydotool"),
                          bridge.subprocess.TimeoutExpired("ydotool", 1)):
                calls = []
                deadline = bridge.Deadline()
                def run(argv, budget, pump, duration):
                    self.assertIs(budget, deadline)
                    self.assertEqual(duration, .15)
                    calls.append(argv[4:])
                    if len(calls) == 1:
                        raise error
                with patch.object(bridge, "run_command", run):
                    with self.assertRaises(type(error)):
                        bridge.send_keys(keys, deadline, lambda _d: None)
                self.assertEqual(calls, [keys, [k.split(":")[0] + ":0" for k in keys if k.endswith(":1")]])

    def test_subprocess_wait_pumps_control_callbacks_and_shares_deadline(self):
        seen = []
        class Process:
            returncode = 0
            attempts = 0
            def communicate(self, input, timeout):
                seen.append((input, timeout))
                self.attempts += 1
                if self.attempts == 1:
                    raise bridge.subprocess.TimeoutExpired("mock", timeout)
            def poll(self):
                return self.returncode
        deadline = bridge.Deadline()
        pumps = []
        with patch.object(bridge.subprocess, "Popen", return_value=Process()):
            bridge.run_command(["mock"], deadline, pumps.append, b"literal")
        self.assertEqual(pumps, [deadline])
        self.assertEqual([data for data, _ in seen], [b"literal", None])
        self.assertTrue(all(0 < timeout <= .02 for _, timeout in seen))


class FocusReplyTests(unittest.TestCase):
    def test_old_observer_and_query_replies_cannot_confirm_fresh_query(self):
        reports = bridge.FocusReports()
        old = reports.begin()
        reports.end()
        nonce = reports.begin()
        a = {"id": "A", "app_id": "ghostty", "pid": 1}
        b = {"id": "B", "app_id": "ghostty", "pid": 2}
        reports.report(a)
        reports.report({**a, "nonce": old})
        self.assertIsNone(reports.reply)
        reports.report({**b, "nonce": nonce})
        self.assertTrue(bridge.same_target(reports.reply, b))
        self.assertFalse(bridge.same_target(reports.reply, a))
        reports.end()
        reports.report({**a, "nonce": nonce})
        self.assertIsNone(reports.reply)

    def test_nested_query_cannot_supersede_pending_nonce(self):
        reports = bridge.FocusReports()
        nonce = reports.begin()
        with self.assertRaisesRegex(bridge.BridgeError, "busy"):
            reports.begin()
        self.assertEqual(reports.pending, nonce)


if __name__ == "__main__":
    unittest.main()
